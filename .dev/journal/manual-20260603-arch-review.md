# Architecture Review: Recursive v0.6.0

**Date**: 2026-06-03  
**Reviewer**: 小华 (AI Architecture Review)  
**Scope**: Full codebase review — architecture, code quality, security, correctness

---

## 🔴 严重问题（影响正确性）

### 1. `SideEffect` 机制名存实亡
- **位置**: `src/kernel.rs`, `src/runtime.rs`
- **描述**: `kernel.rs` 定义了 `SideEffect::BackgroundJob` 和 `ScheduleWakeup` 枚举，`TurnOutcome` 也有 `side_effects` 字段，但 `AgentKernel::run()` 永远返回 `side_effects: Vec::new()`（第339行）。
- **实际情况**: 后台任务调度通过 `WakeupSlot`（`Mutex`）和 `BackgroundJobManager` 两个独立共享状态完成，完全绕过了 `SideEffect` 协议。
- **影响**: `SideEffect` 是死代码；两套并行机制令人困惑；未来扩展容易走错路。
- **建议**: 选一套：要么让 `RunCore` 真正填充 `SideEffect` 并由 `Runtime` 处理，要么删除 `SideEffect` 枚举，只保留 `WakeupSlot`/`BackgroundJobManager`。

### 2. `Permission::Unknown` 默认放行
- **位置**: `src/tools/mod.rs` (`invoke_with_audit`)
- **描述**: 工具没有显式声明权限时，`Unknown` 在非 interactive 模式下被隐式当作 `Allowed` 处理。这个行为只在注释里提到，调用方完全不知情。
- **影响**: 新增工具若忘记声明权限，默认就是放行，是反直觉的安全默认值。
- **建议**: 改为由 `PermissionMode` 决定缺省行为（`strict` 模式下 Unknown = Deny），或者让 `Tool` trait 强制要求声明 `required_permission`。

---

## 🟠 架构问题（影响可维护性）

### 3. `TurnContext` 与 `RunCore` 近乎 1:1 拷贝
- **位置**: `src/kernel.rs` (TurnContext, 11字段), `src/run_core.rs` (RunCore, 12字段)
- **描述**: `AgentKernel::run()` 的唯一工作几乎是将 `TurnContext` 的字段一一解包，再组装成 `RunCore`。两者结构几乎相同，却是两个独立类型。
- **影响**: 每次增减字段都需要改两处；样板代码多；没有类型校验保证转换正确。
- **建议**: 让 `RunCore` 直接接受 `TurnContext`（用 `From<TurnContext>` 实现转换），或合并两者。

### 4. `HookRegistry` 在 `RunCore` 里使用生命周期借用 `&'a`
- **位置**: `src/run_core.rs`, 第88行
- **描述**: 其他所有跨上下文状态（LLM、tools、hooks in kernel）都用 `Arc`，但 `RunCore.hooks` 是 `&'a HookRegistry`。
- **影响**: 要求 kernel 的生命周期不能短于 RunCore，阻止 RunCore 被 `spawn`；与整体 Arc 风格不一致。
- **建议**: 改为 `Arc<HookRegistry>`（HookRegistry 已经是 Clone，改动不大）。

### 5. intra-turn 紧凑检测用字符串 hack
- **位置**: `src/kernel.rs`, 第325-330行
- **描述**: 检测是否发生了 intra-turn compaction 靠 `inner.messages[0].content.contains("[compacted:")`，这是字符串匹配而非类型安全标记。
- **影响**: compaction 消息格式一旦改变就静默失效；无法区分用户消息里恰好包含该字符串的情况。
- **建议**: 在 `RunInnerOutcome` 里加一个 `bool compacted` 标志，或用 `Message` 上的专用 marker 字段。

### 6. `exploring_plan_mode: Arc<AtomicBool>` 穿越 Kernel/Runtime 边界
- **位置**: `src/kernel.rs` (TurnContext), `src/run_core.rs` (RunCore)
- **描述**: Plan mode 状态作为 `Arc<AtomicBool>` 传入 `TurnContext`，违背了 Kernel 的"无状态"设计声明。
- **影响**: Kernel 实际上是有状态的（通过共享原子变量），这让并发场景的行为变得难以推理。
- **建议**: Plan mode 状态应在 `TurnOutcome` 里作为返回值传回，由 Runtime 维护，而不是通过入参共享引用。

### 7. `SharedMemory` 与 `memory/` 模块重复实现
- **位置**: `src/multi.rs` (SharedMemory), `src/memory/`
- **描述**: `multi.rs` 里有独立的 `SharedMemory`（基于 `RwLock<HashMap>`），而 `src/memory/` 是一个独立的记忆模块。两者都提供 key-value 存储能力。
- **影响**: multi-agent 场景下有两套记忆，状态可能分裂；不清楚应该用哪个。
- **建议**: 统一到 `memory/` 模块，`SharedMemory` 成为其一个实现或别名。

---

## 🟡 代码质量问题（影响可读性/可扩展性）

### 8. 单文件过大（超过 1500 行）

| 文件 | 行数 | 问题 |
|------|------|------|
| `tui/app/commands.rs` | 2238 | TUI 命令 God Module，所有 slash 命令处理都在一个文件 |
| `session.rs` | 2330 | 持久化 + transcript + audit + rewind 全混在一起 |
| `runtime.rs` | 2052 | 运行时核心 + 目标状态(GoalState) + checkpoint 混合 |
| `main.rs` | 1739 | 所有 CLI 子命令（run/repl/loop/http/tools/sessions）都在这里 |
| `mcp.rs` | 1921 | MCP 客户端 + 服务器端代码混在一个文件 |
| `tools/facts.rs` | 1337 | 单个工具文件过大 |
| `tools/a2a.rs` | 1348 | A2A 协议工具过大 |

- **建议**: 
  - `session.rs` → 拆分为 `session/mod.rs`, `session/persistence.rs`, `session/audit.rs`, `session/rewind.rs`
  - `main.rs` → 每个子命令一个文件（已有 `cli/` 目录，可以继续拆）
  - `mcp.rs` → `mcp/client.rs` + `mcp/server.rs`
  - `runtime.rs` → `runtime/mod.rs` + `runtime/goal.rs`（已有 `runtime_goal.rs`，但 GoalState 的 re-export 还在 runtime.rs 里）

### 9. 废弃类型的 `#[allow(dead_code)]`
- **位置**: `src/run_core.rs`, 第96行
- **描述**: `permission_mode` 字段有 `#[allow(dead_code)]` 注解，说明这个字段存储了但从未使用。
- **建议**: 要么使用它，要么移除它。

