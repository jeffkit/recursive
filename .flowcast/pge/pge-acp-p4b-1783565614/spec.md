# Recursive ACP Protocol Support — Product Spec

让 Recursive 作为 ACP v1 server 通过 stdio JSON-RPC 与 Zed / JetBrains 等编辑器通信，成为可被任何 ACP client 调用的 coding agent。新增 `recursive acp` CLI 子命令，与现有 MCP / HTTP 并列。分 7 个 sprint（Sprint 0 已完成协议类型层），P1→P7 从 stdio loop 到 CLI + E2E 全覆盖。

## Sprints
### 1. Sprint 1 — stdio JSON-RPC loop + initialize（P1）
- 作为 ACP client（Zed），我发送 `initialize` 请求后能收到正确的 `protocolVersion=1` + `agentInfo` + 完整 `agentCapabilities` 响应，握手成功即可确认 server 在线。
- 作为运维者，`recursive acp` 启动后 stderr 只输出日志、stdout 只输出 newline-delimited JSON-RPC，协议字节不污染。
- 作为 client，我发送非 `initialize` 请求时 server 返回 `MethodNotFound` error（因尚未实现其他方法），不会 panic 或退出。
- 作为开发者，`AcpServerRunner::run()` 复用 `McpServerRunner::run()` 的 stdin/stdout 套路，但独立模块不耦合 MCP 实现。

复用 `src/mcp_server.rs` 的 stdio loop 模式。`agentCapabilities` 先声明全部未来能力（session.new/prompt/cancel/load/resume、fs.readTextFile/writeTextFile、mcp、permission），但方法先只实现 `initialize`。验证方式：脚本 echo 一个 initialize JSON-RPC 请求 pipe 给 `recursive acp`，assert 响应含 `protocolVersion: 1` 和 `agentInfo.name: "recursive"`。

### 2. Sprint 2 — session/new + session/prompt text-only（P2）
- 作为 ACP client，我调用 `session/new` 传入 `cwd` 创建沙箱 session，得到 stable sessionId，后续所有操作以此为 scope。
- 作为 ACP client，我调用 `session/prompt` 发送纯文本消息后，能实时收到 `agent_message_chunk` notification（流式输出），最后收到 `end_turn` + `stopReason` 表示完成。
- 作为 client，`session/prompt` 中的 `ContentBlock[]` 里的 text 被正确拼成 Recursive 的 `Message`，image / resource_link 等 block 暂忽略但不报错。
- 作为 client，`session/update` 里的 `messageId` 是 stable 的（内容 hash），多次 load 同一 session 不会产生不同 id。
- 作为 client，当 prompt 触发 tool use 时（即使暂无 tool_call notification），agent 能正常完成 tool 执行并返回最终文本结果。

`session/new` 的 `cwd` 直接作为沙箱根接入 `resolve_within`，天作之合。`EventSink` 翻译事件 → `session/update` notification。messageId 用 SHA256 前 16 字符。暂不实现 tool_call notification（Sprint 3）、permission 桥（Sprint 4）、fs 反向（Sprint 6）。验证：端到端 `initialize → session/new → session/prompt("列出当前目录文件")`，收 `agent_message_chunk` × N + `end_turn`。

### 3. Sprint 3 — tool_call 通知 + kind/status 生命周期（P3）
- 作为 ACP client，当 agent 调用工具时，我收到 `session/update` 包含 `tool_call` notification（含 toolCallId、name、arguments、kind、status="pending"）。
- 作为 ACP client，工具执行过程中我收到 `tool_call_update` 将 status 推进到 `in_progress`，完成后收到 status=`completed` + 完整 result。
- 作为 ACP client，`tool_call` notification 的 `locations` 字段从工具参数中提取文件路径（如 Read 的 filePath、Edit 的 path），方便编辑器高亮。
- 作为 ACP client，不同工具类型的 `kind` 正确映射：Read→read、Edit→edit、Bash→execute、Glob→search、WebFetch→fetch、其他→other。
- 作为开发者，`Tool` trait 加 `kind() -> ToolKind` 默认方法返回 `Other`，各具体 tool 覆盖即可，不影响外部自定义 tool。

`Tool` trait 加默认方法 `kind() -> ToolKind`（`ToolKind` 新增 enum），各 tool 覆盖。`Event::ToolCallStart/End` → ACP notification 映射在 `src/acp/event_mapping.rs`。locations 用参数里的 `filePath` / `path` 字段（heuristic fallback 到 cwd）。验证：跑一个 Bash `ls` + 一个 Read，收 pending→in_progress→completed 三段 notification，kind 分别是 execute 和 read。

