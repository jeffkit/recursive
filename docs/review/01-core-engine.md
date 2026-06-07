# Review: 核心引擎模块

**Date**: 2026-06-06
**Reviewer**: Architecture Critic (AI)
**Scope**: agent/, kernel.rs, runtime.rs, runtime_goal.rs, run_core.rs, error.rs, event.rs, message.rs, config.rs, config_file.rs

---

## Executive Summary

整体架构分层清晰（Kernel 无状态 / Runtime 有状态），Invariant #7（FinishReason 是数据而非异常）执行到位，错误类型设计完善，无 `anyhow` 滥用。主要风险集中在三处：`run_inner` 中一个隐藏的 O(n²) 批量重排序 bug、`run_goal_loop` 对同一 `RwLock` 的连续重复加锁（在 Budget 超出路径）、以及 `AgentEvent::MessageAppendedWithAudit` 的引入打破了单一事件的向后兼容约定。

---

## 严重问题 (Critical)

### C-1: execute_tool_calls 批量排序逻辑存在结果乱序 bug

**文件**: `src/run_core.rs`, 第 426–456 行

批量并行执行后，结果由 `join_next()` 以**完成顺序**填入 `batch_results`（非提交顺序）。随后在第 427 行用 `batch.iter()` 按原始顺序遍历，通过 `find` 在 `batch_results` 中查找匹配的 `id`——这是 O(n²) 的查找，且更关键的是：`find` 返回的是 `batch_results` 中该条目的引用，但 `audit` 字段随后被 `clone` 插入 `tool_audits`，而对应的 `PostToolCall` hook 中 `result` / `duration_ms` 也取自 `batch_results`。

问题在于：当 `batch_results` 中某个 task 因 panic 缺失时（第 429 行的 `None` 分支），代码向 `results` 推入一个占位错误；但此时外层 `for pc in &batch` 循环仍在继续，循环中其余的 `pc` 对应的 `audit` 全部丢失（因为 `find` 找到的是第一个匹配项，不是该 `pc` 对应的那个）。当批次中存在多个同名工具调用（如两个并行 Read）时，`find` 会始终匹配第一个，导致后续调用的 audit 全部附上第一个调用的 audit 数据。

**为什么严重**: audit 数据错位会导致持久化层（`MessageAppendedWithAudit`）将错误的 audit 绑定到错误的 tool_call_id，这是静默数据损坏，不会触发任何错误。

**建议**: 改用 `HashMap<String, BatchRow>` 按 `id` 索引，消除线性查找和乱序风险：

```rust
let batch_map: HashMap<String, BatchRow> = batch_results
    .into_iter()
    .map(|row| (row.0.clone(), row))
    .collect();
for pc in &batch {
    match batch_map.get(&pc.id) { ... }
}
```

---

### C-2: run_goal_loop Budget 超出路径下对 RwLock 的双重写锁

**文件**: `src/runtime.rs`, 第 844–856 行

```rust
// turn counter increment (line 830)
let turns = {
    let mut guard = self.goal_state.write().ok();
    // ... mutate gs.turns
};

// Budget exceeded check (line 844)
if turns >= max_turns {
    if let Ok(mut g) = self.goal_state.write() {  // 第二次写锁
        ...
        *g = None;
    }
    self.event_sink.emit(AgentEvent::GoalCleared).await;  // await 在锁外，OK
    break;
}
```

虽然两次 `write()` 不在同一 scope（Mutex/RwLock 不可重入，此处用的是 `std::sync::RwLock`），但这里的问题是：`goal_state` 在第 829 行已经被写锁修改了一次，锁已释放；第 845 行再次获取写锁。这本身不会死锁，但在"转数恰好到达 max_turns"的边界处逻辑有歧义——两次加锁之间如果有外部线程（HTTP handler）通过 `clear_goal()` 清除了 goal，第二次加锁会看到 `*g = None`，然后再次赋 `None` 并 emit `GoalCleared`，造成双重 `GoalCleared` 事件。

`set_goal` 和 `clear_goal` 是 `&self` 方法（非 `&mut self`），说明运行时允许外部并发访问 goal_state，这个 TOCTOU 是实际存在的。

**建议**: 将"increment turns + check budget + clear if needed"合并到一次写锁中，消除 TOCTOU：

```rust
let (turns, budget_exceeded) = {
    let mut guard = self.goal_state.write().ok();
    if let Some(ref mut guard) = guard {
        if let Some(ref mut gs) = **guard {
            gs.turns += 1;
            let t = gs.turns;
            if t >= max_turns {
                **guard = None;
                (t, true)
            } else {
                (t, false)
            }
        } else { break; }
    } else { break; }
};
```