### 10. `AgentKernel` 注释与实现不一致
- **位置**: `src/kernel.rs`, 第163-167行
- **描述**: 注释说 "NOTE: The `run()` method is NOT implemented in this goal"，但 `run()` 方法实际上是实现的（第287行）。
- **建议**: 清理过时注释，这是 Goal 219 阶段遗留的 TODO 注释未清理。

### 11. `tools/mod.rs` 中 `ToolRegistry` 暴露了内部实现细节
- **位置**: `src/tools/mod.rs`
- **描述**: `ToolRegistry::transport()` 方法返回内部 Arc，用于测试中的 `ptr_eq` 比较。这暴露了内部实现细节。
- **建议**: 提供更高级别的测试接口，而不是暴露内部 Arc。

---

## ✅ 架构亮点（值得保留/推广）

1. **Kernel/Runtime 分层** — 将无状态执行内核与有状态会话运行时分开，设计思路正确
2. **Tool trait 的正交性** — 新增工具只需实现 trait + 注册，不改 agent 循环，符合开闭原则
3. **`FinishReason` 作为数据而非错误** — 预算超出、卡住等都是 Ok 返回值，让流程控制更清晰
4. **`AuditMeta` + `ToolSideEffect` 分类** — 工具调用审计和副作用分类设计完备
5. **Invariant 文档 (AGENTS.md)** — 把不可破坏的设计约束写成明文合同，非常有助于 AI 自我改进循环
6. **`cargo clippy -D warnings` 强制执行** — lint 门禁维护了代码质量下限

---

---

## 🔴 严重问题（续）

### 12. `compact.rs` 结构化紧凑摘要中步骤号硬编码为 "N"
- **位置**: `src/compact.rs`, 第63行
- **描述**: `render_structured()` 生成摘要消息时用了 `"[Context compacted at step N]"`，`N` 是字面量字符串，而非实际步骤号。
- **影响**: 摘要中的步骤信息完全无意义，调试时无法知道紧凑发生在哪个步骤。
- **建议**: 将实际步骤数传入（或用消息数量代替）。

### 13. `compact.rs` 的结构化紧凑输出格式与 `kernel.rs` 的检测逻辑不一致
- **位置**: `src/compact.rs` 第63行 vs `src/kernel.rs` 第326-329行
- **描述**: 
  - 自由文本紧凑输出格式: `"[compacted: X messages → Y chars]"`
  - 结构化紧凑输出格式: `"[Context compacted at step N]"`
  - `kernel.rs` 检测逻辑: `content.contains("[compacted:")`
- **影响**: 如果 LLM provider 支持 `complete_structured`，则使用结构化路径，其输出不包含 `"[compacted:"` 字符串，导致 `kernel.rs` 的前置摘要消息插入逻辑**静默失效**。
- **建议**: 统一两种格式，或在 `RunInnerOutcome` 里用布尔标志代替字符串检测。

---

## 🟠 架构问题（续）

### 14. `expect()` 出现在非测试生产代码中（违反 Invariant #5）
- **位置**: `src/compact.rs` 第95行
- **描述**: `serde_json::from_str(Self::COMPACT_SCHEMA).expect("COMPACT_SCHEMA is valid JSON")` 出现在 `async fn try_structured_compact` 中（非测试代码）。
- **现状**: 虽然 `COMPACT_SCHEMA` 是编译期常量，实际不会 panic，但这违反了项目的 Invariant #5："No `unwrap()`/`expect()` in non-test code"。
- **建议**: 用 `const` 加 `static` 配合 `once_cell::sync::Lazy` 在程序启动时初始化，或改为在 `build()` 阶段一次性解析。

### 15. `HookEvent<'a>` 使用生命周期参数，但 `external.rs` 中定义了独立的 `HookEvent` 枚举（无生命周期）
- **位置**: `src/hooks/mod.rs` (内部 trait 用 `HookEvent<'a>`)、`src/hooks/external.rs` (外部 hook 协议用独立 `HookEvent`)
- **描述**: 内部 `Hook::on_event` 接受 `HookEvent<'a>`（含生命周期的借用版），外部 hook 协议用独立的 `HookEvent` 枚举（无生命周期，用于 JSON 序列化）。两者名称相同但在不同模块。
- **影响**: 有命名冲突风险，`pub use external::HookResult` 同时被 re-export，容易混淆。
- **建议**: 重命名外部协议类型（如 `HookEventKind`），或将外部 `HookEvent` 改名为 `ExternalHookEvent`。

### 16. `HookEvent::SessionEnd` 在 `AgentRuntime` 中从未被 dispatch
- **位置**: `src/hooks/mod.rs` 注释第109行
- **描述**: 注释明确写明 "The `AgentRuntime` does not yet dispatch `SessionEnd`"，意味着注册了 `SessionEnd` hook 的用户代码永远不会被调用。
- **影响**: API 承诺与实现不符；外部 hook 配置文件可以监听 `sessionEnd` 但永远收不到回调。
- **建议**: 要么在 `AgentRuntime::run()` 末尾补发 `SessionEnd`，要么在文档中标记为 deprecated/not-yet-implemented。

### 17. `multi.rs::MemoryEntry` 与 `memory::MemoryEntry` 同名但不同结构
- **位置**: `src/multi.rs` 第22行, `src/memory/mod.rs` 第50行
- **描述**: 两个 `MemoryEntry` 结构体字段不同（multi 版本多了 `author`、`timestamp`；memory 版本多了 `tags`、`ts`）。
- **影响**: 代码阅读时极易混淆，`use recursive::multi::MemoryEntry` vs `use recursive::memory::MemoryEntry` 是两种完全不同的东西。
- **建议**: 将 `multi.rs` 的类型重命名为 `SharedMemoryEntry` 或移入 `multi/memory.rs` 子模块。

---

## 🟡 代码质量问题（续）

### 18. `hooks/external.rs` 单文件 1760 行
- **位置**: `src/hooks/external.rs`
- **描述**: 包含了 JSON 协议定义、进程启动逻辑、HTTP hook、prompt hook、env var 插值、事件名称匹配等完全不同的职责。
- **建议**: 拆分为 `hooks/external/process.rs`、`hooks/external/http.rs`、`hooks/external/prompt.rs`、`hooks/external/protocol.rs`。

