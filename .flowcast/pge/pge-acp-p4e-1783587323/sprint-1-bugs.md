- [S1-C1] 没有 E2E 测试能验证「发送 prompt → 立即 cancel → finishReason cancelled + 耗时 <2s」的完整场景。e2e/tests/ 目录中只有 01-acp-initialize.yaml 测试 initialize，无任何 cancel E2E 测试。
  - file: e2e/tests/:1
  - repro: 在 e2e/tests/ 下写一个新 YAML：先 session/new → session/prompt（用长指令）→ session/cancel，断言 stopReason:"cancelled" 且总耗时 <2s

- [S1-C2] parse_sse_stream 的 tokio::select! 实现已存在（openai.rs:643-651, anthropic.rs:332-340），但无单元测试验证「创建 CancellationToken + 慢 SSE 源 → 10ms 内 cancel → 函数在 100ms 内返回」。llm 模块的测试只测 mock server response，不测 cancel 时序。
  - file: src/llm/openai.rs:643
  - repro: 添加单元测试：创建 CancellationToken + 1 chunks/sec SSE 源，cancel 后 10ms 断言 tokio::time::timeout(100ms, parse_sse_stream(...)) 返回 Err(Error::Cancelled)

- [S1-C3] 无集成测试验证 TCP RST（reqwest::Response drop 后服务端检测到连接断开 <500ms）。代码中 parse_sse_stream 在 cancel 时 return Err 导致 Response 被 drop，但无测试观测服务端侧的 EOF/error。
  - file: src/llm/mod.rs:123
  - repro: 启动 1 byte/sec HTTP 服务器，发出 reqwest GET，cancel + drop Response，验证服务端 read loop 在 500ms 内检测到错误/EOF

- [S1-C4] 无测试验证 cancel 后 transcript 满足 Invariant #8（每一条带 tool_calls 的 Assistant 消息之后都有对应的 Tool result）。不存在在工具执行中 cancel 并 dump transcript 的 E2E 或集成测试。
  - file: src/acp/server.rs:658
  - repro: 集成测试：使 agent 在慢 shell 命令执行中 cancel，完成后 dump transcript，断言每条 Role::Tool 消息都被匹配的 Role::Assistant::tool_calls 引用

- [S1-C5] 整个 permission bridge 未实现。代码库中无 session/request_permission 通知的发射逻辑、无 permission 相关类型、无 bridge 到 client 的通道。protocol.rs 仅导出 ACP 协议类型但无运行时行为。
  - file: src/acp/server.rs:190
  - repro: 实现 PermissionBridge / 在 dispatch_async 中添加 session/request_permission 发射逻辑，每次工具调用前发送 notification 包含 toolName/path/command

- [S1-C6] 无 debouncing 代码。同一个 Assistant::tool_calls 中的多个工具调用没有合并逻辑。整个 permission/consolidation 子系统不存在。
  - file: src/acp/:1
  - repro: 实现 500ms 窗口的 debouncer：同一个 tool_calls 数组内的多个工具调用合并为一条 consolidated 通知，且 1s 内不再发第二条

- [S1-C7] 无去重逻辑。与 S1-C6 同一原因——整个 permission/debouncing 基础设施未实现。
  - file: src/acp/:1
  - repro: 实现 dedup：同 tool name + 同 path 在 500ms 窗口内合并为一条聚合条目（count:N），不同工具拆分为独立条目

- [S1-C8] PermissionOutcome 和 PermissionDecision 类型完全不存在。代码库中无这些枚举定义、无从 PermissionOutcome 到 PermissionDecision 的映射函数、无匹配 exhaustiveness 测试。
  - file: src/acp/protocol.rs:1
  - repro: 定义 PermissionOutcome (Allowed|Denied|Timeout) 和 PermissionDecision；编写 translate 函数；添加枚举穷举测试确保每变体都有对应映射

- [S1-C9] 常数 ACP_CLIENT_RPC_TIMEOUT_MS 不存在于 src/acp/server.rs 或任何位置。无 30s timeout 的 agent→client fs RPC 实现。server.rs 仅有一个 60s stdin idle timeout。
  - file: src/acp/server.rs:1
  - repro: 在 server.rs 顶部添加 `const ACP_CLIENT_RPC_TIMEOUT_MS: u64 = 30000;` 并支持 ACP_CLIENT_RPC_TIMEOUT_MS 环境变量覆盖；为 fs RPC 调用加上 CancellationToken + 30s timeout

- [S1-C11] 无 permission bridge 实现，因此无法发送 Allowed/Denied 决策。无测试验证 Allowed 后工具执行、Denied 后工具跳过。与 S1-C5/S1-C8 同一根因。
  - file: src/acp/server.rs:190
  - repro: 实现 client→server 的 permission 响应通道；写 E2E 测试：(a) respond Allowed → 工具结果出现在 transcript；(b) respond Denied → 工具被跳过且 run 继续

- [S1-C12] 有三个测试验证 ✓ valid session → result:true（session_cancel_valid_session_returns_result_true）和 ✓ nonexistent session → -32000。但遗漏了两个要求：(a) 取消已创建但从未运行的 session（非 running 状态）返回 -32000 而不是 result:true——而 handle_session_cancel 仅检查 session 是否存在，不检查是否有活跃 prompt；(b) 无测试验证 schema 中的 "success response is {'result':true}"（不是 {'cancelled':true}）——当前实现返回 Value::Bool(true) 是对的，但缺少针对性断言。
  - file: src/acp/server.rs:727
  - repro: (a) 在 handle_session_cancel 中检查 session 是否有正在执行的 turn（无 = 返回 -32000）；(b) 添加测试：发送 cancel 到已创建但未 prompt 的 session，断言 JSON-RPC error code -32000

- [S1-C13] AcpSession 的 cancel_token 字段类型是裸 `CancellationToken` 而非 `Arc<CancellationToken>`。refresh_cancel_token 中使用 `token.clone()`（CancellationToken 内部使用了 Arc 语义），但合同要求明确的 `Arc::clone()` 或 `Arc<CancellationToken>`，且无编译期类型断言。src/acp/ 内无任何 `Arc::new(cancel_token)` 调用。
  - file: src/acp/session.rs:42
  - repro: 将 AcpSession::cancel_token 类型改为 `Arc<CancellationToken>`；在 session/new 和 refresh_cancel_token 中用 Arc::new() 包装；添加编译期测试断言 parse_sse_stream 和 cancel handler 都持有 `Arc<CancellationToken>`

- [S1-C14] 三个边缘情况均缺失测试：(a) 取消从未运行的 session——当前返回 result:true，合同要求 -32000；(b) 连续两次相同 cancel——cancel_token.cancel() 幂等但无测试验证第一次返回 success、第二次不触发 double-cancel 副作用；(c) cancel 与自然完成竞态——无 tokio::join! 或定时控制测试验证 end_turn 只有 single stopReason。
  - file: src/acp/server.rs:1690
  - repro: 添加三个独立测试：(a) cancel 未运行 session → assert error code -32000；(b) 两次 cancel → 第一次 result:true，第二次 no-op；(c) tokio::join! cancel 和 prompt，断言 end_turn 只有一个 stopReason
