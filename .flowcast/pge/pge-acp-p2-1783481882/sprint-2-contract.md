# Contract: Sprint 2 (rev 2)

session/new 创建沙箱 session，session/prompt 将 ContentBlock[] 文本拼接后喂给 AgentRuntime::run()，通过 EventSink 将 AgentEvent 翻译为 session/update notification 流式推送 agent_message_chunk，最终 stopReason='end_turn'。messageId 使用 SHA-256 前 12 字符 hex。多个并发 session 各自独立、互不干扰。不实现 tool_call 通知、permission、fs、session/load、session/close。

## Criteria
- [C1] session/new（带 cwd 参数）返回稳定的 sessionId，该 sessionId 在后续 session/prompt 调用中可正常使用
  - how: 调用 session/new {"cwd": "/tmp/acp-test"}，断言 response.result.sessionId 为非空字符串；随后用该 sessionId 调用 session/prompt，断言未返回 session-not-found 类错误
- [C2] session/new 的 cwd 作为沙箱根传入 AgentRuntime::run()，agent 的文件操作受 resolve_within 约束，无法访问 cwd 子树外的路径
  - how: 创建 session 时 cwd=/tmp/acp-sandbox，发送 prompt 要求 agent 读 /etc/passwd（位于 cwd 外）。断言 agent 的 Read 工具调用被沙箱拒绝（ToolResult 包含 resolve_within 拒绝信息），且 agent 无权访问沙箱外文件
- [C3] session/new 在 cwd 不存在或无权限时返回标准 JSON-RPC 错误（非 panic）
  - how: 调用 session/new {"cwd": "/nonexistent/path/xyz"}，断言返回 JSON-RPC error response（code ≠ 0），服务进程不崩溃
- [C4] session/prompt 接收 ContentBlock[]（含一个或多个 text block），将所有 text 拼接为单个 Message 喂给 AgentRuntime::run()
  - how: 单元测试：构造 ContentBlock[] = [{"type":"text","text":"hello"},{"type":"text","text":" world"}]，验证拼接后传给 runtime 的 Message content 为 "hello world"
- [C5] Agent 的文本回复通过 session/update notification 以 agent_message_chunk 形式逐块流式推送给 client
  - how: 调用 session/prompt 发 "explain this code"，收集所有 session/update notification。断言：(a) 存在 notification.method === 'session/update'，(b) params.update.type === 'agent_message_chunk'，(c) 至少有一条 chunk 的 content.text 非空，(d) chunk 按生成顺序到达（时间戳递增）
- [C6] Agent 正常完成回复后，最后一条 session/update notification 包含 stopReason='end_turn'，不再有后续 chunk
  - how: 等待 session/prompt 返回后，检查最后一条 session/update notification。断言：(a) params.update.stopReason === 'end_turn'，(b) 该 notification 之后无新的 agent_message_chunk
- [C7] 每条 agent_message_chunk 的 messageId 为该段文本内容的 SHA-256 前 12 字符 hex，同一段文本跨会话 messageId 一致
  - how: 单元测试：对已知文本 "Hello, world!" 计算 SHA-256 取前 12 字符 hex 作为 expected_id；模拟生成对应 agent_message_chunk，断言 messageId === expected_id。再在另一 session 中生成相同文本，断言 messageId 不变
- [C8] ACP adapter 通过 EventSink trait 将 AgentEvent（Token/AgentMessage/TurnComplete）翻译为 session/update notification，不侵入 run_inner 的 ReAct 循环
  - how: 代码审查：确认 run_inner（src/run_core.rs）中无 ACP 相关分支或 import；EventSink 实现在 acp/ 模块内，作为独立 trait impl 注入 AgentRuntime::run()
- [C9] session/prompt 对无效的 sessionId 返回 JSON-RPC 错误（-32602 Invalid params 或自定义错误码），不 panic
  - how: 对随机不存在的 sessionId 调用 session/prompt，断言返回 error response（含 code ≠ 0 和 message），服务进程不崩溃
- [C10] 同一 session 内多次 session/prompt 调用保持对话上下文（历史消息累积在 transcript 中）
  - how: session/prompt #1: "my name is Alice"，session/prompt #2: "what is my name?"。断言 #2 的 agent 回复包含 "Alice"，证明 agent 保留了 #1 的上下文
- [C11] session/prompt 的 ContentBlock[] 仅处理 type='text' 的 block，非 text block（如 image、resource_link）在当前 sprint 被忽略或返回友好提示
  - how: 发送 ContentBlock[] 含一个 type='image' 的 block，断言：(a) 不 crash，(b) agent 回复中提示不支持该类型或直接忽略，不影响 text block 的正常处理
- [C12] 两个同时存在的 session 互不干扰：各自的上下文、cwd 沙箱隔离
  - how: 同时创建两个 session（不同 cwd），对 session A 发 prompt "my name is Alice"，对 session B 发 prompt "my name is Bob"。随后分别问 session A 和 session B "what is my name?"，断言 session A 回复包含 "Alice"、session B 回复包含 "Bob"，互不污染