### 19. `ToolTimingHook` 用 `Mutex<HashMap>` 跟踪 PreToolCall 计时，但 PostToolCall 已有 `duration_ms`
- **位置**: `src/hooks/mod.rs`, `ToolTimingHook`
- **描述**: `ToolTimingHook` 在 `PreToolCall` 时记录时间戳，在 `PostToolCall` 时计算耗时。但 `PostToolCall` 事件已经包含了 `duration_ms` 字段，`ToolTimingHook` 的 `PreToolCall` 处理是多余的（同时还有死锁风险：如果同名工具并发调用，HashMap 会记录错误的开始时间）。
- **建议**: `ToolTimingHook` 直接用 `PostToolCall.duration_ms`，删除 `start_times` HashMap。

---

---

## 🔴 严重问题（HTTP API）

### 20. `list_sessions` 顺序锁定每个 session 的 runtime Mutex
- **位置**: `src/http/handlers.rs`, `list_sessions()` 函数
- **描述**: 每次 `GET /sessions` 请求会对**所有**现存 session 顺序调用 `lock().await`，只是为了统计消息数量。如果某个 session 正在执行 agent 循环，这次 `list_sessions` 会一直阻塞。
- **影响**: 并发性能严重问题。多 session 场景下 `list_sessions` 可能阻塞数秒甚至更长；反过来，一个慢的 session 会"毒化"整个会话列表 API。
- **建议**: 在 `SessionState` 里缓存消息计数（agent 每次 append 时更新原子计数器），`list_sessions` 直接读取不需要锁。

---

## 🟠 架构问题（HTTP API）

### 21. `finish_reason` 用 Debug 格式序列化到 HTTP 响应
- **位置**: `src/http/handlers.rs`, 第122行
- **描述**: `format!("{:?}", outcome.finish_reason)` 用 Rust Debug 格式生成 HTTP API 的 `finish_reason` 字段。Debug 输出不是稳定 API，格式随 `FinishReason` 枚举结构变化而变化。
- **影响**: SDK/客户端不能安全地解析和匹配这个字段；API 合约不稳定。
- **建议**: 为 `FinishReason` 实现 `Display` 或用 `serde_json::to_string` 并加 `rename_all = "snake_case"`。

### 22. `generate_session_id()` 手动实现 ID 生成，而项目已有 UUID v7
- **位置**: `src/http/handlers.rs`, 第138-149行
- **描述**: 用了 BLAKE3 hash + 原子计数器 + 时间戳来生成 16 字符 session ID，实现了约 40 行代码。而项目在 `AuditMeta` 里已经使用了 UUID v7 (`uuid::Uuid::now_v7()`)，它是时序的、全局唯一的。
- **建议**: 直接用 `uuid::Uuid::now_v7().to_string()` 或返回完整 UUID。

### 23. `format_timestamp()` 手动实现 UTC 日期格式化
- **位置**: `src/http/handlers.rs`, 第152-201行
- **描述**: 为了避免引入 `chrono`，手写了约 50 行的 UTC 日期格式化代码，包括一个自定义的闰年判断和 epoch-to-ymd 转换函数。
- **影响**: 自定义日期代码有 bug 风险（如闰秒处理）；`time` crate 已是依赖链中的常见项。
- **建议**: 如不想增加依赖，可以直接使用 `unix timestamp` 字符串，或将时间戳接口统一为 Unix 秒数（整数）。

---

## 🟡 代码质量问题（TUI）

### 24. `tui/app/commands.rs` 2238 行，单个 `impl App` 块
- **位置**: `src/tui/app/commands.rs`
- **描述**: 整个文件是一个 `impl App { ... }` 块，包含：键盘事件路由、每个 slash 命令实现、modal 键处理、history search 逻辑、@file 自动完成、权限 modal 处理。
- **建议**: 按职责拆分：
  - `commands/key_routing.rs` — 顶层键盘路由
  - `commands/slash.rs` — slash 命令分发
  - `commands/modal.rs` — modal 键处理
  - `commands/autocomplete.rs` — @file 和历史搜索
  - `commands/permission.rs` — 权限 modal

---

## 🔴 严重问题（执行内核 run_core.rs）

### 25. `ProviderTruncated` 直接违反 Invariant #7
- **位置**: `src/run_core.rs`, 第693行；`src/error.rs` 第49行
- **描述**: 当 LLM 返回 `finish_reason = "length"` 时，代码先 emit `TurnFinished` 事件（正常完成信号），然后立即 `return Err(Error::ProviderTruncated("length")))`。
- **影响**: 这是对 Invariant #7 的直接违反——"Finish reasons are data, not errors"。`ProviderTruncated` 导致 `runtime.run()` 返回 `Err`，transcript 可能未保存，自我改进循环的自动续期门控失效。`FinishReason::ProviderStop` 变体已存在，应当使用它。
- **建议**: 将 `return Err(Error::ProviderTruncated(...))` 改为 `return Ok(RunInnerOutcome { finish_reason: FinishReason::ProviderStop("length".into()), ... })`。

### 26. `"ERROR_DENIAL_LIMIT:"` 用作哨兵字符串传递错误信号
- **位置**: `src/run_core.rs`, 第596行
- **描述**: 工具调用返回字符串 `"ERROR_DENIAL_LIMIT:"` 作为权限拒绝限制的信号，run_inner 循环用字符串比较来检测此条件。
- **影响**: 类型不安全，字符串变化无编译时检查，语义不清晰。
- **建议**: 改为在工具返回类型中携带错误变体，或在 tool dispatch 层返回结构化结果而非纯字符串。

### 27. `shell.rs` 非测试代码中使用 `expect()`（违反 Invariant #5）
- **位置**: `src/tools/shell.rs`, 第111-112行
- **描述**: `child.stdout.take().expect("stdout piped")` 和 `child.stderr.take().expect("stderr piped")` 出现在生产代码中。
- **建议**: 改为 `ok_or_else(|| Error::Tool { ... })` 返回 Result。

---

## 🟠 架构问题（执行内核）

### 28. `run_inner` 有 7+ 个早期返回，每次都重复构造 `RunInnerOutcome`
- **位置**: `src/run_core.rs`, `run_inner()` 方法
- **描述**: `RunInnerOutcome` 有 8 个字段，每个早期返回点都需要完整构造。代码重复、容易遗漏字段。
- **建议**: 提取 `make_outcome(self, finish_reason, final_message, ...) -> RunInnerOutcome` 辅助方法，所有早期返回使用同一构造点。

