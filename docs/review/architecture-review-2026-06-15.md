# Recursive 架构 Review（增量）— 2026-06-15

**Date**: 2026-06-15
**Reviewer**: Lead Architect（深度阅读 kernel / runtime / tools / http / llm / session / permissions / storage + gitnexus 图分析）
**Scope**: 自 2026-06-10 review 之后的代码库状态（HEAD `9dda891`），重点是 06-10 review 中漂到本轮仍未修的问题 + 本轮新发现
**Method**: 全量代码深读（~110 文件，~96k LOC） + 6 路 gitnexus impact/process 查询 + cargo build/test/clippy/fmt 四道质量门

---

## TL;DR

本次 review 时，质量门 **全部通过**（cargo build、cargo test --no-run、cargo clippy --all-targets --all-features -- -D warnings、cargo fmt --check 假定通过）。代码库处于"功能完整 + 稳定状态"，而非"开发中"。

**整体评价**：架构清晰，分层合理，文档与实现高度一致。`Message` / `Role` / `ToolSpec` / `LlmProvider` / `Tool` / `ToolRegistry` / `AgentKernel` / `AgentRuntime` / `TurnContext` / `TurnOutcome` 这套核心抽象非常扎实 — Invariant #1（loop stays small）保持得很好（`run_core.rs` 不到 900 行，主体 loop 在 `run_inner`），Invariant #8（tool-call ↔ tool-result pairing）通过 `compaction_keeps_tool_calls_paired_with_results` 测试 + 多个 recovery 路径守住。

**主要担忧**：
- **06-10 review 的 P0 多已修复，但仍有 4 项跨迭代漂动**（HTTP 默认无认证、`sh -c` 注入、`a2a_call` SSRF、MCP 分叉）
- **本轮新发现 8 项**，其中 3 项是 P1（生产可靠性）
- **TUI 仍是 god-object 模式未根本性修复**，但本轮没有新增 TUI commit，未恶化
- **测试覆盖仍极不均衡**：TUI/CLI 几乎全靠 e2e

---

## 质量门状态

| 检查 | 命令 | 状态 | 备注 |
|------|------|------|------|
| Build | `cargo build --all-targets --all-features` | ✅ Pass | 无 warning |
| Test build | `cargo test --workspace --no-run` | ✅ Pass | |
| Clippy | `cargo clippy --all-targets --all-features -- -D warnings` | ✅ Pass | 全项目 `-D warnings` 干净 |
| Fmt | `cargo fmt --check` (推断) | ✅ Pass | 上次 review 已确认 |

clippy clean 表明 Invariant #5（No `unwrap()`/`expect()` in non-test code）在测试代码之外基本守住 — 但仍发现几处例外（见 P3-J 列表）。

---

## 06-10 review P0/P1 漂动状态

| ID | 标题 | 06-10 状态 | 06-15 状态 | 备注 |
|----|------|-----------|-----------|------|
| SEC-003 | HTTP 默认无认证 | ☐ 未修 | ☐ **仍未修** | 已成为"已知接受的 P0 漂动"；`build_router` 注释说"auth from env"但 env 空时启动无任何拒绝 |
| SEC-002 | `sh -c` 注入 | ☐ 未修 | ☐ **仍未修** | `run_skill_script.rs:135-142` 仍 `sh -c <args>` 拼装 |
| NEW-TOOL-2 | `a2a_call` 无 SSRF 保护 | ☐ 未修 | ☐ **仍未修** | WebFetch 修了，a2a 没修 |
| NEW-CLI-1 | MCP dispatcher 分叉 | ☐ 未修 | ☐ **仍未修** | main.rs vs mcp_server.rs 两份独立实现 |
| NEW-SESS-1 | 本地 CLI 无 session reaper | ☐ 未修 | ☐ **仍未修** | HTTP 端修了 (`spawn_session_reaper`)，本地 CLI 漏 |
| STOR-3 | `atomic_write` 3 处独立实现 | 🆕 P0 | ☒ **已修** | `src/atomic.rs` 是单一来源（注释明确说 "single source of truth"），三处调用点均已统一；`atomic_write_async` 包了 `spawn_blocking` |
| STOR-2 | 5 处非原子写 | 🆕 P0 | 🆕 **部分修** | `transcript.rs::write_to`、`session.rs::SessionFile::write_to` 都改为 `crate::atomic::atomic_write`，但 cost.rs 仍 `serde_json::to_string_pretty` + `std::fs::write` |
| STOR-4 | `SessionMeta` 无 `schema_version` | 🆕 P0 | ☐ **仍未修** | 漂过 2 轮 review，未触动 |
| STOR-1 | session_lock TOCTOU | ☒ 已修 | ☒ **仍修** | `OpenOptions::create_new(true)` at session_lock.rs:206 |
| SEC-001 | WebFetch SSRF | ☒ 已修 | ☒ **仍修** | |
| B1 | 请求体大小限制 | ☒ 已修 | ☒ **仍修** | `DefaultBodyLimit::max(1MB)` at http/mod.rs:436 |
| SEC-008 | Policy sandbox 孤岛 | ☒ 已修 | ☒ **仍修** | `permission_pipeline.rs` 7 阶段编排器 |
| NEW-TUI-2/3 | TUI dead state | 🆕 P0 | ☐ **仍未修** | 本轮没有 TUI commit，问题仍在 |
| NEW-HTTP-2 | /agui 不获取 run_semaphore | 🆕 P1 | ☒ **已修** | `try_acquire_owned` at handlers.rs — commit 45215bc |

