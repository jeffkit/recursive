# ACP (Agent Client Protocol) — 让 Recursive 成为 Zed/JetBrains 等编辑器的原生 coding agent

实现 ACP v1 server 端协议，通过 stdio JSON-RPC 暴露 Recursive agent 给任何 ACP client（Zed、JetBrains 等）。分 7 个 sprint（Sprint 0 已完成协议类型层），从 stdio loop 起步，逐步加上 session 管理、tool call 可视化、流式 cancel、权限桥、历史回放、editor fs 与 MCP 多 transport，最终交付 CLI 子命令与全链路 E2E 验证。

## Sprints
### 1. Sprint 1 — stdio JSON-RPC loop + initialize 握手
- 作为 Zed 用户，我启动 `recursive acp` 后能看到 agent 返回 initialize response，包含 protocolVersion=1、agentInfo（name=recursive、version）、以及完整的 agentCapabilities（包括 session、fs、mcp 等声明）
- 作为 ACP client 开发者，initialize 握手后我收到 `initialized` notification，标志连接就绪，可以调用后续方法
- 作为 Recursive 开发者，AcpServerRunner 结构清晰复用了现有 McpServerRunner 的 stdio 循环模式（stdin 逐行读 JSON-RPC → dispatch → stdout 写），不引入新的 transport 抽象
- 作为 QA，初始化流程有 serde round-trip 单测覆盖 request/response 结构，且 stdin 输入非法 JSON 时返回标准 JSON-RPC parse error 而非 panic

关键交付：`src/acp/server.rs` 的 `AcpServer` struct + `AcpServerRunner::run()` stdio loop。initialize response 的 `agentCapabilities` 字段要声明后续 sprint 会实现的全部能力（session、fs、mcp），但方法只在对应 sprint 注册——未实现的方法返回 `MethodNotFound`（-32601）。参考 `src/mcp_server.rs` 结构但不耦合。

### 2. Sprint 2 — session/new + session/prompt（text-only，无 tool call 通知）
- 作为 Zed 用户，我在编辑器里对 Recursive agent 发一句 'explain this code'，agent 能理解并流式返回回答，我在编辑器里逐字看到 agent_message_chunk 出现
- 作为 ACP client 开发者，我调用 session/new（带 cwd）创建一个 session，拿到稳定的 sessionId，然后调用 session/prompt 发送 ContentBlock[]，通过 session/update notification 收到流式文本和最终的 stopReason='end_turn'
- 作为 Recursive 开发者，ACP session 正确复用了现有 AgentRuntime::run()——接收 cwd 作沙箱根，走 resolve_within，ACP adapter 不侵入 run_inner
- 作为 QA，messageId 使用内容 hash 生成稳定标识，同一段文本的 messageId 在 session/load 回放时与原始 session 一致

关键决策：session/new 的 cwd → 沙箱根（Invariant #3）；EventSink 翻译 AgentEvent → session/update notification；ContentBlock[] 中的 text block 拼接为 Message 喂给 runtime。messageId 用 SHA-256 前 12 字符 hex。暂不做 tool_call notification、permission、fs——这些在 Sprint 3/4/6。

### 3. Sprint 3 — tool_call 通知 + kind/status 生命周期
- 作为 Zed 用户，当 Recursive 执行 `cargo test` 时，我在编辑器里能看到一个 tool_call 卡片，显示 Bash 图标、command 内容、并经历 pending → in_progress → completed 状态变化
- 作为 ACP client 开发者，ToolCall 开始时收到 kind=execute 的 tool_call notification（status=pending），执行中收到 tool_call_update（status=in_progress），完成后收到 tool_call_update（status=completed + output），每条带唯一 toolCallId
- 作为 Recursive 开发者，Tool trait 新增默认方法 `kind() -> ToolKind`，Read→read、Edit→edit、Bash→execute、Glob→search、Write→write、WebFetch→fetch，未归类工具默认 Other
- 作为 QA，tool_call notification 的 locations 字段从工具参数中正确提取（Read/Edit 的 path、Bash 的 cwd 等），且 tool_call 与 tool_call_update 的 toolCallId 严格匹配

`ToolKind` 枚举（read/edit/execute/search/write/fetch/other）映射到 ACP spec 的 kind 字段。status 三态：pending（tool_call 发出时）、in_progress（开始执行时）、completed/failed（结束时）。Event 流中 `ToolCallStarted` → `ToolCallRunning` → `ToolCallFinished` 三个事件映射到 notification 序列。

