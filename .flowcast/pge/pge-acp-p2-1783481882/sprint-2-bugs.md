- [C1] dispatch() 的 method match 只处理 "initialize" 和 "notifications/initialized"，所有其他方法（包括 "session/new"）落入通配 arm 返回 METHOD_NOT_FOUND (-32601)。No session/new handler registered. session.rs 的 AcpSessionManager 存在但未连入 dispatch。
  - file: src/acp/server.rs:185
  - repro: cargo test acp::server::tests::session_new_returns_method_not_found -- --nocapture → 确认 session/new 返回 -32601

- [C2] session/new 未被 dispatch 路由，无法创建 session 和传入 cwd。AcpSession 结构（session.rs:31）持有 cwd: PathBuf，但没有任何代码从 NewSessionRequest.params.cwd 提取值并传入 AgentRuntime::run()。resolve_within 沙箱约束无法被触发测试。
  - file: src/acp/server.rs:185
  - repro: 对 ACP server 发 session/new {"cwd":"/tmp/acp-sandbox"} → 返回 METHOD_NOT_FOUND，无法进入 session 创建逻辑

- [C3] session/new 不被识别为有效方法。即使被路由，也未实现 cwd 存在性校验逻辑——session/new handler 不存在，自然没有路径校验或错误处理。
  - file: src/acp/server.rs:185
  - repro: 发 session/new {"cwd":"/nonexistent/path/xyz"} → 返回 {"error":{"code":-32601,"message":"Method not found: session/new"}}，不是 C3 要求的 cwd-specific JSON-RPC error

- [C4] session/prompt 不被 dispatch 识别。无 ContentBlock[] 解析代码（无对 PromptRequest.prompt 的迭代、type="text" 过滤、text 字段拼接）。src/acp/ 目录下 grep 'ContentBlock\|text.*block\|concatenat' 结果为空。
  - file: src/acp/server.rs:185
  - repro: grep -rn 'ContentBlock\|text_block\|prompt.*concat' src/acp/ → 0 matches

- [C5] src/acp/ 目录下 zero matches for EventSink、AgentEvent、session/update、agent_message_chunk。没有 EventSink impl 将 AgentEvent::Token 翻译为 session/update notification。dispatch() 返回 Vec<Value> 作通知，但从不生成 agent_message_chunk。
  - file: src/acp/
  - repro: grep -rn 'EventSink\|AgentEvent\|agent_message_chunk\|SessionUpdate' src/acp/ → 0 results

- [C6] stopReason='end_turn' 无任何生成路径。session/prompt 未实现 → AgentRuntime::run() 从未被调用 → 无法产生 AgentEvent::TurnFinished → 无 stopReason 映射。dispatch 返回值仅有 Vec<Value> initialized notification，无 stopReason 语义。
  - file: src/acp/server.rs:185
  - repro: grep -rn 'stopReason\|stop_reason\|TurnFinished' src/acp/ → 0 results

- [C7] messageId = SHA-256 前 12 字符 hex 未实现。src/acp/ 目录下 zero matches for sha256、SHA-256、message_id、MessageId。无 hashing 逻辑，无 messageId 生成，无跨 session 一致性验证。
  - file: src/acp/
  - repro: grep -rn 'sha256\|SHA-256\|message_id\|MessageId' src/acp/ → 0 results

- [C8] EventSink trait 存在（src/event.rs:244）且 AgentRuntime 支持注入（src/runtime.rs:1209），但 src/acp/ 目录下无任何 struct 实现 EventSink。grep 'impl.*EventSink' src/acp/ 返回空。run_inner 本身不包含 ACP 分支（src/run_core.rs 中 grep 'acp\|ACP' 为空），这一点符合 non-invasion 要求——但翻译层（EventSink impl）缺失，使 non-invasion 变成了 non-existence。
  - file: src/acp/
  - repro: grep -rn 'impl.*EventSink' src/acp/ → 0 results; grep 'acp\|ACP' src/run_core.rs → 0 results (no invasion, but also no sink)

- [C9] session/prompt 不被 dispatch 识别，对所有无效 sessionId 统一返回 METHOD_NOT_FOUND (-32601)，而非 C9 要求的 session-not-found 语义错误码（如 -32602 Invalid params 或自定义 session-not-found code）。ACPSessionManager.get() 逻辑存在但永远不会被调用。
  - file: src/acp/server.rs:185
  - repro: 发 session/prompt {"sessionId":"nonexistent-123","prompt":[{"type":"text","text":"hi"}]} → 返回 -32601 Method not found

- [C10] session/prompt 未实现 → 无法创建 session → 无法保持对话上下文。AcpSession 结构体有 turn: u64 字段但无任何代码递增它或累积 transcript。无法进行多轮对话。
  - file: src/acp/server.rs:185
  - repro: session/prompt 第一步就返回 -32601，无法测试上下文保留

- [C11] session/prompt 未实现 → 无 ContentBlock[] 解析逻辑 → 无 type 过滤 → 无 image/resource_link 友好降级。CoT 注释和 commit message 提过「仅处理 type='text'」，但实际代码中不存在对应的 match arm。
  - file: src/acp/server.rs:185
  - repro: grep -rn 'type.*text\|type.*image\|ContentBlock' src/acp/server.rs → 仅在注释/测试中出现; grep -rn 'ContentBlock' src/acp/session.rs → 0 results

- [C12] session/new 不被 dispatch 路由 → 无法创建两个并发 session → 无法验证隔离性。AcpSessionManager 使用 HashMap<SessionId, AcpSession> 为隔离提供了数据结构基础，但无任何代码创建或并发使用多个 session。
  - file: src/acp/server.rs:185
  - repro: 无法创建任何 session，隔离测试不可进行
