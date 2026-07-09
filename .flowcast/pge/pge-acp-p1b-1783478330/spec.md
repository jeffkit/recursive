# ACP Protocol Support — Product Spec

让 Recursive 编码 agent 实现 Agent Client Protocol v1 的完整 server 端支持，通过 stdio JSON-RPC 与 Zed/JetBrains 等编辑器通信，使其可被任何 ACP client 当作 coding agent 调用。Sprint 0（协议类型层）已完成，本 spec 覆盖 Sprint 1-7。

## Sprints
### 1. Sprint 1 — P1: stdio JSON-RPC loop + initialize
- 作为 ACP client，我可以通过 stdio 发送 initialize 请求，收到包含 protocolVersion=1、agentInfo、完整 agentCapabilities 的正确响应
- 作为 Recursive 开发者，我希望 AcpServerRunner 复用现有 McpServerRunner 的 stdin/stdout newline-delimited JSON-RPC 2.0 套路，确保协议字节不会与日志（stderr）混淆
- 作为 Zed 用户，我执行 `recursive acp` 后编辑器能探测到 agent 可用并展示其能力声明

地基 sprint。不涉及 LLM、session、tool。只验证 transport 层和 initialize 握手。验收：手动发送 initialize JSON 给 stdin，stdout 出正确 response；cargo test + clippy + fmt 全绿。

### 2. Sprint 2 — P2: session/new + session/prompt（text-only）
- 作为 ACP client，我可以通过 session/new 创建 session（指定 cwd 作为沙箱根），收到稳定的 sessionId
- 作为 ACP client，我可以通过 session/prompt 发送 text 消息并获得流式的 agent_message_chunk 通知，最终收到 end_turn 通知
- 作为 Recursive 用户，session/prompt 的 cwd 自动作为 resolve_within 沙箱根，保证安全隔离
- 作为 ACP client，session/update 中的 messageId 基于内容 hash 生成且稳定可复现

首条端到端链路。EventSink 映射到 ACP notification。暂不做 tool_call 通知、permission、fs。验证：发一条 'echo hello' prompt，收到 streaming chunks + end_turn。

### 3. Sprint 3 — P3: tool_call 通知 + kind/status 生命周期
- 作为 ACP client，当 agent 调用工具时我收到 tool_call notification（含 toolCallId、name、kind、status: 'pending'）
- 作为 ACP client，当工具开始执行时我收到 tool_call_update（status: 'in_progress'），完成后收到 completed/failed
- 作为 ACP client，tool_call notification 的 locations 字段正确提取自工具参数中的路径（Read→uri, Edit→uri, Bash→cwd 等）
- 作为 Recursive 开发者，Tool trait 新增 kind() 默认方法（Read→read, Edit→edit, Bash→execute, Glob→search 等），具体 tool 覆盖即可

Tool trait 扩展必须向后兼容（默认方法返回 Other）。验证：跑一个 Bash 调用，client 观察 pending→in_progress→completed 完整三段。

### 4. Sprint 4 — P4: session/cancel + LLM 流 abort + permission 桥
- 作为 ACP client，我发送 session/cancel 后 LLM 流立即停止（不等待下一个 chunk），session/prompt 响应为 stopReason: 'cancelled' 而非 error
- 作为 Recursive 开发者，ChatProvider::stream（OpenAI + Anthropic）的 SSE 循环通过 tokio::select! 绑定 cancel token，reqwest::Response drop 立即关闭连接
- 作为 ACP client，当 agent 需要权限确认时我收到 session/request_permission 通知，可以返回 PermissionOutcome，agent 据此继续或拒绝操作
- 作为 Recursive 用户，取消后 transcript 中 tool-call ↔ tool-result 配对仍然完整（Invariant #8），host 拒绝编辑时写入 error tool result 而非丢弃
- 作为 Recursive 开发者，agent→client 的 fs/* 在途 RPC 绑定 CancellationToken + 30s 超时双保险（决策 4a）

最复杂 sprint。涉及三处改动：(a) 两个 provider 的 SSE 解析加 cancel select，(b) PermissionHook 桥接，(c) fs RPC 超时+取消双保险。src/acp/server.rs 顶部需写「协作式 cancel」文档（决策 4c）。可能需按 4a/4b/4c 拆子 sprint。验证：(1) cancel 时流立刻断；(2) stopReason 是 cancelled 不是 error；(3) permission 弹窗能交互；(4) transcript 配对不破。

### 5. Sprint 5 — P5: session/load 历史回放 + session/resume
- 作为 ACP client，session/load 后我能收到完整的 session/update 通知序列，逐条回放历史消息（user_message_chunk / agent_message_chunk / tool_call），回放完成 return result=null
- 作为 ACP client，session/resume 直接恢复上下文 return result，不回放历史，不触发新 LLM 调用
- 作为 ACP client，SessionCapabilities 正确声明 resume: {} 和 loadSession: true
- 作为 Recursive 用户，load 或 resume 时自动 kill 旧的 stdio MCP 子进程并重连（决策 5.4），不泄露进程

依赖 Sprint 2-4 的 session 基础设施。messageId 复用 Sprint 2 的内容 hash 逻辑。验证：关掉重连，load 看到完整历史；resume 不回放。

### 6. Sprint 6 — P6: editor fs（agent→client）+ MCP 多 transport + session/close 清理
- 作为 ACP client，当声明 fs.readTextFile=true 时，agent 优先通过 ClientReadFile 工具从编辑器 buffer 读取未保存内容，否则降级到本地 Read（沙箱校验永远执行）
- 作为 ACP client，agent 可通过 ClientWriteFile 工具将内容写回编辑器 buffer
- 作为 ACP client，我可以配置 MCP server 为 stdio（本地子进程）、http 或 sse（远程 URL）三种 transport，agentCapabilities.mcpCapabilities 声明 { http: true, sse: true }
- 作为 Recursive 用户，session/close 时自动 kill 所有 stdio MCP 子进程，ps 无遗留
- 作为 Recursive 用户，session 级 MCP 配置与全局 config 合并，命名冲突时 session 优先（决策 5.2）

MCP bridge 实现在 src/acp/mcp_bridge.rs。ClientReadFile/ClientWriteFile 是 agent→client 的反向 RPC，需处理编辑器不支持的降级路径。验证：(1) 拿 editor 未保存 buffer；(2) 三种 MCP transport 都通；(3) close 后无进程泄露。

### 7. Sprint 7 — P7: CLI recursive acp 子命令 + E2E + invariants 测试
- 作为 Recursive 用户，`recursive acp` 子命令与现有 mcp/http 并列，`recursive --help` 可见
- 作为 QA，E2E 测试覆盖完整 ACP 流程：initialize → session/new → session/prompt（含 tool_call）→ session/cancel → session/load，断言 notification 序列正确
- 作为 Recursive 开发者，invariants 测试通过 AST 检查确认：ACP 代码未在 run_inner 加分支、host fs 操作均经 resolve_within、cancel 后 tool-call↔tool-result 配对成立
- 作为 Zed 用户，手动执行 `recursive acp` 并配置编辑器后，可正常完成一次 coding session

收尾 sprint。CLI 注册在 crates/recursive-cli/src/main.rs 的 Cli 枚举加 Acp 变体。E2E 严格按 CLAUDE.md e2e 规则：argusai -c e2e.yaml。Invariants 测试复用 tests/invariants/ 现有 AST 检查模式。最终手动验收：Zed 能连上跑一次完整 coding session。