---

## 中等问题 (Major)

### M-1: MessageAppendedWithAudit 破坏 non_exhaustive 约定，变相引入分叉事件类型

**文件**: `src/event.rs`, 第 136–143 行；`src/runtime.rs`, 第 514–527 行

`AgentEvent` 标注了 `#[non_exhaustive]`，目的是允许添加新变体而不破坏下游的 `match`。但 `MessageAppendedWithAudit` 实质上是 `MessageAppended` 的一个特化变体——两者携带同一个 `Message`，只是后者多了 `AuditMeta`。

注释（第 139–142 行）本身承认这个设计的唯一动机是"keep the common path zero-cost"——但 `MessageAppended` 的 `usage: Option<UsageMeta>` 已经是可选字段了，同样的模式完全可以用 `audit: Option<AuditMeta>` 实现，代价是每个 MessageAppended 事件多一个 None 字段（8 bytes）。

更严重的是：`emit_turn_messages` 中（第 514 行）需要用 `if msg.role == Role::Tool && tcid.is_some() && audit_map.contains(tcid)` 分叉来决定发哪种事件，这个分叉逻辑不在 Kernel 层，而是散落在 Runtime 层，未来增加新 audit 来源时容易遗漏。

**建议**: 将 `MessageAppendedWithAudit` 合并回 `MessageAppended`，增加 `audit: Option<AuditMeta>` 字段。消除分叉，保持事件类型的单一性。

---

### M-2: intra-turn compaction 后的 new_messages 提取逻辑依赖脆弱的字符串启发式

**文件**: `src/kernel.rs`, 第 299–309 行

```rust
if !inner.messages.is_empty()
    && inner.messages[0].role == crate::message::Role::System
    && inner.messages[0].content.contains("[compacted:")
{
    new_messages.insert(0, inner.messages[0].clone());
}
```

这段逻辑通过检测系统消息内容是否以 `"[compacted:"` 开头来判断是否发生了 intra-turn compaction。这是一个**字符串启发式**，不是结构化的状态传递。`RunInnerOutcome` 中没有 `bool compacted` 字段，判断只能依赖内容字符串。

问题：
1. 如果用户的系统 prompt 本身以 `"[compacted:"` 开头（不太可能但不排除），会把原始系统消息当成 compaction summary 误插入 `new_messages`；
2. 如果 `Compactor::apply_to_transcript` 的 summary 格式在未来变化，这里会静默失效——不报错，只是 Runtime 的 transcript 少了一条 compaction summary 消息；
3. `input_len` 的计算（第 270 行）和 compaction 后的 slice 提取（第 299 行）是两段逻辑，代码注释虽清晰但紧耦合。

**建议**: 在 `RunInnerOutcome` 增加 `compaction_summary: Option<Message>` 字段，由 `RunCore::maybe_compact` 填充，消除字符串启发式。

---

### M-3: run_goal_loop 在 Runtime 层实现，违反了层次职责

**文件**: `src/runtime.rs`, 第 798–897 行

`run_goal_loop` 包含完整的 judge 调用逻辑（GoalEvaluator）、turn 计数管理、多次 RwLock 操作、以及自动 prompt 生成（第 888–893 行的字符串拼接），这些都直接写在 `AgentRuntime` 的 impl 中。

`runtime_goal.rs` 存在的目的就是"让 `runtime.rs` 只承载 AgentRuntime impl + 其 loop 方法"（注释第 6 行），但实际上 `run_goal_loop` 的全部 body（90 行）都在 `runtime.rs` 里，`runtime_goal.rs` 只提供了 data types 和 `GoalEvaluator`。这与注释承诺不符——loop body 本应在 `runtime_goal.rs` 或独立模块中。

此外，`run_goal_loop`、`run_loop`、`run_event_loop` 三个方法是三种不同的"多轮驱动"模式，它们没有共享的抽象，未来继续增加新的 loop 驱动方式会无限膨胀 `runtime.rs`。

**建议**: 将 loop 逻辑（包括 goal 评估循环）提取为独立的 `GoalLoop` 结构，接受 `&mut AgentRuntime` 作为参数运行，保持 `AgentRuntime` 本身只暴露单轮 `run()` 接口。

---

### M-4: Config 结构体有 26 个字段，所有字段 pub，没有 builder

**文件**: `src/config.rs`, 第 17–73 行

`Config` 是一个 26 字段的扁平结构体，所有字段 `pub`，没有 builder 或 setter。测试代码中（第 592–624 行）每次构造 `Config` 都要手写全部字段，这是测试质量问题——每次新增字段时，所有手写构造的测试都会编译失败（没有 `Default` 推导）。