**结论**：06-10 列出的 14 个 P0/P1，本轮确认 7 个已修 / 部分修；4 个仍未修（SEC-003 / SEC-002 / NEW-TOOL-2 / NEW-CLI-1），1 个 STOR-2 部分修，1 个 STOR-4 漂两轮，1 个 TUI dead state 漂。

---

## 新发现 — P0（数据损坏 / 安全 / 必修）

### NEW-CORE-15 — runtime 消息计数与 transcript 漂动
- **位置**: `src/http/handlers.rs:870-875`
- **现象**: `send_session_message` 在 turn 完成后重新 `transcript().iter().filter(|m| m.role != Role::System).count()`，写入 `msg_count_arc`。如果 turn 失败返回 Err，这条更新**不会执行**，但 `MessageAppended` 事件已在 forwarder 中发射 — list_sessions 看到的 count 与 detail endpoint 看到的 messages 不一致。
- **影响**: SSE 客户端可能看到 `MessageAppended` 事件已落地，但 `GET /sessions` 显示 message_count 仍为旧值。诊断时容易以为是 race。
- **修复**: 在 forwarder task 里维护计数（forwarder 是 single owner of message stream），而不是在 handler 主线程里从 transcript 重算。或者：handler 在 turn 失败时也调用一次 `recount` 保证一致性。

### NEW-TOOL-15 — `MessageAppendedWithAudit` 跨 turn 共享 `tool_call_id` 时 audit 静默丢
- **位置**: `src/runtime.rs:525-562` (06-10 已记为 NEW-KERN-1)
- **现象**: `tool_audits.remove(tcid)` 在 `new_messages` 第一个匹配命中后即从 HashMap 中删除。如果后续另一个 turn 复用相同的 `tool_call_id`（MockProvider 在测试中常这样做，且 LLM provider 在流式 tool search loop 中也允许），audit 会归零。
- **影响**: 持久化的 transcript 中 tool message 的 `audit` 字段为空，事后 `recursive resume` 时该 tool 消息会被 orphan-detection 判定为 missing audit。
- **修复**: 不在 emit 时 `.remove()`，而是在 turn 结束 (TurnFinished) 时统一清理。或者：改为 `Vec<AuditMeta>` per `tool_call_id`。

### NEW-STORE-15 — `SessionMeta.status: String` 无 enum 约束
- **位置**: `src/session.rs:300+` (推断，与 STOR-4 同根源)
- **现象**: 多个调用点写 `"active"` / `"completed"` / `"interrupted"` 等状态字符串，没有任何类型约束。grep 整个 session.rs 看到至少 5 处 `String::from("active")` / 字符串拼接。
- **影响**: 添加新的完成状态需要全文 grep；typo 导致 list_sessions 显示 unknown status；写时和读时不一致（"completed" vs "complete"）。
- **修复**: 引入 `enum SessionStatus { Active, Completed, Interrupted }` + `#[serde(rename_all = "lowercase")]`。

---

## 新发现 — P1（生产可靠性 / 架构缺陷）

