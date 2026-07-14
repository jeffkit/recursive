# ACP (Agent Client Protocol) v1 Support for Recursive

使 Recursive 编码 agent 成为 ACP v1 server，通过 stdio JSON-RPC 与 Zed / JetBrains 等编辑器通信，作为可被任意 ACP client 调用的 coding agent。新增 `recursive acp` CLI 子命令，与现有 `recursive mcp` / `recursive http` 并列。覆盖完整的 session 生命周期、tool_call 通知、cancel+abort、历史回放、editor fs 反向读写、MCP 多 transport、权限桥接，以及端到端验收和 invariants 守护测试。

## Sprints
### 1. Sprint 0 — ACP 协议类型层 (P0)
- 作为开发者，我希望 `agent-client-protocol-schema` crate 的类型定义能在项目中编译通过，以便后续所有 ACP 模块都能依赖统一的 wire type。
- 作为开发者，我希望每个 ACP 请求/响应/notification 的 struct 和 enum 都有 serde round-trip 单元测试覆盖，确保与 spec 兼容。
- 作为开发者，我希望 ACP 类型层不引入任何运行时依赖，只做纯数据定义，遵守 Invariant #6（无冗余依赖）。

落地 `src/acp/protocol.rs`，基于官方 `agent-client-protocol-schema` crate。验证点：serde round-trip 单测覆盖每个 enum/struct；编译过；clippy 干净。此为地基 sprint，所有后续模块依赖此层。

### 2. Sprint 1 — stdio JSON-RPC loop + initialize (P1)
- 作为 ACP client（如 Zed），我希望发送 `initialize` 请求后能收到 `protocolVersion=1`、`agentInfo` 和完整 `agentCapabilities` 的响应，以便确认 server 能力并完成握手。
- 作为运维者，我希望 ACP server 的 stdio loop 复用现有 `McpServerRunner` 的成熟模式（stdin 读行、JSON-RPC 2.0 解析、stdout 写响应），不引入新的 transport 范式。
- 作为开发者，我希望 stderr 只用于日志，stdout 无任何非协议字节，确保 ACP client 不会解析失败。

在 `src/acp/server.rs` 实现 `AcpServerRunner::run()`，初期只支持 `initialize` 方法。验证：喂一个 `initialize` 请求，能写出正确的 response。此 sprint 确立整个 server loop 骨架。

### 3. Sprint 2 — session/new + session/prompt (text-only) (P2)
- 作为 Zed 用户，我希望在编辑器中触发 Recursive agent 后，agent 能接收我的文本 prompt 并返回流式响应，以便获得 AI 编码辅助。
- 作为 Recursive agent，我希望 `session/new` 的 `cwd` 参数自动成为沙箱根，确保所有文件操作受现有 sandbox 约束（Invariant #3）。
- 作为开发者，我希望 `session/prompt` 将 `ContentBlock[]` 中的文本拼成 `Message` 喂给 `AgentRuntime::run()`，并通过 `EventSink` 翻译成 `session/update` notification（`agent_message_chunk` + 最终 `stopReason`）。
- 作为 ACP client，我希望每条消息都有稳定的 `messageId`（基于内容 hash 生成），以便客户端进行去重和引用。

ACP session 接到 `AgentRuntime`。暂不做 tool_call 通知、permission、fs。验证：端到端跑一个 text prompt，收到 `agent_message_chunk` + `end_turn`。messageId 用内容 hash（决策 2），不改 transcript schema。

### 4. Sprint 3 — tool_call 通知 + kind/status (P3)
- 作为 Zed 用户，我希望在 agent 调用工具（如 Read、Bash、Edit）时，编辑器能实时展示工具的执行状态（pending → in_progress → completed），以便了解 agent 正在做什么。
- 作为开发者，我希望每个 `Tool` 实现都能通过 `kind()` 方法声明自己的 `ToolKind`（Read→read, Edit→edit, Bash→execute, Glob→search 等），以便 ACP 通知自动携带正确的 kind 字段。
- 作为 ACP client，我希望 `tool_call` notification 的 `locations` 字段能从工具的 path 参数自动提取，以便编辑器高亮相关文件。

`Tool` trait 加默认方法 `kind() -> ToolKind`（默认 `Other`）。翻译 `ToolCall` / `ToolResult` 事件为 `session/update` 的 `tool_call`（首次）和 `tool_call_update`（后续）notification。验证：跑一个 Bash 调用，client 看到 pending→in_progress→completed 三段。