### 29. 并行工具调用结果查找是 O(n²)
- **位置**: `src/run_core.rs`, `execute_tool_calls()`, 约第423行
- **描述**: 并行 batch 完成后，通过 `.find(|(id, ...)| id == &pc.id)` 对每个 `pc` 进行线性扫描，导致 O(n²) 复杂度。
- **影响**: 并行工具调用数量通常较小（< 10），实际影响有限，但设计不佳。
- **建议**: 用 `HashMap<id, result>` 代替线性扫描。

---

## 已读模块清单

- [x] `src/agent/mod.rs` — 类型定义（agent loop 已迁移到 runtime/kernel）
- [x] `src/kernel.rs` — Kernel/TurnContext/TurnOutcome
- [x] `src/run_core.rs` — 无状态 ReAct 执行内核
- [x] `src/runtime.rs` — 有状态运行时
- [x] `src/session.rs` — session 持久化（1/4 扫描）
- [x] `src/multi.rs` — multi-agent 编排（1/4 扫描）
- [x] `src/tools/mod.rs` — Tool trait、ToolRegistry、权限
- [x] `src/permissions/mod.rs` — 权限系统
- [x] `src/compact.rs` — 紧凑策略
- [x] `src/hooks/mod.rs` — Hook trait 和内部 registry
- [x] `src/hooks/external.rs` — 外部进程/HTTP/prompt hook（结构扫描）
- [x] `src/storage/mod.rs` — 存储后端抽象
- [x] `src/memory/mod.rs` — 向量记忆层
- [x] `src/http/handlers.rs` — HTTP API 处理层（1/4 扫描）
- [x] `src/tui/app/commands.rs` — TUI 命令（结构扫描）
- [x] `src/config.rs` — 配置加载（1/4 扫描）
- [x] `src/run_core.rs` — 核心执行逻辑（深度扫描）
- [x] `src/tools/shell.rs` — shell 工具实现
- [x] `src/error.rs` — 错误类型定义
- [x] `src/mcp.rs` — MCP 客户端（关键路径扫描）
- [x] `src/llm/openai.rs` — OpenAI provider（结构扫描）
- [x] `src/llm/anthropic.rs` — Anthropic provider（深度扫描）
- [x] `src/session.rs` — session 持久化（深度扫描）
- [x] `src/tui/backend.rs` — TUI 后端 worker loop（深度扫描）

---

## 🔴 严重问题（MCP 客户端）

### 30. `.mcp.json` 中配置的 `env` 字段被静默丢弃（真实 Bug）
- **位置**: `src/mcp.rs`, `load_mcp_discovery_config()` 第1506-1515行 + `spawn_stdio()` 第272-281行
- **描述**:
  1. `McpServerConfig`（从 `.mcp.json` 解析）有 `env` 字段
  2. 转换为 `McpServer` 时 `config.env` 被静默丢弃（代码里只转了 `command/args/url`）
  3. `McpServer` struct 没有 `env` 字段
  4. `spawn_stdio` 启动子进程时没有设置任何 env vars
- **影响**: 任何在 `.mcp.json` 里通过 `env` 字段配置 API key 等环境变量的 MCP server，其 env 设置完全不生效。这是一个**静默失效**的 Bug，用户不会收到任何错误提示。
- **建议**: 为 `McpServer` 添加 `env: Option<HashMap<String, String>>` 字段，并在 `spawn_stdio` 中用 `.envs()` 设置。

---

## 🟠 架构问题（MCP 客户端）

### 31. `McpTransport::HttpSse` 的 `post_url` 类型应为 `String` 而非 `Option<String>`
- **位置**: `src/mcp.rs`, `McpTransport::HttpSse`, 第174-182行
- **描述**: `post_url` 在初始化后一定为 `Some`（SSE 握手失败时整个 spawn 就 Err 了），但仍然是 `Option<String>`，每次工具调用都需要处理 None 分支。
- **建议**: 将 `post_url` 改为 `String`，在 `spawn_http_sse` 中初始化时直接存入。

### 32. `McpServerConfig` 与 `McpServer` 两个重复结构体（除了 `env` 字段不同）
- **位置**: `src/mcp.rs`, 第36-61行
- **描述**: 两个结构体几乎完全相同（`McpServerConfig` 有 `env`；`McpServer` 有 `name`），只因为格式略有差异（一个是配置文件格式，一个是运行时格式）就定义了两个类型。
- **建议**: 可以合并为一个 `McpServer` 并加可选的 `name` 字段，或者利用 serde 的 `flatten` 来减少重复。

---

## 🟠 架构问题（Anthropic provider）

### 33. SSE 流式解析先缓冲全量再处理（伪流式）
- **位置**: `src/llm/anthropic.rs`, `parse_sse_stream()`, 第473行
- **描述**:
  ```rust
  let reader = resp.text().await?;  // 一次性缓冲全部响应体
  for line in reader.lines() {       // 然后才逐行解析
  ```
  `resp.text().await?` 会等到服务端关闭连接后才返回。虽然函数内部会向 `stream_tx` 发送 token，但这些 token 只有在响应完全到达本地后才会发出，不是真正的增量流式。
- **影响**: 对于长响应（> 1000 tokens），用户看不到实时的打字机效果，而是等待所有 token 后一次性刷出。同时会占用大量内存存储整个响应体。
- **建议**: 使用 `resp.bytes_stream()` 结合 `futures::StreamExt` 逐块处理，实现真正的增量流式解析。

---

## 🟡 代码质量（session.rs）

### 34. `session_id` 时间戳精度只到秒，可能碰撞
- **位置**: `src/session.rs`, `SessionWriter::create_with_tools()`, 第536行
- **描述**:
  ```rust
  let session_id = format!("{}-{}", filesystem_safe_timestamp(), slug);
  ```
  `filesystem_safe_timestamp()` 最小粒度为秒（格式 `YYYY-MM-DDTHH-MM-SSZ`）。同一秒内在同一 workspace 创建两个 session 会生成相同的 `session_id`，导致 `create_dir_all` 复用已有目录并覆盖 `.meta.json`。
- **影响**: 在压力测试或快速连续 resume 场景下，session 数据可能被静默覆盖。
- **建议**: 在时间戳后追加一个 UUID v4 的短 hex（如 8位），确保唯一性。

### 35. `hash_tool_specs` 序列化失败时静默返回空字符串
- **位置**: `src/session.rs`, `hash_tool_specs()`, 第137行
- **描述**:
  ```rust
  let canonical = serde_json::to_string(&map).unwrap_or_default();
  let hash = blake3::hash(canonical.as_bytes());
  ```
  若 `BTreeMap<String, Value>` 序列化失败（几乎不可能，但 panic-safe），`canonical` 为空字符串，所有工具的 hash 都是 `blake3("")`，导致每次 resume 都能通过 tool_registry 校验——完全绕过了漂移检测。