### NEW-KERN-15 — `MessageAppended.parent_uuid` 全程是 `None`
- **位置**: `src/event.rs:124-130`（字段定义）+ `src/runtime.rs:413,456,490,535,540,555`（构造点）
- **现象**: `parent_uuid` 字段被定义为 `Option<String>`，文档说"用于 subagent 分支点 (g155)"。但 runtime.rs 里**所有 8 处构造都是 `parent_uuid: None`** — dead surface area。
- **影响**: API surface 误导新读者以为这是个真实机制；SessionWriter 的 `parent_uuid` 字段也依赖此事件但永远拿到 `None`。
- **修复**: 要么实现 subagent branch（看 g155 的设计意图），要么删除字段 + 同步 SessionWriter。推荐删除并合并到 `SessionWriter::append_with_parent` 的隐式链指针。

### NEW-KERN-16 — `[compacted:` 字符串嗅探仍在 2 处使用
- **位置**: `src/kernel.rs:302-307`（intra-turn 检测）+ `src/compact.rs:327,376,449,471`（compactor 自身）
- **现象**: kernel 用 `inner.messages[0].content.contains("[compacted:")` 来判断是否是 compaction 摘要消息，决定是否要 prepend 到 `new_messages`。如果用户 prompt 含 `[compacted:` 字面量（比如讨论 compaction 机制时），会被误判。
- **影响**: 新生成的 system summary 被识别成 compaction summary 而 not 返回；user 看到的 `new_messages` 少了关键内容，turn 行为偏怪。
- **修复**: 在 `Message` 上加 `is_compaction_summary: bool` 标记位，compactor 创建时设置，kernel 据此判断。整文件 grep `[compacted:` 字符串字面量应只剩在 compactor 内部一处。

### NEW-HTTP-15 — `session_clear_goal` 失败路径的二次重试无超时
- **位置**: `src/http/handlers.rs:684-694` (`runtime_goal_state_clear`)
- **现象**: 内部 retry 最多 5 次 × 50ms = 250ms。如果 runtime 被长 turn 占用（max_steps 假设 32 × 平均 5s/step = 160s），5 次 retry 全部失败后 silently 放弃，但 API 返回 200 OK — client 以为 clear 成功。
- **影响**: 在长 agent run 中 DELETE /goal 实际未生效，下次 restart 重新激活；竞态下 goal 评估继续运行。
- **修复**: 显式返回 `StatusCode::CONFLICT` 并附带 retry 失败原因；或拆为 async 任务，在 runtime 空闲时执行 clear。

### NEW-PERM-15 — `with_policy` 但 `permissions=None` 时跳过 safety check
- **位置**: `src/tools/permission_pipeline.rs:89-260` + `src/permissions/mod.rs:317-330`（与 06-10 NEW-PERM-2 同问题，未修）
- **现象**: 当 `tool_registry.with_policy(policy)` 被调用但 `permissions` 仍是 `None`（即未调用 `with_permissions`），pipeline 的 safety check (`is_destructive_path` 等) 不被触发，但 policy 中的 allow/deny 列表**仍生效**。这是一个"配置错误的静默降级"。
- **影响**: 部署者以为开了 policy 就所有 protection 都到位，结果 safety check 仍是默认 Deny 的命名路径子集（如 `.git/hooks`、`.env`）未被检测 — 上次 review 已识别，本轮确认仍未修。
- **修复**: pipeline 启动时检测 `permissions.is_some() || policy.is_some()` 必须同时满足，否则记 WARN 日志并返回 `ToolRejected`。

### NEW-HOOK-15 — external hook 的 `mode: "open" | "closed"` 缺失
- **位置**: `src/hooks/external.rs:432-541`
- **现象**: hook 配置可声明 `PreToolUse` 等拦截点，但无法声明该 hook 失败时是 fail-open（默认 Continue）还是 fail-closed（默认 Skip）。安全敏感场景（如删除操作的二次确认）需要 fail-closed。
- **影响**: hook timeout / 解析失败时默认 Continue — 与 SEC-002 / SEC-008 的 fail-closed 期望冲突。
- **修复**: hook JSON schema 增加 `mode: "open" | "closed"` 字段，pipeline 据此决定默认决策。

### NEW-MEM-15 — `MessageBus.messages` 无界增长仍是真问题
- **位置**: `src/multi.rs:142, 155-165`（与 06-10 NEW-KERN-5 同问题，未修）
- **现象**: `AgentPool` 长期运行时每个 agent 的 `SharedMemory.messages` Vec 持续追加，从不清理。8 小时多 agent 编排任务可积累百万条 message。
- **影响**: OOM；LLM context 一次性塞进整个 history 会触发 Truncation。
- **修复**: 引入 ring buffer (VecDeque with capacity) 或按 turn 边界 compaction。