更重要的是：`Config` 与 `AgentKernel` 之间不存在直接的 `From<Config>` 转换，意味着从 `Config::from_env()` 到 `AgentKernelBuilder` 之间的字段映射散落在调用端（main.rs 或其他入口），容易在新增字段时漏映射。

**建议**: 为 `Config` 派生 `Default`，或实现 `AgentKernelBuilder::from_config(config: &Config)` 减少字段遗漏风险。

---

### M-5: execute_tool_calls 中 plan_mode 检查是分散的逻辑分支

**文件**: `src/run_core.rs`, 第 282–321 行

在 `execute_tool_calls` 的工具调用循环中，有两段独立的 `if` 块：

1. 第 282 行：检测"正在 plan mode 但调用了写工具"
2. 第 304 行：检测"没在 plan mode 但调用了需要 plan mode 的工具"

这两段逻辑与 plan mode 相关的特判（`call.name != "exit_plan_mode"`、`call.name != "enter_plan_mode"`）以字符串硬编码写在 run_core 里，是对工具注册表以外的隐性知识——当 plan mode 工具名变化时，这里不会报编译错误。

Invariant #1 要求"不在 agent.rs::Agent::run 里分支"，但现在分支转移到了 `run_core.rs::execute_tool_calls`——本质上一样，只是位置不同。正确的做法是工具元数据携带 plan mode 语义，由注册表 dispatch 层统一处理。

**建议**: 将 plan_mode 阻断逻辑收进 `ToolRegistry::invoke_with_audit`，基于工具元数据（`is_readonly`/`is_plan_mode`）判断，不在 run_core 里硬编码工具名。

---

## 轻微问题 (Minor)

### N-1: kernel.rs build() 中 unwrap_or_else 调用 current_dir，等于在测试路径引入了"当前目录"的隐式依赖

**文件**: `src/kernel.rs`, 第 477 行

```rust
Arc::new(crate::storage::local::LocalStorageBackend::new(
    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
))
```

`current_dir()` 的返回值依赖进程的 CWD。在测试环境中，CWD 是 cargo 的项目根。这不是 unwrap 违规（有 fallback），但"LocalStorageBackend 默认 rooted at CWD"是隐式行为，没有在文档中明确。如果测试忘记 mock storage，会意外写入项目根目录。

**建议**: 文档中增加明确警告，或在 test cfg 下 default 到 tempdir。

---

### N-2: PlanningMode 枚举有且只有一个变体，是个僵尸类型

**文件**: `src/agent/types.rs`, 第 29–34 行

```rust
pub enum PlanningMode {
    #[default]
    Immediate,
}
```

注释说"Currently only `Immediate` is supported"。这个 enum 从未被 match，没有被任何业务逻辑读取（`AgentKernel` 和 `RunCore` 中均无此字段）。实际的 plan mode 控制依赖 `Arc<AtomicBool> exploring_plan_mode`，与 `PlanningMode` enum 没有连接。

**建议**: 删除或用 `#[allow(dead_code)]` 加注释说明保留原因，避免混淆。

---

### N-3: stuck_window_env_override 测试不通过 Config::from_env 验证实际行为

**文件**: `src/config.rs`, 第 1116–1124 行

```rust
#[test]
fn stuck_window_env_override() {
    std::env::set_var("RECURSIVE_STUCK_WINDOW", "5");
    let window = std::env::var("RECURSIVE_STUCK_WINDOW")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(10);
    assert_eq!(window, 5);
    std::env::remove_var("RECURSIVE_STUCK_WINDOW");
}
```

这个测试只验证了"能从 env var 读取字符串再 parse 为数字"——它没有调用 `Config::from_env()`，因此不能证明 `stuck_window` 实际上被正确写入 `Config` 结构体。同样的模式对 `stuck_error_rate_env_override` 测试也适用。若 `Config::from_env` 中的映射行有 bug，这两个测试不会捕获它。

**建议**: 按照 `shell_timeout_default_and_env_override` 的模式，在测试中实际调用 `Config::from_env()` 并断言 `config.stuck_window == 5`。

---

### N-4: emit_turn_messages 中对 new_messages 的双重克隆

**文件**: `src/runtime.rs`, 第 502–554 行

```rust
let new_messages = outcome.new_messages.clone();  // 第 503 行
// ...
self.transcript.extend(new_messages.iter().cloned());  // 第 512 行
for (idx, msg) in new_messages.iter().enumerate() {   // 第 513 行
    // ...
    let event = ... AgentEvent::MessageAppended { message: msg.clone(), ... }  // 每条消息再 clone
```