- **影响**: 严重情况下可以用错误的工具注册表继续 resume，导致不一致行为。
- **建议**: 改为 `serde_json::to_string(&map).map_err(|e| crate::Error::Other(...))?`，让调用方传播错误。

---

## 🟡 代码质量（tui/backend.rs）

### 36. 多处 `Arc::try_unwrap(...).expect(...)` 违反 Invariant #5
- **位置**: `src/tui/backend.rs`, 第404-406行、448-450行、649-651行等
- **描述**:
  ```rust
  let mut recovered = Arc::try_unwrap(rt_shared)
      .expect("single owner after task end")  // 违反 Invariant #5
      .into_inner();
  ```
  当 tokio task 因 panic 被 abort 后，`Arc` 引用计数不一定为 1，`try_unwrap` 会返回 `Err`，触发 `.expect()` 导致整个 TUI 进程崩溃。
- **影响**: 任何 agent turn 内部的 panic 都会向上传播为 TUI 进程崩溃，而不是优雅的错误恢复。
- **建议**: 改用 `match Arc::try_unwrap(rt_shared) { Ok(m) => m.into_inner(), Err(arc) => arc.lock().await.clone() }`，或者使用 `Arc<tokio::sync::Mutex<_>>` 配合 `lock().await` 直接访问。

### 37. `wait_for_cancel` 是 100ms 忙轮询（中断响应性问题）
- **位置**: `src/tui/backend.rs`, 第669-675行
- **描述**:
  ```rust
  pub async fn wait_for_cancel(flag: Arc<AtomicBool>) {
      loop {
          if flag.load(Ordering::SeqCst) { return; }
          tokio::time::sleep(Duration::from_millis(100)).await;
      }
  }
  ```
  用户点击 Ctrl+C 后，平均需要等待 50ms、最多 100ms 才能中断 turn。若 LLM 在流式输出，这 100ms 延迟用户会明显感受到。
- **建议**: 改用 `tokio::sync::Notify`：`cancel_flag` 用 `Arc<Notify>` 替代，触发时 `notify_waiters()`，等待方 `notify.notified().await`，延迟降至接近 0。

### 38. `ToolResult.success` 用字符串前缀 `"ERROR: "` 判断成功失败（脆弱耦合）
- **位置**: `src/tui/backend.rs`, 第128-134行
- **描述**:
  ```rust
  AgentEvent::ToolResult { id, name, output, .. } => {
      let success = !output.starts_with("ERROR: ");
      Some(UiEvent::ToolResult { id, name, output, success })
  }
  ```
  工具输出以 `"ERROR: "` 开头被认为失败。如果工具的合法输出恰好以此前缀开头（如某工具读取一段以 ERROR: 开头的文件内容），TUI 会错误地标记为失败。这与 `run_core.rs` 中的相同哨兵字符串是同一个设计缺陷的不同体现。
- **建议**: 在 `AgentEvent::ToolResult` 中增加 `is_error: bool` 字段，由工具执行层填充，而不是在 UI 层做字符串解析。

---

## 🟡 代码质量（llm/mod.rs）

### 39. `RetryPolicy` 不重试 HTTP 429（rate limit）
- **位置**: `src/llm/mod.rs`, `RetryPolicy::backoff_for()`, 第70行
- **描述**:
  ```rust
  let is_transient = is_network_error || status.is_some_and(|s| (500..600).contains(&s));
  ```
  只重试 5xx 错误和网络错误。HTTP 429 (Too Many Requests) 是几乎所有 LLM API 的限流响应，应该带退避重试，但目前会立即作为永久错误抛出。
- **影响**: 在 self-improve 循环或高频调用场景下，如果触发 provider 的 RPM/TPM 限制，agent 会以错误终止整个 run，而不是等待后重试。
- **建议**: 在 `is_transient` 判断中加入 `s == 429`，并为 429 使用更长的退避时间（如解析 `Retry-After` 响应头）。

---

## 🟡 代码质量（tools/facts.rs）

### 40. `find_duplicate` 返回类型歧义（相同 `Option<String>` 表示两种不同结果）
- **位置**: `src/tools/facts.rs`, `find_duplicate()` + `RememberFact::execute()`
- **描述**: `find_duplicate` 对"保留已有事实"和"需要超级取代已有事实"两种情况都返回 `Some(id)`，调用方被迫重复相同的 `text.len() > dup.text.len()` 判断。这是典型的 boolean blindness。
- **建议**: 定义 `enum DuplicateResult { KeepExisting(String), SupersedeExisting(String) }` 让类型系统表达意图。

### 41. `days_to_date` O(年份) 线性循环（自造时间计算轮子）
- **位置**: `src/tools/facts.rs`, 第399-407行
- **描述**: 从 Unix epoch 计算年月日时，用 `loop { total += days_in_year; year += 1 }` 逐年累加。若代码在 2500 年运行，循环 530 次。同一文件中 `session.rs` 已有等效但更精确的 `epoch_day_to_ymd`，存在代码重复。
- **建议**: 提取一个共享的 `epoch_secs_to_ymd` 工具函数到 `src/time.rs` 或 `src/paths.rs`，消除三处重复实现（`facts.rs`、`session.rs`、`http/handlers.rs`）。

---

## 🔴 严重问题（Hook 系统）

### 42. 大多数 HookEvent 变体在生产代码中从未被触发
- **位置**: `src/hooks/mod.rs` (事件定义) vs `src/run_core.rs` + `src/runtime.rs` (实际派发)
- **描述**: `HookEvent` 枚举定义了以下变体：
  - `SessionStart` — 从未派发
  - `SessionEnd` — 从未派发（注释中写 "AgentRuntime does not yet dispatch SessionEnd"）
  - `UserPromptSubmit` — 从未派发
  - `Stop` — 从未派发
  - `SubagentStart` / `SubagentStop` — 从未派发
  - `PostToolCallFailure` — 从未派发
  - `PermissionDenied` — 从未派发

  **实际只有 4 个变体被触发**: `PreToolCall`、`PostToolCall`、`PreCompact`、`PostCompact`。