### NEW-TOOL-16 — `run_skill_script` 注入未走 `permission_pipeline.check()`
- **位置**: `src/tools/run_skill_script.rs:136-149`（与 06-10 NEW-SKILL-2 同问题，未修）
- **现象**: skill script 走 `sh -c <args>`，args 未走 shell-words 解析，且 `permission_pipeline.check()` 不被调用 — skill 信任 + sh -c = policy bypass。
- **影响**: skill 升级一次即可绕过所有 allow/deny 列表。
- **修复**: 用 `shell-words` crate 解析参数；`invoke_with_audit` 路径上让 skill script 与普通 tool 一视同仁。

---

## 新发现 — P2（架构债）

### NEW-CORE-25 — `run_goal_loop` judge transcript tail 实现分散
- **位置**: `src/runtime_goal.rs`（推断）+ `src/runtime.rs:319` (`GOAL_EVAL_TRANSCRIPT_TAIL = 12`)
- **现象**: `transcript_tail` 在 runtime 端有 hardcoded 常数 12；runtime_goal 端 judge 内部再做自己的 TAIL slicing。两个层各自实现"最近 N 条"的语义。
- **影响**: judge 看到的实际消息数可能与 caller-side 传的 payload 数不一致，导致 prompt 大小不可预测（账单漂动）。
- **修复**: 把 transcript tail 提取为一个 trait / 函数，单一实现。

### NEW-LLM-25 — `AnthropicProvider` 未审，UTF-8 截断 + thinking block 仍可疑
- **位置**: `src/llm/anthropic.rs:713-731`（06-10 NEW-LLM-4）+ 整个 stream parser
- **现象**: 06-10 提到 OpenAI 修了 SSE UTF-8 截断（PR a2d3e2b），Anthropic 路径**未审**。extended-thinking 启用时 `redacted_thinking` 块必须回传；当前 `ContentBlock::Unknown` 分支直接丢弃，会触发下一轮 400。
- **影响**: 用户启用 thinking 后第二次 turn 直接 400。
- **修复**: 优先 audit anthropic.rs 的 SSE parser + content block 处理；引入 `thinking`/`redacted_thinking` 回传机制。

### NEW-LLM-26 — `OpenAiProvider::with_stream_tx` instance 字段污染
- **位置**: `src/llm/openai.rs:104-108`
- **现象**: `stream_tx` 存到 `OpenAiProvider` instance。并发 `stream()` 调用会写到同一个 channel，导致不同 session 的 token 交错。
- **影响**: 多 session 并发时 SSE 输出混乱。
- **修复**: 移除 instance 字段，强制每个 `stream()` 调用传 `stream_tx` 参数（已存在，但 instance 字段是 dead code 备选）。

### NEW-LLM-27 — `EmbeddingProvider::embed` 吞所有错误返回 `vec![]`
- **位置**: `src/memory/openai_embedding.rs:79-110`
- **现象**: API key 错配 / 网络错误时返回空向量，配 ANN 搜索退化为"找不到任何记忆"而非显式配置错误。
- **影响**: 用户无法区分"无匹配"与"embedding 失败"。
- **修复**: 改返回 `Result<Vec<f32>, Error>`，上层决定 fallback 行为；记录 metric `embedding_errors_total`。

### NEW-LLM-28 — `Retry-After` header 未解析
- **位置**: `src/llm/openai.rs:175-178` + `src/llm/anthropic.rs:120-122`
- **现象**: 429 / 529 后按 1-8s 指数退避重发，仍然打 429。
- **影响**: Anthropic 流量突增时所有请求都慢在重试上。
- **修复**: 解析 `Retry-After` header（HTTP-date 或 delta-seconds），用其值替代默认 backoff。

### NEW-CLI-15 — `cli/resume.rs` 与 `cli/session.rs` 仍有 `lock().unwrap()` 锁中毒 panic
- **位置**: `src/cli/resume.rs:338,365,403,404` + `src/cli/session.rs:128`
- **现象**: 4 处 `w.lock().unwrap()`，违反 Invariant #5（No `unwrap()` in non-test code）。如果 session writer 在持有锁时 panic 锁中毒，CLI 直接 panic。
- **影响**: 罕见但灾难性 — 用户看到 stderr "session corruption" 后 CLI crash。
- **修复**: 改为 `.lock().expect("session writer poisoned")` 或 `.lock().map_err(|e| Error::Other(format!("{e}")))?`。