`outcome.new_messages` 先被整体 clone 到 `new_messages`，随后 `extend` 时又对每条 `.cloned()`，最后建 `AgentEvent` 时每条消息又 `.clone()` 一次。一条消息最多被 clone 3 次，对于包含大量 `tool_calls` 或长 `content` 的消息（如长 shell 输出）这是可观的开销。

这是性能问题，不是正确性问题，但在高频调用路径上（每个 turn 的每条消息）值得优化。

**建议**: 将 `outcome.new_messages` consume 掉而非 clone，由 `into_iter()` 遍历，先 extend transcript，再在同一遍历中建 event：

```rust
for msg in outcome.new_messages {
    self.transcript.push(msg.clone());  // 仅一次 clone
    self.event_sink.emit(build_event(msg, ...)).await;
}
```

---

### N-5: load_layered_permissions 中 HOME 解析与 config_file_path 不一致

**文件**: `src/config_file.rs`, 第 186 行 vs 第 22 行

`config_file_path()` 优先读取 `RECURSIVE_HOME` env var，但 `load_layered_permissions()` 直接读取 `HOME`（第 186 行），没有遵守相同的优先级链。在测试中，`PinnedHome` 通过 pin `HOME` 来工作，`PinnedRecursiveHome` 通过 pin `RECURSIVE_HOME` 来工作——如果两者使用不同的函数，会导致测试间行为不一致。

**建议**: `load_layered_permissions` 中也通过 `config_file_path()` 的同一逻辑（先 `RECURSIVE_HOME` 后 `HOME`）定位用户配置文件路径，保持一致性。

---

## 正面评价

1. **Invariant #7 执行彻底**: `FinishReason` 作为数据在整个代码路径流转，没有任何地方把 finish reason 转成 `Err()`，`run_inner` 始终返回 `Ok(RunInnerOutcome)`，包括 BudgetExceeded / Stuck / Cancelled，这是一流的设计执行力。

2. **错误类型设计完备**: `error.rs` 的 14 个变体覆盖了所有实际失败模式，`is_retryable` / `is_transient` 语义清晰，`#[from]` 转换只限于真正透明的系统错误（io, reqwest, serde_json），无 `anyhow` 滥用。

3. **AgentKernel::run 真正无状态**: `AgentKernel::run` 接收 `TurnContext`，构造 `RunCore`，调用 `run_inner()`，返回 `TurnOutcome`——7 行主逻辑（第 267–320 行），无分支。Runtime 完全负责跨轮状态，分层干净。

4. **并行工具执行的 Invariant #8 保护**: `execute_tool_calls` 在 DENIAL_LIMIT 路径（第 716–744 行）先 flush 所有 pending tool results 再 return，工具调用与工具结果的配对不破坏，这是对协议约束的正确执行。

5. **LLM 重试策略集中**: `call_llm_with_retry` 统一处理 `RateLimited` + `Timeout` 的指数退避，`retry_after_ms` hint 被优先使用，逻辑在一处，没有散落在各 provider 中。

6. **测试密度合理**: 核心路径（AgentRuntime::run, goal loop, queue drain, compaction, checkpoint）均有集成级测试，MockProvider 的使用方式正确（预先准备 completion 序列）。

---

## 建议优先级

1. **立即修复 (C-1)**: `execute_tool_calls` 中批量并行结果按 `id` 建 HashMap 索引，消除 O(n²) 查找和 audit 错位风险。这是静默数据损坏，优先级最高。

2. **短期修复 (C-2)**: 将 `run_goal_loop` 中"increment turns + budget check + clear"合并到单次写锁，消除 TOCTOU。尤其在 HTTP API 并发场景下有实际影响。

3. **中期重构 (M-2)**: `RunInnerOutcome` 增加 `compaction_summary: Option<Message>`，替换 `kernel.rs:299` 的字符串启发式检测。

4. **中期重构 (M-1)**: 合并 `MessageAppendedWithAudit` 到 `MessageAppended`，加 `audit: Option<AuditMeta>` 字段，消除 Runtime 层的事件类型分叉逻辑。

5. **清理 (N-2, N-3)**: 删除 `PlanningMode` 僵尸 enum；修复 `stuck_window_env_override` 测试使其实际通过 `Config::from_env` 验证。

6. **架构守护 (M-3)**: 将 goal loop body 从 `runtime.rs` 迁移到 `runtime_goal.rs` 或独立 `GoalLoop` 结构，避免三种不同 loop 驱动继续膨胀 runtime.rs。