- **影响**:
  1. 用户在 `.recursive/hooks.json` 中配置的 `sessionStart`/`sessionEnd`/`userPromptSubmit` 钩子完全无效，没有任何报错。
  2. Hook 文档与实现严重不一致，用户无从知晓哪些事件是"存在但未实现"的。
  3. 这对于自我改进循环尤其关键 — 许多用例依赖 `sessionStart`（注入初始上下文）和 `sessionEnd`（保存会话摘要）。
- **建议**:
  1. 在 runtime.rs 的 `enqueue()` 入口处派发 `SessionStart { goal }`
  2. 在 runtime 的 `run_resumed()` 或 `cmd_resume` 完成后派发 `SessionEnd { outcome }`
  3. 在 `run_core.rs` 的工具失败处理中派发 `PostToolCallFailure`
  4. 在权限拒绝时派发 `PermissionDenied`
  5. 至少在文档注释中标注哪些事件是"尚未实现"的

---

## 🟠 架构问题（multi.rs — 多智能体模块）

### 43. `AgentRole.allowed_tools` 声明但从不生效
- **位置**: `src/multi.rs`, `AgentRole` 结构体 + `AgentPool::run_with_role()`
- **描述**: `AgentRole` 有 `allowed_tools: Vec<String>` 字段，文档暗示可限制角色的工具访问。但 `run_with_role()` 每次都用 `AgentKernel::builder()` 创建空工具集（未调用 `.tools()`），`allowed_tools` 从未被读取或应用。`default_roles()` 里 `coder` 角色的 prompt 写着"write code, run tests, fix errors"，实际却没有任何工具可用。
- **影响**: multi-agent 场景下 agent 无法操作文件、执行 shell，与预期能力完全相反；`allowed_tools` 是误导性的死字段。
- **建议**: 在 `run_with_role` 中接受外部 `ToolRegistry`（或从 pool 读取），并用 `allowed_tools` 过滤工具列表。

### 44. `MessageBus.messages` 无上限增长（内存泄漏）
- **位置**: `src/multi.rs`, `MessageBus::send()` → `self.messages.write().await.push(msg)`
- **描述**: `MessageBus` 把所有历史消息追加到 `Vec`，没有上限、没有 TTL、没有 rotation。长时间运行的多智能体系统会无限积累消息历史。
- **建议**: 加 `max_history: usize` 选项并在超出时 ring-buffer 循环覆盖，或者定时 `clear()`。

### 45. `TurnContext.mailbox` 始终为 `None`（inter-agent 消息总线实际无效）
- **位置**: `src/multi.rs` (`run_with_role`) 和 `src/runtime.rs` (`execute_kernel_turn`)
- **描述**: `TurnContext` 有 `mailbox` 字段用于 agent 运行期接收消息，但两处都硬编码为 `None`。`MessageBus` 的 `send`/`subscribe` 基础设施完整，却因 mailbox 未接入而对 agent 执行路径完全不可见。agents 互发消息只能存档历史、无法触达运行中的对端。
- **影响**: inter-agent 实时通信功能是完整死代码，订阅机制全部无效。
- **建议**: 在 `run_with_role` 中用 `bus.subscribe(role_name).await` 获取 receiver 并传入 `TurnContext.mailbox`；在 `execute_kernel_turn` 中如 runtime 配有 bus 也同样接入。

### 46. `TeamOrchestrator` 委派任务串行执行（可并发）
- **位置**: `src/multi.rs`, `TeamOrchestrator::run()` 第 456 行
- **描述**: lead agent 把 N 个独立任务委派给 specialist agents，但 `for (role, task) in &delegations { pool.run_with_role(role, task).await?; }` 是顺序执行，无法利用并发。若 N=5、每个任务 10 秒，总等待 50 秒而非 10 秒。
- **建议**: 用 `futures::future::join_all` / `tokio::spawn` 并发执行独立委派，最后汇总结果。

### 47. `runtime.rs` 每 turn 全量克隆 transcript（O(n) 内存分配）
- **位置**: `src/runtime.rs`, `execute_kernel_turn()` 第 427 行
- **描述**: `TurnContext.messages = self.transcript.clone()` 在每次 `run()` 时对完整 transcript 做深拷贝。session 增长至数百条消息后，每 turn 的内存分配随对话长度线性增长，同时带来大量 `String` clone 开销。
- **建议**: 考虑用 `Arc<Vec<Message>>` + copy-on-write；或将 kernel 接口改为 `&[Message]`，避免所有权转移。

### 48. `CompactionBoundary` 事件 `summary_uuid: None`（摘要消息未被关联）
- **位置**: `src/runtime.rs`, `maybe_compact_cross_turn()` 第 390 行
- **描述**: `CompactionBoundary` 事件的 `summary_uuid` 字段始终为 `None`，不链接到紧接其后发出的摘要 `MessageAppended` 事件。SessionPersistenceSink 等下游消费者无法通过事件直接找到对应的摘要消息。
- **建议**: 在生成摘要消息前分配 UUID，`CompactionBoundary.summary_uuid` 和 `MessageAppended.parent_uuid` 都填同一个 UUID。

---

## 🟠 架构问题（checkpoint.rs / sub_agent.rs）

### 49. `checkpoint.rs` 在异步上下文中直接调用阻塞 git 子进程（blocking I/O）
- **位置**: `src/checkpoint.rs` (所有 `git_cmd()...output()` 调用) + `src/runtime.rs` (`snapshot_pre_turn` / `snapshot_post_turn`)
- **描述**: `snapshot_for_session()` 内部连续调用 4-5 次 `std::process::Command::output()`（同步阻塞调用），而这些函数从 `async fn run()` 中直接调用（没有 `tokio::task::spawn_blocking` 包裹）。每次 turn 做两次 snapshot（pre + post），共 8-10 次阻塞 subprocess。
- **影响**: 每次 git subprocess 调用都会阻塞当前 tokio 工作线程，降低并发吞吐；若文件系统较慢（网络挂载、Docker volume），每 turn 增加数百毫秒延迟。
- **建议**: 将 `ShadowRepo` 的阻塞操作封装进 `tokio::task::spawn_blocking`，或者使用 `tokio::process::Command`（异步版本）。

### 50. `sub_agent.rs` 中 `step_events_tx: None` 导致子 Agent 事件对父不可见
- **位置**: `src/tools/sub_agent.rs`, `execute()` 中 `TurnContext { step_events_tx: None, ... }`
- **描述**: 子 Agent 执行时，`step_events_tx` 被硬编码为 `None`，所有 tool call / message 事件都不会流向父 Agent 的 `EventSink`。TUI 和 HTTP API 的流式界面看不到子 Agent 的任何中间步骤。
- **影响**: 用户看不到子 Agent 的执行进展，调试困难；session JSONL 中子 Agent 的活动无法被 SessionPersistenceSink 记录。
- **建议**: 将父 Agent 的 `event_tx`（或一个带前缀的包装 sink）传入 `TurnContext.step_events_tx`；可以用 `AgentEvent::SubAgentStarted/Finished` 包裹。