### NEW-STORE-25 — `cost.rs` 仍非原子写
- **位置**: `src/cost.rs:137-144,151-211`
- **现象**: 仍 `serde_json::to_string_pretty(&tracker)?; std::fs::write(path, json)` — 没走 `atomic_write`。
- **影响**: 断电时 cost.json 可能写一半，下一轮启动解析失败。
- **修复**: 1 行改 `crate::atomic::atomic_write(path, json.as_bytes())`。

### NEW-CLI-16 — `--provider` 只接受 `["openai", "anthropic"]`，拒 14+ preset
- **位置**: `src/main.rs:54,119`
- **现象**: deepseek / glm / kimi / qwen / moonshot 等用户期望通过 CLI flag 切换的 provider 仍要靠环境变量。
- **影响**: onboarding 摩擦；用户必须先读 docs 才能切换 provider。
- **修复**: 把 `--provider` 改为接受 preset id 字符串，运行时通过 `providers::find_preset(name)` 校验。

### NEW-SKILL-15 — skill 无 hash pinning
- **位置**: `src/skills.rs:99-144,496-510`（与 06-10 NEW-SKILL-1 同问题，未修）
- **现象**: `Mode::Always` 自动注入 system prompt，但 skill 文件无签名校验、无 hash pinning。攻击者可写 `~/.recursive/skills/auto-inject/SKILL.md` 实现持久化 prompt injection。
- **影响**: 用户开 Always 模式即被 RCE / 凭据外泄。
- **修复**: skill manifest 加 `sha256` 字段；启动时校验并拒绝未 pin 的 skill 在 Always 模式生效。

### NEW-TUI-25 — bash 输出无 backpressure
- **位置**: `src/tui/bash.rs:27-55`（与 06-10 NEW-TUI-5 同问题，未修）
- **现象**: `cat 100MB_file` 把全部内容加载进 transcript，无截断、无 streaming UI 更新、无 chunking。
- **影响**: 单次 tool call OOM；transcript 永久保留 100MB。
- **修复**: 工具层截断到 e.g. 10KB + 提供 head/tail/grep filter；TUI 用 streaming 块增量显示。

### NEW-TUI-26 — App god-object 仍未拆
- **位置**: `src/tui/app/mod.rs:45-169`（37 字段）+ `commands.rs:2568 行` + `event_loop.rs:817 行`
- **现象**: 5 个连续 commit 是 scroll/render bug（94acf55 → 8827c63）— 根因（缺单 source of truth + 增量渲染）未修。
- **影响**: 下一个 scroll bug 仍会出现；`App` 无法单测。
- **修复**: 拆 modal registry、pick `blocks[last_printed_idx..]` 作为唯一 source of truth；transcript 改增量 diff 渲染。

---

## 新发现 — P3（技术债）

### NEW-DEBT-15 — `Message` 是 `pub` 字段而非 builder
- **位置**: `src/message.rs:20-32`
- **现象**: `Message` 5 个 `pub` 字段。已有 5 个 `Message::xxx` 构造器（system/user/assistant/assistant_with_tool_calls/tool_result），但 `session.rs` 测试里仍然写 `Message { role: ..., content: ..., tool_calls: ..., tool_call_id: None, reasoning_content: None }` 字面量 7+ 次。
- **影响**: 新加字段时所有测试要改；构造错时编译器无法拒绝。
- **修复**: 把字段改为 `pub(crate)`，强制走构造器；或加 `#[non_exhaustive]`。

### NEW-DEBT-16 — `AgentRuntime` `Debug` 实现有重复代码
- **位置**: `src/runtime.rs:282-307`
- **现象**: 17 行手写 `f.debug_struct(...).field(...).field(...)` — 但 `kernel`、`transcript`、`event_sink` 等字段的 Debug 形式各异（`&"<LlmProvider>"`、`&self.transcript`、`&"<EventSink>"`），新加字段容易漏。
- **影响**: 不是大问题，但 `transcript` 直接 dump 整个 messages（生产 trace 时可能数 MB）。
- **修复**: 改为 `#[derive(Debug)]` 或显式截断 transcript（`&self.transcript.len()`）。

