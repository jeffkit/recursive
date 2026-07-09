- [AC-S1-03] session/new returns METHOD_NOT_FOUND (-32601) because `AcpServer::run()` passes `None` as LLM provider. The dispatch_async function requires `Some(llm)` to route to session methods. Unit tests pass (they use mock LLM), but the binary wiring is incomplete.
  - file: crates/recursive-cli/src/main.rs:665
  - repro: echo '{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp"}}' | ./target/debug/recursive acp 2>/dev/null → {"error":{"code":-32601,"message":"Method not found: session/new"}}

- [AC-S1-04] Same root cause as AC-S1-03. session/new unreachable because no LLM provider is wired. Unit tests for cwd validation (nonexistent, not-a-directory, path traversal) all pass with mock LLM.
  - file: crates/recursive-cli/src/main.rs:665
  - repro: Same as AC-S1-03.

- [AC-S1-05] session/prompt returns METHOD_NOT_FOUND because no LLM provider in the real binary. Unit tests (`session_prompt_emits_agent_message_chunk_and_stop_reason`) pass with mock LLM.
  - file: crates/recursive-cli/src/main.rs:665
  - repro: Any session/prompt call after initialize → -32601.

- [AC-S1-06] end_turn notification and stopReason mapping cannot be tested via real binary because session/prompt is unreachable. Unit test `ac25_stop_reason_end_turn_for_normal_completion` passes.
  - file: crates/recursive-cli/src/main.rs:665
  - repro: Same as AC-S1-05.

- [AC-S1-07] Cannot verify text-only constraint via real binary. Unit test `ac27_event_sink_only_allowed_notifications` passes — asserts only user_message_chunk, agent_message_chunk, end_turn are emitted.
  - file: crates/recursive-cli/src/main.rs:665
  - repro: Same as AC-S1-05.

- [AC-S1-10] session/prompt with nonexistent sessionId returns -32601 (Method not found) instead of -32001 (Session not found) because the method itself is not wired. Unit test `session_prompt_invalid_session_returns_error` passes with SESSION_NOT_FOUND code when LLM is available.
  - file: crates/recursive-cli/src/main.rs:665
  - repro: Any session/prompt call → error code -32601, not in range -32099..=-32000.

- [AC-S1-12] Cannot create sessions to test messageId determinism. Unit test `ac26_message_id_deterministic_across_sessions` passes — same prompt content produces identical 12-char lowercase hex messageIds across different sessions.
  - file: crates/recursive-cli/src/main.rs:665
  - repro: Same as AC-S1-03.

- [AC-S1-16] Cannot test session/prompt parameter validation via real binary. Unit tests pass: missing prompt field → INVALID_PARAMS, empty array → error/helpful response, non-array → INVALID_PARAMS (via schema validation).
  - file: crates/recursive-cli/src/main.rs:665
  - repro: Same as AC-S1-05.


## Evaluator 输出失败

Evaluator 跑结构化输出失败（3 次重试仍非合法 JSON）。这通常是 evaluator 模型自身的输出格式问题，不一定是代码错。
请 generator 重新自评一遍 contract 各验收点，确认实现无误。

## Evaluator 输出失败

Evaluator 跑结构化输出失败（3 次重试仍非合法 JSON）。这通常是 evaluator 模型自身的输出格式问题，不一定是代码错。
请 generator 重新自评一遍 contract 各验收点，确认实现无误。