### 51. `sub_agent.rs` 文档说"capped at parent's remaining budget"，但实际只 clamp(1, 100)
- **位置**: `src/tools/sub_agent.rs`, ToolSpec 注释 + 实现第 215 行
- **描述**: ToolSpec 文档写道"capped at parent's remaining budget"，但代码实际是 `clamp(1, 100)`，与父 Agent 的剩余步数预算完全无关。子 Agent 可消耗比父 Agent 剩余步数更多的 LLM calls。
- **建议**: 在 `SubAgent` 结构中加 `parent_remaining_steps: Option<usize>`，在 `execute()` 中取 min(max_steps, parent_remaining)；或删除误导性文档。

---

## 🟡 代码质量（http/handlers.rs）

### 56. `get_session` 在 Agent 运行中返回 `status: "idle"`（状态不准确）
- **位置**: `src/http/handlers.rs`, `get_session()` 第 313-316 行
- **描述**: session 状态只区分 `"plan_pending_approval"` 和 `"idle"` 两种情况。当 agent 正在执行（runtime Mutex 被锁住时），`try_lock()` 失败，返回 status = `"idle"` + 空 messages。实际上 agent 是 `"running"` 状态，SDK 消费者无法区分"空闲"和"正在运行中"。
- **建议**: 加第三种状态 `"running"`：当 `try_lock()` 返回 `Err` 时 status = `"running"`，让 API 消费者能准确感知状态。

### 57. `send_session_message` 丢弃 `RuntimeOutcome`，仅返回最后一条 assistant 消息
- **位置**: `src/http/handlers.rs`, 第 795-812 行
- **描述**: `run_result` 被解包为 `_outcome`（下划线），`finish_reason`、`total_usage`、`steps`、`llm_latency_ms`、`checkpoint_id` 全部丢弃。响应体只包含 transcript 最后一条 assistant 消息的文本，而非 `outcome.final_text`。
- **影响**: SDK/HTTP API 消费者无法得知 token 消耗、步骤数、checkpoint ID、终止原因，造成可观测性缺失；若 `final_text` 与 transcript 末尾消息不同，响应内容不准确。
- **建议**: 在 `SessionMessageResponse` 中加上 `finish_reason`、`usage`、`steps`、`checkpoint_id` 字段，并从 `_outcome` 中填充。

---

## 🟡 代码质量（tools/a2a.rs / llm/openai.rs）

### 52. `a2a.rs` async_mode 生成依赖 curl + python3 的 shell 一行脚本
- **位置**: `src/tools/a2a.rs`, 第 317-326 行
- **描述**: `async_mode=true` 时，工具返回一段需要 `curl` 和 `python3` 才能运行的 shell 脚本。但在 Docker sandbox / e2b 沙盒等受限环境中，这两个命令未必存在；脚本中 JSON 解析用 `python3 -c "..."` 也很脆弱，一旦 `base` URL 或 `task_id` 含特殊字符就会 quoting 出错。
- **建议**: 用原生 Rust + `a2a_task_check` 工具做后台轮询，不依赖外部二进制。

### 53. `a2a.rs` / `web_fetch.rs` HTTP 客户端每次调用都重新构建（无连接池复用）
- **位置**: `src/tools/a2a.rs` `build_client()` + 其他工具
- **描述**: `A2aCallTool` 是无状态结构体（`struct A2aCallTool;`），每次 `execute()` 都调用 `Self::build_client()` 创建新的 `reqwest::Client`（含独立连接池），执行后丢弃。Reqwest 的高效连接复用要求在调用间共享同一 `Client` 实例。
- **建议**: 用 `OnceLock<reqwest::Client>` 或将客户端存入工具结构体（借助 `Arc` 共享）。

### 54. `llm/openai.rs` 也存在伪流式问题（同 Issue #33）
- **位置**: `src/llm/openai.rs`, `parse_sse_stream()` 第 345 行
- **描述**: `let reader = resp.text().await?;` 在处理 SSE 流前先将完整响应体缓冲为字符串，与 Anthropic provider 的 Issue #33 完全相同。OpenAI streaming 也是假流式。
- **建议**: 改用 `resp.bytes_stream()` 逐行处理（与 Issue #33 同一修复方向）。

### 55. `llm/openai.rs` 构造函数中使用 `expect()`（违反 Invariant #5）
- **位置**: `src/llm/openai.rs`, 第 52 行：`.expect("reqwest client build")`
- **描述**: `OpenAiProvider::new()` 构造函数中调用 `reqwest::Client::builder().build().expect(...)` ——如果 TLS 后端不可用，这会 panic。`new()` 返回 `Self` 而非 `Result<Self>`，是历史遗留 API 设计问题。
- **建议**: 将构造函数改为 `new() -> Result<Self>` 并在所有调用点处理错误；或使用 `OnceLock` 延迟初始化并返回 `Option`。

---

## 🔴 严重问题（run_core.rs — 并行工具调用）

### 60. 并行工具任务 panic 后，对应工具调用无结果（orphan tool call）
- **位置**: `src/run_core.rs`, 第 414-427 行
- **描述**: 并行工具任务通过 `JoinSet::spawn` 执行，若某任务 panic（`Err(e) = join_set.join_next().await`），代码仅打印 `error!` 日志，不向 `batch_results` 插入任何内容。随后在 batch 结果遍历中（第 422-442 行），找不到对应 ID 的结果时执行 `continue`，该工具调用的结果被静默跳过。
- **影响**: transcript 中存在没有对应 tool result 的 tool call message，导致下一次 LLM API 请求因 "tool call ID not found" 被拒绝（Anthropic/OpenAI 均会返回 400 错误）。run 就此中断，且 panic 栈信息仅在 tracing log 中，用户完全不知情。
- **建议**: 对 `Err(e)` 情形也推入占位 error 结果：
  ```rust
  Err(e) => {
      tracing::error!("parallel tool task panicked: {e}");
      // Must push a placeholder to keep tool-call pairs intact
      batch_results.push((pc.id.clone(), pc.name.clone(), format!("ERROR: task panicked: {e}"), pc.args.clone(), AuditMeta::default(), 0));
  }
  ```