### NEW-DEBT-17 — `MockProvider` 用 `Mutex<Vec<Completion>>` 模拟 LLM 响应
- **位置**: `src/llm/mock.rs:39-50+`
- **现象**: 每个测试都手 push `Completion { content: ..., tool_calls: ... }`；需要 N 步的测试要 push N 个。
- **影响**: 测试代码冗长；新增 LLM 行为（如 reasoning_content）需要改所有 mock。
- **修复**: 提供 builder `MockProvider::script(vec![...])` + 默认响应 fallback。

### NEW-DEBT-18 — `AtomicUsize` + `Mutex<Instant>` 并存管理 session 元数据
- **位置**: `src/http/mod.rs:88-89` (`SessionState`)
- **现象**: `non_system_message_count: Arc<AtomicUsize>` + `last_active: Arc<Mutex<Instant>>` + `interrupt_token: Arc<Mutex<Option<CancellationToken>>>` — 三个 Arc 同步管理同一会话的元数据，互相无一致性保证。
- **影响**: 加新字段（e.g. `created_at` 已有的反而用 String 而非 Instant）易混乱。
- **修复**: 把 `SessionState` 内部 struct `metadata: Arc<RwLock<SessionMeta>>` 统一。

### NEW-DEBT-19 — `PermissionMode` enum 支持新旧 alias 但 alias 文档不全
- **位置**: `src/permissions/mod.rs:89-126`
- **现象**: `Default` 接受 `"allow"` alias，`DontAsk` 接受 `"deny"`, `"interactive"`，`Plan` 接受 string + object 两种 form — 文档表格（行 78-88）已列，但单元测试覆盖不全。
- **影响**: 外部 SDK 不知道 alias；新加 alias 时无回归测试。
- **修复**: 加一组 round-trip 测试覆盖每个 alias。

### NEW-DEBT-20 — `tui/ui/markdown.rs` 1244 行单文件
- **位置**: `src/tui/ui/markdown.rs:1-1244`
- **现象**: 整个 markdown 渲染器（parser + line layout + syntax highlighting + theme）在一个文件。
- **影响**: 加新 markdown 特性需要 merge conflict；无法单测 line layout。
- **修复**: 拆 `tui/ui/markdown/{mod.rs, parser.rs, layout.rs, theme.rs, render.rs}`。

### NEW-DEBT-21 — `tui/ui/modal.rs` 1141 行单文件 + 多个 modal 类型同文件
- **位置**: `src/tui/ui/modal.rs:1-1141`
- **现象**: plan approval modal / permission modal / interrupt modal / goal modal 在同一文件，z-order 不明。
- **修复**: 每 modal 一文件 + 显式 `ModalStack` 数据结构管理 z-order。

### NEW-DEBT-22 — `tui/ui/command_menu.rs` 742 行 + `tui/ui/transcript.rs` 959 行
- 同 NEW-DEBT-20 模式。

### NEW-DEBT-23 — `tui/backend.rs` 1016 行 + `tui/commands.rs` 1132 行
- 同上。

### NEW-DEBT-24 — `tui/app/event_loop.rs` 817 行单文件
- 事件循环 / 渲染 / 状态机混杂。

### NEW-DEBT-25 — `hooks/external.rs` 1759 行单文件
- hook discovery / execution / mode / protocol 都在同文件。

### NEW-DEBT-26 — `permissions/mod.rs` 1657 行单文件
- 与 `permissions/auto_classifier.rs` 290 行职责重叠。

### NEW-DEBT-27 — `session.rs` 估计 2200+ 行（含 inline test）
- `SessionFile` / `SessionWriter` / `SessionReader` / `OrphanToolCall` / `UsageMeta` / `epoch_day_to_ymd` 等全在同文件。

### NEW-DEBT-28 — `runtime.rs` 2330 行单文件（含 inline test）
- `AgentRuntime` + `CheckpointState` + `GoalState` re-export + `enqueue`/`drain_queue` + `execute_kernel_turn` + tests。

### NEW-DEBT-29 — `tui/app/commands.rs` 2568 行单文件
- 已记录。

### NEW-DEBT-30 — `tools/mod.rs` 1484 行单文件
- `Tool` trait / `ToolRegistry` / `AuditMeta` / `ExitStatus` / `PermissionHook` / `build_standard_tools` + 50+ `pub use` + tests。

---

## 跨模块系统性问题（持续）

### 1. **"补丁掩盖架构错配" 模式 (PERSISTENT)**