### 5. Sprint 4 — session/cancel + LLM 流 abort + permission 桥 (P4)
- 作为 Zed 用户，我希望按 Esc 或点击取消后，agent 的 LLM 流能立刻中止（不等下一个 chunk），且 session 返回 `stopReason: "cancelled"` 而非 error，以便获得流畅的打断体验。
- 作为开发者，我希望 `ChatProvider::stream` 在 SSE 循环中用 `tokio::select!` 监听 cancel token，一旦触发就 drop `reqwest::Response` 关闭连接，返回 `Err(Error::Cancelled)`，且 run_inner 已有逻辑将其翻成 `FinishReason::Cancelled`（不破 Invariant #7）。
- 作为 Zed 用户，我希望 agent 请求权限时（如执行危险命令），编辑器能弹出 permission 对话框，我选择 Allow/Deny 后 agent 继续执行。
- 作为开发者，我希望 agent→client 的在途 fs/* RPC 既有 `CancellationToken` 绑定又有 30s 超时双保险，防止 client 无响应时 agent 永久挂起。
- 作为开发者，我希望取消/abort 后 transcript 中 tool-call ↔ tool-result 配对仍然成立，host 拒绝编辑时必须写入一条 error tool result（Invariant #8）。

最复杂的 sprint。分三块：(a) 改 `src/llm/openai.rs::parse_sse_stream` 和 `src/llm/anthropic.rs::parse_sse_stream` 加 cancel token select；(b) `session/cancel` notification 接 `AgentRuntime::set_interrupt_token` + `PermissionHook` 桥接；(c) `src/acp/server.rs` 顶部写「协作式 cancel」文档（决策 4c）。验证 4 项：cancel 流立刻断、响应是 cancelled 非 error、permission 弹窗可交互、Invariant #8 不破。

### 6. Sprint 5 — session/load 历史回放 + session/resume (P5)
- 作为 Zed 用户，我希望关闭编辑器后重新打开时，能看到上一次 agent 会话的完整历史（我的消息、agent 响应、tool 调用），以便无缝继续工作。
- 作为 Zed 用户，我希望 `session/resume` 只恢复上下文而不回放历史，避免重复通知刷屏。
- 作为开发者，我希望 `session/load` 或 `session/resume` 时自动 kill 上次遗留的 stdio MCP 子进程并启动新 server（决策 5.4），确保不泄露进程资源。
- 作为 ACP client，我希望 `SessionCapabilities` 声明 `resume: {}` 和 `loadSession: true`，以便客户端知道可以调用这些方法。

`session/load` 从 SessionStore 拉历史 transcript，逐条用 `session/update` 回放（`user_message_chunk` / `agent_message_chunk` / `tool_call`），回放完 `return result=null`。`session/resume` 不回放，恢复 context 后 return。messageId 用内容 hash（决策 2）。验证：关掉重连，load 能看到完整历史；resume 不回放。

### 7. Sprint 6 — editor fs + MCP 多 transport + session/close 清理 (P6)
- 作为 Zed 用户，我希望 agent 能读取我在编辑器中尚未保存的 buffer 内容，以便基于最新编辑状态给出建议。
- 作为 Zed 用户，我希望 agent 能直接写入编辑器 buffer（经我确认），而不是只写磁盘文件。
- 作为开发者，我希望 editor fs（`ClientReadFile` / `ClientWriteFile`）在 client 声明 `fs.readTextFile=true` 时优先使用 client buffer，否则降级到本地 Read，且所有操作仍过 `resolve_within` 沙箱校验。
- 作为开发者，我希望 MCP bridge 支持 stdio（起子进程）、http、sse 三种 transport，配置转换集中在 `src/acp/mcp_bridge.rs`。
- 作为运维者，我希望 `session/close` 后所有 stdio MCP 子进程被 kill，`ps` 看不到遗留进程。
- 作为开发者，我希望 session-scoped MCP server 与全局 config 合并时，命名冲突以 session 优先。

决策 1（editor fs）+ 决策 5.1/5.2/5.3/5.4（MCP 多 transport + 清理）合并在一个 sprint，因为它们共享 session 生命周期管理。验证：(1) 能拿 editor 未保存 buffer；(2) stdio/http/sse 三种 MCP server 都能连；(3) session 关闭后无遗留子进程；(4) 沙箱逃逸检测通过。

### 8. Sprint 7 — CLI 子命令 + E2E + invariants 守护 (P7)
- 作为 Recursive 用户，我希望 `recursive acp` 子命令能像 `recursive mcp` 一样简单启动，以便在任何支持 ACP 的编辑器中使用 Recursive。
- 作为 QA，我希望 E2E 测试覆盖完整的 prompt→tool→cancel→load 流程，断言 notification 序列与 ACP spec 一致。
- 作为架构守护者，我希望 invariants test 自动验证：ACP 代码不往 `run_inner` 加分支、host fs 操作过 `resolve_within`、cancel 后 tool-call↔tool-result 配对成立。
- 作为 Zed 用户，我希望最终能通过编辑器的 ACP 配置指向 `recursive acp`，获得完整的 coding agent 体验。

在 `crates/recursive-cli` 加 `Acp` 子命令，与 mcp/http 并列。E2E 按 `CLAUDE.md` e2e 规则编写。invariants test 包括：(1) AST 检查 ACP 代码不往 `run_inner` 加分支；(2) 沙箱逃逸检测；(3) Invariant #8 配对检查。最终手动验收：Zed 编辑器能通过 `recursive acp` 使用 Recursive 当 coding agent。