---

## 🟡 代码质量（compact.rs / session.rs）

### 58. `Compactor::estimate_chars` 忽略 tool_calls 参数，导致压缩触发时机不准确
- **位置**: `src/compact.rs`, 第 54 行
- **描述**: `estimate_chars` 计算 prompt 大小时只累加 `m.content.len()`，完全忽略 `m.tool_calls`（可能包含数千字符的 `arguments` JSON），以及 `reasoning_content`。对工具密集型 agent（大量 write_file / apply_patch 调用），真实 token 用量远超估算值，导致压缩触发门槛失效（实际上下文窗口已满，但 `estimate_chars` 认为还没到阈值）。
- **建议**: 在 `estimate_chars` 中加上 `tool_calls` arguments 的长度：
  ```rust
  transcript.iter().map(|m| {
      m.content.len() + m.tool_calls.iter().flat_map(|tc| tc.arguments_str.as_bytes()).count()
  }).sum()
  ```

### 59. `session.rs` msg_id 3位零填充在超过 999 条消息后破坏词典序
- **位置**: `src/session.rs`, 第 685 行：`format!("msg_{:03}", self.message_count)`
- **描述**: 3位补零格式（`msg_001`...`msg_999`）在消息数超过 999 后会输出 `msg_1000`，破坏了消息 ID 的词典序排序。一个 50 轮、每轮 20 条消息的 session 就会超过 1000 条。
- **建议**: 改为更大位宽，如 `{:06}`（`msg_000001`），或用 UUID 替代序号 ID。

---

## 总结（按优先级，共 60 个问题）

### 立即修复（11个）
1. **`ProviderTruncated` 违反 Invariant #7** → `run()` 返回 `Err` 导致 transcript 未保存，自我改进续期失效
2. **MCP `.mcp.json` env 字段静默丢弃** → MCP server 的 API key 等 env 完全不生效
3. **`hash_tool_specs` 序列化失败静默返回空字符串** → tool_registry 漂移检测失效
4. **`RetryPolicy` 不重试 HTTP 429** → 触发限流时 run 直接失败而非等待
5. 结构化紧凑输出的 `[compacted:` 检测不一致 → 静默丢失摘要前置
6. `list_sessions` 阻塞锁定 → HTTP API 可用性风险
7. **tui/backend.rs `Arc::try_unwrap().expect()`** → agent turn panic 导致 TUI 进程崩溃
8. **大多数 HookEvent 变体从未在生产中触发** → `sessionStart`/`sessionEnd`/`userPromptSubmit` 等钩子完全无效
9. **`openai.rs` 构造函数 `expect()`** → TLS 不可用时 panic
10. **`checkpoint.rs` 阻塞 git 调用在 async 中** → 每 turn 阻塞 tokio 工作线程
11. **`run_core.rs` 并行工具 panic 后 orphan tool call** → 下轮 LLM 请求 400 错误

### 短期改善（11个）
8. `SideEffect` 机制清理（要么实现，要么删除）
9. `Permission::Unknown` 改为显式权限策略
10. `HookEvent::SessionEnd` 在 Runtime 中补发或标记 not-implemented
11. `MemoryEntry` 命名冲突（multi vs memory 模块）
12. `TurnContext` 与 `RunCore` 合并/简化
13. `"ERROR_DENIAL_LIMIT:"` / `"ERROR: "` 字符串哨兵改为类型化枚举字段
14. `shell.rs` / `compact.rs` `expect()` 改为 `Result` 返回
15. **SSE 伪流式问题** → 改为真增量流式（`bytes_stream()`）
16. `session_id` 时间戳精度到秒 → 加短 UUID 后缀确保唯一性
17. `compact.rs` 步骤号硬编码 "N" → 调试信息无效
18. `find_duplicate` 返回类型歧义 → 引入枚举表达两种结果

### 短期改善（补充）
- **`AgentRole.allowed_tools` 死字段** → `run_with_role` 接入工具注册或删除字段
- **`MessageBus.messages` 无上限** → 加 max_history + ring-buffer
- **`TurnContext.mailbox` 始终 None** → 接入 MessageBus 实现真正的 inter-agent 通信
- **子 Agent step 预算** → 从父 Agent 继承剩余 step budget

### 长期重构（8个）
19. 超大文件拆分（session.rs / commands.rs / mcp.rs / handlers.rs / external.rs）
20. HTTP API `finish_reason` 稳定化（改用 serde Display）
21. `run_inner` 早期返回模板代码提取
22. MCP `McpServer`/`McpServerConfig` 合并
23. 内部/外部 `HookEvent` 命名去歧义
24. `wait_for_cancel` 忙轮询改为 `tokio::sync::Notify`
25. 时间戳工具函数统一（`facts.rs`/`session.rs`/`http/handlers.rs` 三处重复实现）
26. **`TeamOrchestrator` 串行委派改并发** (`join_all`)
27. **`runtime.rs` transcript 全量克隆** 改为 `Arc<Vec>` 或 borrow
28. **`checkpoint.rs` 阻塞 git 调用** 包装到 `spawn_blocking`

---

## 亮点（值得保留和借鉴）

1. **JSONL + `.meta.json` 分离** — 会话大文件追加写，元数据读写分离，设计合理
2. **`SessionLock` 基于 lockfile 防双开** — 用文件锁防止 resume 冲突，简单有效
3. **`CompactBoundaryEntry` 压缩边界标记** — 恢复时只读边界后内容，O(后压缩大小)
4. **`ToolSearchTool` 延迟加载** — 超大工具注册表下防 token 爆炸的优雅方案
5. **`filter_leading_assistant`** — 防止 Anthropic API 拒绝请求的防御层
6. **Orphan tool call 检测（g153）** — 跨进程重启后能识别未完成的工具调用
7. **RetryPolicy 指数退避** — 400/429/5xx 分级处理

*已完成模块: agent.rs, kernel.rs, run_core.rs, tools/mod.rs, hooks/mod.rs, event.rs, llm/anthropic.rs, llm/mod.rs, llm/openai.rs(部分), session.rs, tui/backend.rs, tools/facts.rs, tools/str_replace.rs, tools/a2a.rs(部分), tools/memory.rs(部分), tools/apply_patch.rs(部分), tools/sub_agent.rs, providers.rs, config_file.rs, config.rs, runtime.rs, multi.rs, checkpoint.rs, compact.rs, transcript.rs(部分), http/handlers.rs(部分), main.rs(部分)*