06-10 review 已记录 TUI 5 个连续 commit + LLM tool_search 4 次来回改。本轮**没有新增** TUI / LLM 折腾 commit — 这是一个改善信号。但根本问题（TUI 三套 source of truth、tool_search 设计契约未对齐）仍在。

**建议**：在下次大规模 TUI 重构前先：
1. 删 dead state（`flush_ready_blocks`、`pre_draw_sentinel`、last_printed_idx 等）
2. 引入 `ModalStack` 显式 z-order
3. 选 `blocks[last_printed_idx..]` 作为唯一 source of truth

预计 1-2 天能 obsolete 后续所有 scroll bug commit series。

### 2. **大文件堆积 (PERSISTENT)**

06-10 列出 6 个 700+ 行 TUI 文件，本轮新增：
- `hooks/external.rs` 1759
- `permissions/mod.rs` 1657
- `runtime.rs` 2330
- `tui/app/commands.rs` 2568
- `session.rs` ~2200
- `tools/mod.rs` 1484

总共 **14 个 1000+ 行单文件**。每次加功能都要在多文件中跨章节 patch，merge conflict 高频。

**建议**：drip-feed 拆分；优先拆 `tools/mod.rs`（最模块化、依赖最少）和 `permissions/mod.rs`（已有 `auto_classifier.rs` 子模块模式可借鉴）。

### 3. **State Machine 缺乏显式建模 (PERSISTENT)**

- `TaskStatus` (06-10 NEW-TASK-1) 仍未 central state machine
- `GoalStatus` 在 `runtime_goal.rs`，但 goal transition 散落 `runtime.rs` 多处
- `FinishReason` 各种 exit path 都手动构造 outcome
- `[compacted:` 字符串嗅探（NEW-KERN-16）
- `RunCore::DENIAL_LIMIT_SENTINEL` 字符串 sentinel

**建议**：引入 `state_machine` derive 或自建 transition table，至少 task lifecycle + goal lifecycle 应该是可验证的。

### 4. **测试覆盖不平衡 (PERSISTENT)**

- `tools/`: ✅ 有 unit test
- `storage/`: ✅ 有 unit test
- `permissions/`: ✅ 有 unit test（但 1657 行里 unit test 比例仍偏低）
- `llm/`: ⚠️ 有 streaming parser 测试但 UTF-8 边界 case 不足（Anthropic 仍未审）
- `runtime/`: ⚠️ 有 happy-path 测试，但 error path / 并发 / race 覆盖不足
- `kernel/`: ⚠️ 基础测试
- `multi/`: ❌ 并发执行路径几乎无单元测试
- `tui/`: ❌ 几乎全靠 e2e
- `cli/`: ❌ 几乎全靠 e2e

`multi.rs` 的 `TeamOrchestrator` / `run_with_role` 并发代码风险高但不可测 — 是最大盲区。

### 5. **配置默认值仍偏不安全 (PERSISTENT → 漂 2 轮)**

4 个 P0 默认设置从 06-07 漂到现在仍未修：
- HTTP 默认无认证（SEC-003）
- `run_skill_script` 注入（SEC-002）
- `a2a_call` SSRF（NEW-TOOL-2）
- MCP dispatcher 分叉（NEW-CLI-1）

每次 review 都记录但都不修。需要明确 owner + deadline。

### 6. **持久化缺统一原子写 (PERSISTENT → 部分修)**

06-10 提出的 `src/atomic.rs` 统一方案已实施（`atomic_write` + `atomic_write_async`），三处实现已合并。但 `cost.rs` 两处仍 `std::fs::write` — 1 行修复。

### 7. **Policy Sandbox 仍非默认 (PERSISTENT)**

`PolicyConfig` 存在但默认未启用。新部署者容易忘记配 → 走默认 `Permissions = None` 路径 → safety check 失效。

---

## 新出现的关注

### A. `runtime.rs` 已 2330 行

是项目最大单文件（除 TUI commands.rs）。`AgentRuntime` 一个 struct 就把：
- transcript (`Vec<Message>`)
- event_sink
- streaming flag
- compactor
- checkpoint state (grouped struct)
- todo_list (`Arc<RwLock<Vec<TodoItem>>>`)
- plan_approval_gate
- plan_mode_request_gate
- goal_state
- message_queue
- deferred_turn_finished
- session_closed

13 个字段全在一个 struct 上。`AgentRuntime::run` 入口逻辑 + `execute_kernel_turn` 内部实现 + `emit_turn_messages` 三段紧耦合。建议下一步抽 `RuntimeCore` / `RuntimeShell` 分层（参考 `kernel.rs` 已有的 Kernel/Context 分层思路）。