### 4. Sprint 4 — session/cancel + LLM 流 abort + permission 桥（P4）
- 作为 ACP client，我发送 `session/cancel` notification 后，正在执行的 prompt 立刻中止，最终 `end_turn` 的 `stopReason` 为 `"cancelled"`（不是 error），且 token 用量报告已完成部分。
- 作为 ACP client，cancel 后工具调用被安全中断：已跑的 tool 继续完成但不再启动新 tool，transcript 里 tool-call↔tool-result 保持配对（Invariant #8 不破）。
- 作为 client，当 agent 需要执行可能危险的操作时，我收到 `session/request_permission` notification，reply `PermissionOutcome` 后 agent 继续或拒绝。
- 作为开发者，LLM SSE 流在 cancel token 触发后立刻 `tokio::select!` 断开，`reqwest::Response` 被 drop 关闭连接，不等下一个 chunk。
- 作为 client，fs/* 的 agent→client RPC（如读取 editor buffer）30s 超时 + cancel token 双保险，不会永久挂起。

最复杂的 sprint。改 `src/llm/openai.rs::parse_sse_stream` 和 `src/llm/anthropic.rs::parse_sse_stream` 加 `tokio::select!` cancel token。`session/cancel` → `AgentRuntime::set_interrupt_token`。`PermissionHook` 桥 → ACP `session/request_permission` notification + 等待 client reply。`src/acp/server.rs` 顶部加「协作式 cancel」文档说明 token 传播路径。验证：(1) cancel 100ms 内 LLM 流断；(2) cancel 后 transcript 里配对完整；(3) permission 弹窗可 deny。

### 5. Sprint 5 — session/load 历史回放 + session/resume（P5）
- 作为 ACP client，我调用 `session/load` 后收到完整历史回放：所有 user/agent message、tool_call、tool_result 以 `session/update` notification 按时间序重放，回放完后收到 `return result=null`。
- 作为 ACP client，`session/resume` 恢复 context 但不回放历史，直接 `return result=null`，之后 prompt 继续在同一会话上下文中。
- 作为 client，load/resume 时如果 session 之前有 MCP server，旧的 stdio 子进程被 kill，用新传入的 `mcpServers` 配置重新连接（不泄露进程）。
- 作为 client，`SessionCapabilities` 声明 `resume: {}` + `loadSession: true`，我可以在 initialize 响应中确认这些能力存在。

从 `SessionStore` 拉 transcript 逐条回放。messageId 用内容 hash（Sprint 2 决策）。resume 只恢复 `AgentRuntime` context 不发 notification。MCP 重连逻辑：kill 老进程 → 解析新 `mcpServers` 配置 → 启动新进程。验证：session A 跑一轮 prompt，关闭后 `load` session A，看到所有历史消息回放；再 `resume` 同一 session，不回放但接续上下文。

### 6. Sprint 6 — editor fs 反向读写 + MCP 多 transport + session/close 清理（P6）
- 作为 Zed 用户，当我在编辑器中打开未保存文件时，agent 的 Read 操作优先读取 editor buffer 内容（而非磁盘旧版本），拿到我最新的编辑状态。
- 作为 ACP client，我在 `initialize` 时声明 `fs.readTextFile=true`，agent 的 Read tool 自动切换到 `ClientReadFile` 路径；若未声明则降级到本地文件系统 Read，**沙箱校验永远跑**。
- 作为 client，agent 的 Edit/Write 结果会通过 `ClientWriteFile` 写回 editor buffer，我不需要手动刷新文件。
- 作为运维者，`session/close` 或 session 超时后，session 的所有 stdio MCP 子进程被 kill，`ps` 无残留。
- 作为 ACP client，我可以在 `session/new` 的 `mcpServers` 里配置 stdio/http/sse 三种 transport 的 MCP server，agent 能正确连接并使用。
- 作为 client，session-scoped MCP server 与全局 config 的 MCP server 共存，命名冲突时 session 优先。

`ClientReadFile` / `ClientWriteFile` 新增工具，挂在 `src/acp/tools/`。client 声明 `fs.readTextFile=true` 时 Read tool 优先发 `fs/read_text_file` RPC 给 client，超时或失败降级本地。MCP bridge 扩 `src/acp/mcp_bridge.rs`：stdio 起子进程、http/sse 连 URL。session 清理：`Drop` 或显式 `close` 时 kill 所有子进程。声明 `mcpCapabilities: { http: true, sse: true }`。验证：(1) editor 未保存 buffer 内容被 agent 读到；(2) stdio/http/sse 三种 MCP 都能连；(3) close 后 `ps` 无残留。

### 7. Sprint 7 — CLI `recursive acp` 子命令 + E2E + invariants 测试（P7）
- 作为 Recursive 用户，我执行 `recursive acp` 启动 ACP server，`recursive --help` 看到 `acp` 子命令与 `mcp` / `http` 并列。
- 作为 QA，E2E scripted ACP client 能跑完 `initialize → session/new → session/prompt（触发 tool_call）→ session/cancel → session/load` 全流程，断言所有 notification 序列正确。
- 作为架构守护者，invariants test 验证：(a) ACP 代码未在 `run_inner` 中加分支；(b) ACP host fs 操作过 `resolve_within`；(c) cancel 后 transcript tool-call↔tool-result 配对成立。
- 作为 Zed 用户，我在 Zed 的 ACP 配置里指向 `recursive acp` 后能用「Ask」面板正常对话、看到流式输出、使用工具、取消请求。

CLI 在 `crates/recursive-cli/src/main.rs` 的 `Cli` 枚举加 `Acp` 变体。E2E 按 `CLAUDE.md` 规则：scripted ACP client via expect/bash pipe，断言 JSON-RPC notification 序列。invariants test 参考 `tests/invariants/loop_size_orthogonality.rs` 的 AST 检查模式。最终手动验收：Zed 连上 `recursive acp` 完成一次对话。