### 4. Sprint 4 — session/cancel + LLM 流 abort + permission 桥
- 作为 Zed 用户，我按 Escape 取消一个正在跑的长时间 agent 任务，LLM 流立刻停止（不等下一个 token），session/prompt 返回 stopReason='cancelled' 而非 error
- 作为 Zed 用户，当 agent 要执行 `rm -rf /` 时，编辑器弹出权限确认弹窗，我点 Deny 后 agent 收到拒绝并把 tool result 写为 'denied by user'，transcript 配对不破
- 作为 ACP client 开发者，我发 session/cancel notification 后，agent 的 LLM 流通过 cancel token 立刻 abort（reqwest Response drop 断连接），返回 cancelled 而非 HTTP 499
- 作为 Recursive 开发者，PermissionHook 桥通过 session/request_permission → client → PermissionOutcome 的异步 round-trip 实现，在途 fs/* RPC 有 cancel token + 30s 超时双保险
- 作为 QA，cancel 后 transcript 的 tool-call ↔ tool-result 配对仍完整（Invariant #8）：每个 Assistant tool_call 后面紧跟着对应的 Tool result，取消路径不丢消息

最复杂的 sprint。改 `src/llm/openai.rs::parse_sse_stream` 和 `src/llm/anthropic.rs::parse_sse_stream`：SSE 循环加 `tokio::select!` 同时监听 cancel token 和下一个 chunk。`src/acp/server.rs` 顶部加「协作式 cancel」文档。permission 桥：ACP adapter 收到 tool 的 permission 请求时发 `session/request_permission`，等 client 返回 `permission_outcome` callback，转 `PermissionDecision`。决策 4a：fs/* RPC 超时用 `tokio::time::timeout(30s)` 包 cancel token。

### 5. Sprint 5 — session/load 历史回放 + session/resume 恢复
- 作为 Zed 用户，我关掉编辑器后重新打开，之前的 agent 对话历史完整回放出来——我的消息显示为 user_message_chunk，agent 回复显示为 agent_message_chunk，tool call 卡片也重现
- 作为 Zed 用户，我用 session/resume 恢复对话时不会看到历史回放（节省带宽），但 agent context 已恢复，可以直接继续对话
- 作为 ACP client 开发者，session/load 方法从 SessionStore 拉历史 transcript，逐条翻译为 session/update notification 推送，全部回放完后 return null（不额外推送），且 messageId 与原始 session 一致
- 作为 QA，session/load 和 session/resume 都正确处理 MCP 生命周期——kill 上一次的 stdio 子进程、按新的 mcpServers 配置启动新 server、再返回结果（决策 5.4），用 `ps` 验证无进程泄露

SessionCapabilities 声明 `resume: {}` + `loadSession: true`。load 回放顺序：按 transcript 时间线逐条推送 user_message_chunk / agent_message_chunk / tool_call + tool_call_update，全部推送完后 return。resume 只恢复 runtime context，不推送历史。决策 5.4：必须在加载/恢复前完成 MCP 清理+重连，不能先返回再异步处理。

### 6. Sprint 6 — editor fs（agent→client）+ MCP 多 transport + session/close 清理
- 作为 Zed 用户，当 agent 读一个我修改了但还没保存的文件时，agent 拿到的是编辑器 buffer 的最新内容（而非磁盘上的旧版本），且这个读取仍受沙箱限制——agent 只能读 cwd 子树内的文件
- 作为 ACP client 开发者，我可以在 session/new 时声明 fs.readTextFile=true，agent 会在 ClientReadFile 内部先调 editor fs/read_text_file RPC，失败或无声明时降级到本地 Read 工具
- 作为 ACP client 开发者，我的 MCP server 可以通过 stdio（本地起子进程）、HTTP（远程 URL）、SSE（远程事件流）三种方式接入 agent，session/new 的 mcpServers 配置灵活支持，session 内的 MCP 工具与全局 config 合并且命名冲突时 session 优先
- 作为 QA，session/close 后所有该 session 的 stdio MCP 子进程都被 kill（`ps aux | grep` 无遗留），且多 session 并存时各自的 MCP server 互不干扰；多 transport 有端到端连通性测试

ClientReadFile / ClientWriteFile 工具注册在 ACP session scope，通过决策 1 的路由逻辑：client 声明 fs.readTextFile=true → 调 editor RPC → 降级本地 Read。沙箱校验永远跑（Invariant #3）：editor 返回的路径也要过 resolve_within。MCP bridge 在 `src/acp/mcp_bridge.rs`，扩展现有 `src/mcp.rs` 的 transport 支持（stdio 起子进程、http 连 URL、sse 连事件流），session/close 触发子进程 kill。

### 7. Sprint 7 — CLI `recursive acp` 子命令 + E2E + 不变式验证
- 作为 Recursive 用户，我在终端输入 `recursive acp` 就能启动 ACP server（与 `recursive mcp`、`recursive http` 同级），`recursive --help` 的 subcommand 列表里能看到 acp
- 作为 QA，一个 scripted ACP client 跑完整流程——initialize → session/new → session/prompt（含 tool call）→ session/cancel → session/load → session/close——断言每一步的 notification 序列和字段值都正确
- 作为 Recursive 维护者，invariants test 程序化验证：(1) ACP 代码未在 run_inner 加分支（AST 检查），(2) ACP host fs 操作过 resolve_within，(3) cancel 后 transcript tool-call↔tool-result 配对成立，(4) session/cancel 响应 stopReason='cancelled' 非 error
- 作为 Zed 用户，我在 Zed 的 agent 配置里把 command 设为 `recursive acp`，打开一个 Rust 项目，让 agent 解释一段代码——端到端工作正常

CLI 子命令注册在 `crates/recursive-cli/src/main.rs` 的 `Cli` 枚举加 `Acp` 变体。E2E 测试按 `CLAUDE.md` 的 e2e 规则编写，用 argusai 跑（通过 symlink 的 infra4agent/argusai）。invariants test 参考 `tests/invariants/` 下已有模式。最终手动验收：Zed 编辑器配置 `recursive acp` 为 agent，完成一次真实 coding 交互。