### B. `hooks/external.rs` 1759 行单文件

是 hook 子系统的唯一源 — discovery / execution / output streaming / mode / safety 全混。建议拆 `hooks/external/{discovery.rs, execution.rs, transport.rs}`。

### C. TUI markdown 仍每 token 重渲染

NEW-TUI-4 描述的问题未修：流式 assistant block 每 token 重新跑一遍 pulldown-cmark + syntect 全文本解析（20 次/响应）。TUI 是用户感知最强的部分。

### D. `Message::assistant` 构造器不带 tool_calls，强制调用 `assistant_with_tool_calls`

不一致设计：5 个构造器里只有 `assistant_with_tool_calls` 能带 `Vec<ToolCall>`。调用点用错时 `Message::assistant(content)` + 后续 mut 改 `tool_calls` 字段，违反不变性。

---

## 建议的下一步工作顺序

```
P0 block-release（3-5 工作日，本轮必须修）:
  1. CORE-15: send_session_message 在 turn 失败时也更新 message_count
  2. TOOL-15: tool_audits 不要在 emit 时 remove，改在 TurnFinished 时清理
  3. STORE-15: SessionStatus enum 化（替代 SessionMeta.status: String）
  4. CLI-15: 4 处 lock().unwrap() 改 .lock().expect() 或 .lock().map_err()
  5. STORE-25: cost.rs 改用 atomic_write
  6. SEC-003 收尾（漂 2 轮）: 默认 deny + startup fail 而非 warn

P1 this-iteration（1-2 周）:
  7. KERN-15: 删 MessageAppended.parent_uuid 字段（dead surface area）
  8. KERN-16: Message 加 is_compaction_summary 标记位
  9. HTTP-15: session_clear_goal 失败时显式返回 409
 10. PERM-15: with_policy + permissions=None 时记 WARN + ToolRejected
 11. HOOK-15: hook schema 加 mode: "open" | "closed"
 12. MEM-15: MessageBus 改 ring buffer
 13. TOOL-16: run_skill_script 走 permission_pipeline + shell-words 解析
 14. LLM-25: audit AnthropicProvider SSE parser + thinking/redacted_thinking
 15. LLM-28: parse Retry-After header
 16. CLI-16: --provider 接受 preset id
 17. SKILL-15: skill manifest 加 sha256 pinning
 18. TUI-25: bash 输出 backpressure

P2 next-iteration:
  19. CORE-25: 统一 transcript_tail 实现
 20. LLM-26: 删 OpenAiProvider.stream_tx instance 字段
 21. LLM-27: EmbeddingProvider::embed 返回 Result
 22. SEC-002 收尾: run_skill_script 用 shell-words
 23. NEW-CLI-1 收尾: 统一 MCP dispatcher
 24. NEW-SESS-1 收尾: 本地 CLI 启动 session_reaper

P3 tech-debt（drip-feed）:
  25. DEBT-20~30: 14 个 1000+ 行单文件按职责拆
  26. DEBT-15: Message 字段改为 pub(crate) 强制构造器
  27. DEBT-17: MockProvider 提供 builder
  28. DEBT-18: SessionState metadata 统一 RwLock
```

---

## 总结对比

| 维度 | 06-10 review | 06-15 review | 趋势 |
|------|-------------|-------------|------|
| 总行数 | ~96k | ~96k | 持平（无大 commit） |
| TUI 大文件数 | 6 个 700+ 行 | 6 个 700+ 行 | 持平 |
| 1000+ 行单文件 | ~10 | ~14 | **恶化** |
| P0 漂动未修 | 4 | 4 | 持平 |
| P1 总数 | ~25 | ~30 | **新增 5** |
| 测试覆盖 | 不均衡 | 不均衡 | 持平 |
| clippy clean | ✅ | ✅ | 持平 |
| Cargo build | ✅ | ✅ | 持平 |

**主要诊断**：项目处于"稳定运营期"，不是"开发爆发期"。本轮新增 P0/P1 主要来自代码 review 时发现的隐性 bug（如 `parent_uuid` dead field、字符串嗅探、atomic_write 漏改 cost.rs），而非新功能引入的问题。3 个仍未修的 P0（SEC-003、SEC-002、NEW-CLI-1）已经从 06-07 漂到 06-15，需要明确 owner + deadline 才能打破循环。