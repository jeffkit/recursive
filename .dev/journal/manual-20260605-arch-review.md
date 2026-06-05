# 架构审查报告：Recursive 项目

**日期**：2026-06-05  
**审查者**：资深架构师视角（AI 辅助）  
**审查范围**：`src/` 全部约 68,000 行 Rust 代码  
**说明**：不依赖文档，直接以代码事实为依据

---

## 目录

1. [严重问题（会导致 bug 或安全风险）](#一严重问题)
2. [架构问题（影响可维护性和扩展性）](#二架构问题)
3. [代码质量问题](#三代码质量问题)
4. [次要问题](#四次要问题)
5. [深入分析：Session 持久化](#五深入分析session-持久化)
6. [深入分析：HTTP Server 层](#六深入分析http-server-层)
7. [深入分析：MCP 协议层](#七深入分析mcp-协议层)
8. [深入分析：Permissions 自动分类器](#八深入分析permissions-自动分类器)
9. [深入分析：Hooks 系统](#九深入分析hooks-系统)
10. [自我迭代 Dashboard 可观测性分析](#十自我迭代-dashboard-可观测性分析)
11. [深入分析：Compaction / Checkpoint / Shell 工具 / main.rs](#十一深入分析compaction--checkpoint--shell-工具--mainrs)
12. [整体评分与修复优先级](#十二整体评分与修复优先级)

---

## 一、严重问题

### 1.1 双重重试逻辑，实际重试次数翻倍

**位置**：`src/run_core.rs:116-180` 和 `src/llm/mod.rs:42-78`

`RunCore::call_llm_with_retry` 在 agent 循环层做了最多 3 次重试（`LLM_MAX_RETRIES = 3`）；而每个 LLM provider（`AnthropicProvider`、`OpenAiProvider`）内部还有独立的 `RetryPolicy`（默认 `max_retries = 2`）。两套重试完全独立：一次限速错误会先在 provider 内重试 2 次，失败后再被 `RunCore` 重试 3 次，实际最多触发 **6 次 HTTP 请求**，退避时间也不一致。

**建议**：删掉 `RunCore` 层的重试逻辑，统一交给 provider 的 `RetryPolicy`，只在一处控制重试语义。

---

### 1.2 Stuck 检测逻辑有缺陷

**位置**：`src/run_core.rs:759-797`

```rust
if is_error {
    if last_call_key == Some(call_key.clone()) {
        consecutive_errors += 1;
    } else {
        consecutive_errors = 1;  // 切换到不同工具后重置为1
    }
} else {
    consecutive_errors = 0;
}
```

`consecutive_errors` 只统计**完全相同**（名称 + 参数）的连续错误调用。如果 agent 在 `TOOL_A(fail)` → `TOOL_B(fail)` → `TOOL_A(fail)` → `TOOL_B(fail)` 之间循环，`consecutive_errors` 永远不会达到阈值 3，agent 会消耗完整个步骤预算。

**建议**：使用一个时间窗口内的全局错误率追踪，例如：在最近 N 步内错误率超过 80% 则触发 Stuck。

---

### 1.3 `blake3_canonical_json` 命名与实现不符

**位置**：`src/tools/mod.rs:137-141`

```rust
fn blake3_canonical_json(v: &Value) -> String {
    let canonical = v.to_string();  // 这不是 canonical JSON
    let hash = blake3::hash(canonical.as_bytes());
    ...
}
```

`serde_json::Value::to_string()` 的 Object key 顺序依赖于 `Value` 的内部构造方式（`serde_json::Map` 默认是随机哈希顺序）。两个逻辑等价的 JSON 对象因字段插入顺序不同，会产生**不同的哈希值**，导致 `args_hash` 用于"检测参数漂移"的功能不可靠。

**建议**：使用 BTreeMap 强制键排序后再序列化，或引入 `json-canon` crate。

---

### 1.4 `execute_kernel_turn` 永远传 `permission_hook: None`

**位置**：`src/runtime.rs:462-471`

```rust
let ctx = TurnContext {
    ...
    permission_hook: None,  // 硬编码 None！
    ...
};
```

`AgentRuntime` 通过 `set_permission_hook` 设置的 hook 走的是 `ToolRegistry` 内部路径。这里传 `None` 意味着 `RunCore` 中的 `permission_hook` 字段从 `AgentRuntime` 路径来看是**死代码**，但在 HTTP handler 和子 agent 路径中可能有不同行为，产生不一致的权限语义。

**建议**：消灭 `RunCore` 层的 `permission_hook` 字段，统一由 `ToolRegistry` 在 `invoke_with_audit` 时执行权限检查。

---

## 二、架构问题

### 2.1 `App` 是 30+ 字段的 God Object

**位置**：`src/tui/app/mod.rs:45-165`

`App` 结构体混合了：输入状态、显示状态、网络连接状态、权限状态、计划模式状态、TODO 列表、目标状态、自动补全状态、历史搜索状态、主题等完全不同职责的字段。虽然实现已拆分到 4 个子模块，但每个方法仍能访问所有字段，没有任何边界约束。

**建议**：提取 `InputState`、`ViewState`、`SessionState`、`PlanState` 等独立结构体，`App` 只持有这些的组合。

---

### 2.2 `commands.rs` 2558 行，键盘处理缺乏状态机设计

**位置**：`src/tui/app/commands.rs`

键盘处理、模态对话框逻辑、@文件补全、历史搜索、斜杠命令执行全部在一个文件里。`handle_key` 是一条数百行的 `if-else` 链，而不是状态机。增加新快捷键必须在链的正确位置插入，极易引入优先级冲突。

**建议**：引入 `KeyHandler` trait，每个输入模式（Prompt、CommandInteract、AtFile 等）对应一个实现，通过 `InputMode` 分发。

---

### 2.3 两套并行的 Compaction 实现

**位置**：`src/run_core.rs:221-256`（intra-turn）和 `src/runtime.rs:395-439`（cross-turn）

两处代码结构几乎一致：检查阈值 → PreCompact hook → `apply_to_transcript` → PostCompact hook → emit 事件。这是逻辑重复，如果将来修改压缩触发条件需要同步修改两处。

**建议**：提取 `CompactionRunner` 结构体，封装完整的触发-执行-emit 流程。

---

### 2.4 `mcp.rs` 1931 行，`main.rs` 2222 行

- `mcp.rs`：混合了 transport 连接管理、工具 spec 发现、请求分发，关注点未分离。
- `main.rs`：包含 CLI 解析、运行时构建、session 管理等，应重构为多个 builder/setup 模块。

---

## 三、代码质量问题

### 3.1 `execute_tool_calls` 返回未命名的 5-元组

**位置**：`src/run_core.rs:261-509`

```rust
-> Vec<(String, String, String, serde_json::Value, Option<AuditMeta>)>
//      id      name    result  args               audit
```

位置敏感的 5-元组使调用处必须靠数数才能知道哪个字段是什么，应改为命名结构体：

```rust
struct ToolCallOutcome {
    id: String,
    name: String,
    result: String,
    args: serde_json::Value,
    audit: Option<AuditMeta>,
}
```

---

### 3.2 `maybe_trim_transcript` 修剪策略次优

**位置**：`src/run_core.rs:182-214`

按时间顺序从旧到新修剪，意味着一个 1KB 的旧结果先于一个 100KB 的新结果被修剪。更优策略是优先修剪最大的 tool result，在相同字符数减少的前提下修改更少的消息，并更好地保留最近上下文。

同时，`Compactor::estimate_chars` 只统计 `m.content.len()`，忽略了 `tool_calls` 的 JSON 序列化体积和 `reasoning_content`，导致阈值估计系统性偏低。

---

### 3.3 生产代码中大量 `unwrap()` 隐藏 panic 风险

**位置**：`src/tui/backend.rs`

```rust
let rt = rt_opt.as_mut().unwrap();  // line 318
let rt = rt_opt.take().unwrap();     // line 444, 492
Arc::try_unwrap(rt_shared).expect("single owner after task end")  // line 467
```

如果状态机处于非 `Ready` 状态（如 `Offline` 时收到 `ConfirmPlan`），或有意外的 Arc 克隆存在，这些都会 panic。应使用模式匹配并发送错误事件。

---

### 3.4 `ToolRegistry` 破坏封装原则

**位置**：`src/tools/mod.rs:311-313`

```rust
pub headless: bool,
pub hook_runner: crate::hooks::ExternalHookRunner,
```

其他字段都是 private + getter/setter，唯独这两个是 `pub`，允许绕过 setter 直接修改。

---

### 3.5 `deferred_turn_finished` 耦合隐患

**位置**：`src/runtime.rs`

`execute_kernel_turn` 将 `TurnFinished` 事件存入 `self.deferred_turn_finished`，必须随后调用 `emit_turn_messages` 消费。如果未来任何代码路径调用了前者却跳过了后者，`TurnFinished` 会永久丢失，SDK 消费者的流会不干净地关闭。

---

## 四、次要问题

| # | 位置 | 问题 |
|---|------|------|
| 4.1 | `src/tools/mod.rs:117-121` | `unix_millis()` 失败时静默返回 0，审计记录时间戳会乱序，应至少 `warn!` |
| 4.2 | `src/session.rs` | `SESSION_SCHEMA_VERSION = 1` 无 migration 路径，schema 变更会导致旧 session 无法 resume |
| 4.3 | `src/input_state.rs:22-28` | `double_press_window()` 每次按键都读环境变量，应初始化时读取一次并缓存 |
| 4.4 | `src/run_core.rs:390-410` | 并行 batch 中 `is_readonly_for_call` 对同一条 pending 调用两次（外层 if + 内层 while），冗余计算 |
| 4.5 | `src/tui/app/mod.rs:136` | `theme: &'static Theme` 限制动态主题加载，必须通过 `Box::leak`，设计不灵活 |
| 4.6 | `src/llm/anthropic.rs:58` | `Client::builder().build().expect("reqwest client build")` 在生产路径使用 `expect`，Client build 极少失败但属违反 Invariant #5 |

---

## 五、深入分析：Session 持久化

### 5.1 session_id 命名方案无法跨平台保证唯一性

**位置**：`src/session.rs:549`

```rust
let session_id = format!("{}-{}", filesystem_safe_timestamp(), slug);
```

`filesystem_safe_timestamp()` 只精确到秒（`YYYY-MM-DDTHH-MM-SSZ`），`slug` 来自 workspace 路径哈希的前 8 位。如果同一秒内在同一工作区启动两个 session，**两个 session_id 完全一样**，第二次 `create_dir_all` 不会失败（目录已存在），transcript.jsonl 会被两个 writer 追加写入，造成静默数据混合。

**HTTP 层 `create_session`** 用 UUID v7 作 id（`generate_session_id()`），内存中无此问题；但 `AgentRuntime::run()` 的旧式 `SessionWriter::create` 路径仍使用时间戳方案。

**建议**：`create_with_tools` 中直接使用 `uuid::Uuid::now_v7().to_string()`，完全消灭时钟冲突。

---

### 5.2 `SessionWriter` 每次 `append` 都重新 open 文件

**位置**：`src/session.rs:602-622`（`CheckpointLogWriter::append`，`SessionWriter::append_entry`）

`SessionWriter::append_entry` 重复检查文件可用性，但 `CheckpointLogWriter::append` 每次调用都重新 `OpenOptions::open`，重复走 OS 的 `open(2)`。对于每 step 都写一次 checkpoint 日志的场景，这是不必要的 fd 分配-关闭循环，极端情况下可以触发文件描述符限制。

**建议**：持有一个长期 `BufWriter<File>`，仅在 `Drop` 时关闭。`SessionWriter` 已这样做（`writer: BufWriter<std::fs::File>`），`CheckpointLogWriter` 应同样改造。

---

### 5.3 schema 版本字段存在但无 migration 路径

**位置**：`src/session.rs:SESSION_SCHEMA_VERSION = 1`（legacy JSON session）和 `SessionMeta`

JSONL 格式 `SessionMeta` 缺少 `schema_version` 字段，未来字段新增需要在每处 `serde(default)` 打补丁。`SessionFile`（旧 JSON 格式）有 `schema_version` 常量但无 migration 代码，`validate_tool_registry` 直接中止而非尝试迁移。

**建议**：在 `SessionMeta` 中加入 `schema_version: u32` 字段，并为跨版本 resume 提供显式的 `migrate_v1_to_v2` 路径。

---

## 六、深入分析：HTTP Server 层

### 6.1 `POST /run` 与 `POST /sessions/:id/messages` 存在语义不一致

**位置**：`src/http/handlers.rs:40-150` vs `src/http/handlers.rs:746-877`

两个端点都运行 agent，但行为差异大：

| 特性 | POST /run | POST /sessions/:id/messages |
|------|-----------|----------------------------|
| 失败时 metrics | `agent_runs_failed` 先于 `agent_runs_total` 递增 | 成功时 `agent_runs_success` 不递增 |
| usage tracking | `tokens_prompt_total` / `tokens_completion_total` 更新 | **不更新 metrics** |
| 中断支持 | ❌ | ✅ |
| SSE 支持 | ❌ | ✅ |

最关键的是：`POST /sessions/:id/messages` 运行后，全局 metrics 的 token 和 step 计数**从不更新**，导致 `/metrics` 端点数据不准。

**建议**：提取 `update_metrics_after_turn(state, outcome)` 函数，两个端点统一调用。

---

### 6.2 `days_to_ymd` 实现重复且低效

**位置**：`src/http/handlers.rs:195-224` 和 `src/session.rs:211-223`

同一个"从 epoch 天数转换为年/月/日"的算法在两处几乎独立实现：handlers 用循环逐年减法（O(year - 1970) 复杂度），session.rs 用 Tomohiko Sakamoto 算法（O(1)）。应统一为一个公共函数，或直接引入 `chrono`（Cargo.toml 中 `chrono` 已作为 dev-dependency 存在，可提升为 regular dependency）。

---

### 6.3 Session 内存无上限，泄漏风险高

**位置**：`src/http/mod.rs:267`

```rust
pub sessions: Arc<RwLock<HashMap<String, SessionState>>>,
```

`delete_session` 端点可以删除，但没有自动驱逐（LRU、TTL、内存压力）。每个 `SessionState` 持有完整 `AgentRuntime`（含完整 transcript），长时间运行的服务器最终会 OOM。

**建议**：引入 `max_sessions` 配置参数，超出时 LRU 驱逐最久未活跃的 session，并在 `AgentRuntime.drop()` 确保 transcript 已持久化到磁盘。

---

### 6.4 `runtime_goal_state_clear` 的 busyloop 重试

**位置**：`src/http/handlers.rs:690-699`

```rust
for _ in 0..5u8 {
    if let Ok(rt) = runtime.try_lock() {
        rt.clear_goal().await;
        return;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
}
```

50ms × 5 次的轮询是一个非正式的 hack，不保证在 250ms 内成功（agent 步骤可能更长）。更合理的方式是在 `AgentRuntime` 里暴露一个 `clear_goal_token: Arc<AtomicBool>`，HTTP handler 直接设置，无需锁。

---

## 七、深入分析：MCP 协议层

### 7.1 stdio transport 中，non-matching ID 的响应被丢弃而非缓冲

**位置**：`src/mcp.rs:860-881`

```rust
if let Some(resp_id) = parsed.get("id") {
    if resp_id.as_u64() == Some(expected_id) {
        // process ...
    }
    // else: silently skip this line and read next
}
```

如果 MCP 服务器发来了一条通知（无 `id`）或一条属于不同并发请求的响应，这条消息会被**永久丢弃**。由于 `McpClient` 的 `&mut self` 要求限制了并发，当前不支持多路复用请求；但若将来放开 `Arc<Mutex<McpClient>>`，这里会造成响应丢失。

**当前风险**：服务器发出的 JSON-RPC 通知（如服务器主动 push 的工具变更通知）会被静默丢弃，没有任何日志。

**建议**：记录 `tracing::debug!` 至少，未来实现一个通知队列。

---

### 7.2 HTTP+SSE transport 缺乏重连机制

**位置**：`src/mcp.rs:316-400`（`spawn_http_sse`）

SSE 连接断开后没有任何重连逻辑。如果远端 MCP server 短暂重启（CI 场景很常见），整个 `McpClient` 实例变为无效，下次 `send_request` 会返回网络错误，agent 必须被整体重启才能恢复。

**建议**：在 `read_response` 检测到连接断开时，尝试 exponential backoff 重建 SSE 连接。

---

### 7.3 `McpServerConfig` 和 `McpServer` 结构体字段重叠

**位置**：`src/mcp.rs:36-64`

`McpServerConfig`（从 `.mcp.json` 反序列化）和 `McpServer`（运行时使用）是几乎完全相同的两个结构体。二者之间没有 `From<McpServerConfig> for McpServer` 实现，调用方需要手动构造 `McpServer`，容易漏字段。

**建议**：合并为一个结构体，或至少实现 `From`/`Into` 转换消除手动复制。

---

## 八、深入分析：Permissions 自动分类器

### 8.1 分类器的 JSON 解析失败默认"允许"，安全假设反向

**位置**：`src/permissions/auto_classifier.rs:147-150`

```rust
Err(_) => {
    // Parse failure — conservative: allow the tool
    Ok((false, "classifier parse error, defaulting to allow".into()))
}
```

注释称"conservative"（保守），但对于安全分类器来说，**解析失败应该默认拒绝**，而非默认允许。LLM 可能生成格式错误的 JSON（markdown 代码块、前置说明文字），此时会误放行本应拦截的操作。

**建议**：改为 `Ok((true, "classifier parse error, defaulting to block"))`，并在 `reason` 中说明原因，方便调试。

---

### 8.2 `CLASSIFIER_PROMPT` 包含原始 HTML 风格字符串替换，无注入防护

**位置**：`src/permissions/auto_classifier.rs:18-28`

```rust
const CLASSIFIER_PROMPT: &str = "...
  args: {args_summary}
...";

let prompt = CLASSIFIER_PROMPT
    .replace("{tool_name}", tool_name)
    .replace("{args_summary}", args_summary)
    .replace("{transcript_snippet}", transcript_snippet);
```

`args_summary` 来自 tool 调用参数（LLM 可控），如果参数中包含 `{transcript_snippet}` 这样的字符串，前一个 `.replace` 的结果会被后续 `.replace` 再次处理，形成提示注入向量。

**建议**：改用 `format!` 或专门的模板引擎，避免链式字符串替换；或先做转义。

---

### 8.3 `DenialTracker` 的字段是 `pub`，外部可绕过计数器

**位置**：`src/permissions/auto_classifier.rs:38-43`

```rust
pub struct DenialTracker {
    pub consecutive: u32,
    pub total: u32,
}
```

外部代码可直接设置 `tracker.total = 0` 来重置安全限制。这些字段应该是私有的，只通过 `record_denial`/`record_allow` 修改。

---

## 九、深入分析：Hooks 系统

### 9.1 LLM Prompt 类型的 hook 会使安全响应时间无上限

**位置**：`src/hooks/external.rs:242-246`

```rust
ResolvedHookKind::Prompt {
    prompt: String,
},
```

`HOOK_TIMEOUT: Duration = Duration::from_secs(5)` 的超时只应用于外部进程和 HTTP hooks。LLM Prompt 类型的 hook（通过 `complete_simple` 调用 LLM）使用的是 AutoClassifier 的 60 秒 timeout，但该 timeout 是**在 AutoClassifier 内部**，不是在外部 hook runner 里。如果 Prompt hook 被用于 `PreToolCall`，每个工具调用前会最多阻塞 60 秒。

**建议**：为 Prompt hook 也加明确的 deadline，建议默认 10-15 秒，可通过 hooks config 覆盖。

---

### 9.2 `ExternalHookRunner` 的 `dedup_set: Arc<Mutex<HashSet<String>>>` 可能永不清理

**位置**：`src/hooks/external.rs:28`（推断）— 结合 hooks 配置中的 dedup 字段

去重 set 在 `Setup` 事件等单次触发 hooks 中防止重复执行是合理的，但如果 runner 被复用于多个 sessions（HTTP 模式下 `AppState.tool_registry` 在所有 session 间共享），setup hooks 的 dedup 状态会永久保留——即使在新 session 中，`Setup` hook 也不会重新执行。

**建议**：dedup set 应属于 session 级别，而非 runner 全局状态；或在每个新 session 创建时 `clear()` dedup set。

---

### 9.3 Hook 协议中 `updated_input` 字段无类型验证

**位置**：`src/hooks/external.rs:125`

```rust
pub updated_input: Option<serde_json::Value>,
```

Hook 可以返回任意 JSON 作为 `updated_input` 来覆盖工具参数，但没有任何 schema 验证。若 hook 返回错误结构的 JSON，该 JSON 会被直接传给工具执行，可能引发工具内部的 panic 或意外行为。

**建议**：在执行工具前，使用工具的 `parameters` schema 对 `updated_input` 进行验证。

---

## 十、自我迭代 Dashboard 可观测性分析

### 10.1 现有可观测数据盘点

通过对 `.dev/metrics/*.yaml`、`observe.sh`、`metrics-summary.sh`、`checkpoint_log.rs`、`session.rs` 和 HTTP `/metrics` 端点的分析，现有可观测数据如下：

**已有（shell metrics + YAML）**：
- `run_id`、`goal_tag`、`provider`、`model`、`batch`
- `outcome`（committed / rolled-back / panic / skip-commit）
- `exit_reason`、`steps_used`/`steps_budget`
- `total_tool_calls`、`error_count`
- `tokens_prompt`/`tokens_completion`、`cost_usd`、`wall_time_seconds`
- `files_changed`/`lines_added`/`lines_removed`
- `test_pass`、`self_review_enabled`、`review_verdict`、`review_rounds`

**已有（JSONL session + CheckpointLog）**：
- 每条消息的 `uuid`、`parent_uuid`、`role`、`content`、`timestamp`
- 每条消息的 `usage`（input/output/cache tokens）
- 每轮（turn）的 `pre`/`post` checkpoint、`touched_files`、`started_at`/`finished_at`
- `audit` 元数据（工具名称、耗时、side effect 类型）

**已有（HTTP /metrics 端点）**：
- `requests_total`、`requests_active`
- `agent_runs_total`/`success`/`failed`
- `tokens_prompt_total`/`tokens_completion_total`
- `agent_steps_total`

### 10.2 明显缺失的可观测维度

| 维度 | 现状 | 影响 |
|------|------|------|
| **per-goal 成功率趋势** | 仅存在 YAML 文件，无时序聚合 | 无法看到某个 goal 随 batch 的改进曲线 |
| **apply_patch 成功率** | observe.sh 只统计调用次数 | 不知道有多少 patch 被拒绝（表示上下文不对齐） |
| **stuck 子类型** | `hit_stuck=yes/no` | 不知道是工具循环还是 LLM 重复输出导致的 stuck |
| **跨 session 成本归因** | 每 run 有 cost_usd | 无法按 goal 类型、模型聚合分析 ROI |
| **工具耗时 P50/P99** | `AuditMeta` 记录了 duration_ms | 仅在 JSONL 里，没有汇总指标 |
| **compaction 触发频率** | CompactBoundaryEntry 写入 JSONL | 无汇总；不知道哪些 goal 频繁触发 compaction |
| **token 使用中 cache hit 率** | `UsageMeta.cache_read_tokens` 存在 | 未被暴露到 metrics 端点或 YAML |
| **revision 循环深度** | `review_rounds` 字段存在 | 仅最终数值，中间每轮的修改内容不可查 |

### 10.3 Dashboard 设计建议

基于以上分析，建议构建一个 **本地 HTML/JSON 静态 Dashboard**（可以通过 `cargo run -- dashboard serve` 启动），数据来源为已有的 `.dev/metrics/*.yaml` 和 JSONL session 文件。

#### 核心面板

**Panel 1: 迭代健康总览**
- commit rate % (时间序列折线图，按 batch)
- rollback rate % 
- panic rate %
- 最近 N 次运行的成功/失败序列（sparkline）

**Panel 2: 效率指标**
- avg steps/run 趋势
- avg cost/commit 趋势（剔除 rollback）
- apply_patch:write_file ratio（目标 > 3:1）
- avg wall time

**Panel 3: 卡顿与失败分析**
- stuck 触发率（按 `exit_reason` 分类）
- error_count 分布（按工具类型）
- budget exceeded 触发率

**Panel 4: 模型/Provider 对比**
- 按 provider 的 commit rate
- 按 provider 的 cost per commit
- 各 provider token 效率（commits / 1000 tokens）

**Panel 5: Token 经济**
- cache hit rate (= cache_read / (input - cache_read) %)
- compaction 频率
- reasoning token 占比（针对 DeepSeek R1/o1）

#### 数据管道建议

```
.dev/metrics/*.yaml  →  metrics-summary.sh  →  JSON API
JSONL sessions       →  session analyzer    →  JSON API
                                           ↓
                               HTML Dashboard (静态 JS)
```

**建议引入 `recursive dashboard` 子命令**，直接解析 `.dev/metrics/` 和 `~/.recursive/sessions/`，生成单文件 HTML 报告（内嵌 ECharts/Chart.js），可以被 `open` 直接查看，也可以接入 CI artifact。

### 10.4 最小可行实现路径

1. **新增 `src/tools/metrics_reader.rs`**：解析 `.dev/metrics/*.yaml` 返回结构化 `RunMetrics` 列表
2. **新增 HTTP 端点 `GET /dev/metrics`**：聚合返回 JSON（仅在 dev/headless 模式开启）
3. **新增 `GET /dev/metrics/html`**：返回单文件 HTML dashboard（内嵌数据 + Chart.js CDN）
4. **更新 `metrics-summary.sh`**：添加 cache hit rate、工具耗时 P50/P99 等新指标



---

## 十一、深入分析：Compaction / Checkpoint / Shell 工具 / main.rs

### 11.1 `compact()` 和 `apply_to_transcript()` 的分割点计算不一致

**位置**：`src/compact.rs:206-265`（`compact`）vs `src/compact.rs:175-195`（`apply_to_transcript`）

`compact()` 用 `n = self.keep_recent_n.min(transcript.len() - 1)`、`split = len - n` 计算旧消息范围（用于摘要），但 `apply_to_transcript()` 用同样的 `keep`/`split` 逻辑计算 drain 范围（用于实际删除）。**两者的 split 点相互独立**：

```rust
// compact() 内部
let n = self.keep_recent_n.min(transcript.len().saturating_sub(1));
let split = transcript.len().saturating_sub(n);  // "older" 范围

// apply_to_transcript() 独立计算
let keep = self.keep_recent_n;
let mut split = transcript.len().saturating_sub(keep);  // "drain" 范围
```

如果 `apply_to_transcript()` 后退了 split（因为命中 `Role::Tool`），而 `compact()` 使用未后退的 split 来决定摘要哪些消息，**summary 摘要的消息集合和实际 drain 的消息集合不完全一致**。极端情况：工具结果被 drain 掉，但其对应的 assistant tool_call 消息被摘要（因为 compact 时未后退，而 apply_to_transcript 后退了）。

**建议**：将分割点计算提取为单一 `fn safe_split_point(transcript, keep_n) -> usize`，`compact()` 和 `apply_to_transcript()` 共用同一结果。

---

### 11.2 `estimate_chars` 遗漏了 tool_calls 的序列化体积

**位置**：`src/compact.rs:53-55`

```rust
pub fn estimate_chars(transcript: &[Message]) -> usize {
    transcript.iter().map(|m| m.content.len()).sum()
}
```

`m.content` 只计算了文字内容。`m.tool_calls`（每个含 `name`、`id`、`arguments` JSON 串）和 `m.reasoning_content`（DeepSeek 思维链）完全不在估算里。一个 step 调用了 5 个工具，每个 `arguments` 是 500 字符的 JSON，估算就少了 2500 字符。对 compaction 阈值的判断会系统性偏低，导致压缩触发比预期晚。

---

### 11.3 Checkpoint 系统每次操作都 fork 新 git 进程

**位置**：`src/checkpoint.rs:143-265`（`snapshot_for_session`）

`snapshot_for_session` 一次快照需要顺序执行 4 个同步 `git` 子进程：`git add`、`git write-tree`、`git commit-tree`、`git update-ref`。在 agent 每次 turn 开始和结束时都要调用两次快照，每次 4 个进程，一个 50 步的 run 会产生 400 次 `git` fork。

- `std::process::Command::output()` 是同步阻塞的，在 tokio async 上下文中会阻塞工作线程
- 如果 git 因为 gitconfig 读取慢（NFS/远程文件系统）每次多花 50ms，整个 run 会多花 20 秒

**建议**：将 `ShadowRepo` 方法包装在 `tokio::task::spawn_blocking` 中，使其异步化；或使用 `git2` crate（libgit2 bindings）避免 fork。

---

### 11.4 `restore_paths` 没有路径规范化，存在 path traversal 风险

**位置**：`src/checkpoint.rs:345-398`

```rust
pub fn restore_paths(&self, checkpoint: &CheckpointId, paths: &[String]) -> Result<RestoreStats> {
    for path in paths {
        let abs = self.workspace.join(path);  // 直接 join，没有 resolve_within 检查
```

`paths` 来自 `CheckpointRecord::touched_files`，是 `run_shell` 的 shell-diff 归因结果。如果 shell 命令（通过 symlink 或 `../` 写入）产生了包含 `..` 的路径，`restore_paths` 会盲目地在 workspace 外写入/删除文件。`validate_session_id` 对 session_id 做了路径检查，但 `restore_paths` 对文件路径没有同等保护。

**建议**：在 `restore_paths` 的循环里用 `resolve_within(&self.workspace, path)` 验证 `abs` 确实在 workspace 内。

---

### 11.5 Shell 工具的沙箱仅保护 `cwd`，不限制命令本身

**位置**：`src/tools/shell.rs:79-91`

```rust
let cwd = if let Some(rel) = args.get("cwd").and_then(|v| v.as_str()) {
    resolve_within(&self.root, rel).map_err(...)?
} else {
    self.root.clone()
};
let mut cmd = Command::new("/bin/sh");
cmd.arg("-c").arg(command);   // command 本身没有任何限制
cmd.current_dir(&cwd);
```

`cwd` 的路径通过 `resolve_within` 做了沙箱验证，但 `command` 字符串本身（`/bin/sh -c "$command"`）完全未受约束。agent 可以执行：
- `cat /etc/passwd`（读 workspace 外的文件）
- `curl http://...`（外部网络请求）  
- `rm -rf /`（如果有权限）

**这是设计决策**（`ToolSideEffect::External` 的语义），不是 bug。但它意味着：在 `PermissionMode::Auto` 或 `BypassPermissions` 下，RunShell 的安全性完全依赖 AutoClassifier 和权限层，而不是内核层面的强制隔离。README/文档中应明确说明这一点，并建议生产环境使用容器化（`docker_sandbox` feature）。

---

### 11.6 `main.rs` 2222 行，`main()` 函数 500+ 行，参数冲突检测缺失

**位置**：`src/main.rs`

`main()` 函数处理所有参数的合并、provider 选择、工具注册、subcommand 分发，超过 500 行。问题：

1. **互斥参数未显式声明冲突**：`--json` 和 `--output-format=json` 语义重叠，`--system-prompt` 和 `--system-prompt-file` 互斥，但 Clap 的 `conflicts_with` 特性未被使用，靠代码中的 `if let Some`/`else` 约定处理，容易漏掉组合。

2. **`effective_json` 等局部变量跨越 500+ 行**：在 `main()` 顶部定义的变量一直到底部才被消费，阅读时难以追踪状态。

3. **subcommand 处理全部在同一个 `main()` 中**：应提取为 `run_cmd_run()`、`run_cmd_resume()`、`run_cmd_sessions()` 等独立函数。

**建议**：
- 将 `main()` 重构为：参数解析 → `AppConfig::from_cli(cli)` → `dispatch_cmd(app_config, cmd)` 三层
- 对互斥参数使用 `#[clap(conflicts_with = "...")]` 声明

---

### 11.7 `compact()` 的 tool_calls 内容被 XML 转义包裹，但 `m.content` 可能包含 `<`

**位置**：`src/compact.rs:218-230`

```rust
format!("<{role_tag}>{}</{role_tag}>", m.content)
```

`m.content` 是原始字符串，可能包含 `<`、`>`、`&` 等字符（如文件内容、代码片段）。将其直接嵌入 XML 风格标签中，会在摘要 prompt 里产生格式混乱，误导 LLM 解析结构。

另外，`tool_calls` 字段完全没有被序列化进 `older_text`，即历史工具调用的名称和参数在摘要 prompt 里不可见，LLM 摘要时会遗漏哪些工具被调用过。

---

## 十二、整体评分与修复优先级

### 评分

| 维度 | 分数 | 说明 |
|------|------|------|
| 核心 agent 循环 | 8/10 | `RunCore` 设计清晰，数据流向明确 |
| 错误处理 | 6/10 | `error.rs` 规范，但生产代码有 unwrap |
| TUI 层 | 4/10 | God Object + 超长函数，职责爆炸 |
| 权限系统 | 6/10 | 多路径冗余，语义不一致 |
| 测试覆盖 | 5/10 | 核心类型有测试，集成路径覆盖不足 |
| 可观测性 | 4/10 | 日志有 tracing，但缺结构化 metrics |

### 修复优先级

| 优先级 | 问题 | 影响 | 成本 |
|--------|------|------|------|
| **P0** | 合并双重重试逻辑 | 高（实际重试次数翻倍，成本加倍） | 低 |
| **P0** | 修复 Stuck 检测 | 高（agent 循环卡死消耗全部预算） | 低 |
| **P0** | 修复 `blake3_canonical_json` | 高（args_hash 不稳定，resume 漏检） | 低 |
| **P0** | AutoClassifier 失败改为默认 block | 高（安全假设方向错误） | 低 |
| **P1** | `restore_paths` 加 `resolve_within` 验证 | 高（path traversal 风险） | 低 |
| **P1** | 统一 permission hook 路径 | 中（`RunCore` hook 路径是死代码） | 中 |
| **P1** | `backend.rs` unwrap 替换为 pattern match | 中（生产 panic 风险） | 低 |
| **P1** | Checkpoint `snapshot_for_session` 改 `spawn_blocking` | 中（阻塞 tokio 线程） | 中 |
| **P1** | `estimate_chars` 补充 tool_calls 体积 | 中（compaction 阈值偏低） | 低 |
| **P1** | Session metrics 更新不一致（POST /messages 不更新） | 中（监控数据失真） | 低 |
| **P2** | `compact()` / `apply_to_transcript()` 分割点统一 | 中（摘要与 drain 不一致） | 低 |
| **P2** | `App` 拆分 + `commands.rs` 重构 | 低（可维护性） | 高 |
| **P2** | Sessions 内存上限 | 低（长期服务器 OOM 风险） | 中 |
| **P2** | `main()` 拆分为子函数 | 低（可维护性） | 中 |
| **P3** | MCP SSE 重连机制 | 低（稳定性） | 中 |
| **P3** | compact 的 tool_calls 加入 older_text | 低（摘要质量） | 低 |
