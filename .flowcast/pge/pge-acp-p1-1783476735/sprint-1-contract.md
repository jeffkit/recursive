# Contract: Sprint 1 — ACP Transport Foundation

Establish the stdio JSON-RPC transport loop for the ACP v1 protocol. Deliver `recursive acp` CLI entry point, `initialize` handshake with full capability declaration, spec-compliant error handling for malformed/unsupported requests, and clean stdout/stderr separation. Batching, notifications, and out-of-order response dispatch are structurally supported even though only `initialize` is wired in this sprint. Module-level transport contract is documented at the top of `src/acp/server.rs`.

## Criteria
- [C1] `recursive acp` starts and blocks on stdin as a JSON-RPC stdio server
  - how: Run `recursive acp --help` — output shows subcommand description and flags (--log-level, --home, --config). Run `echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' | recursive acp` — a valid JSON-RPC response is printed to stdout before the process exits (or times out waiting for more input).
- [C2] `initialize` request returns `protocolVersion: 1` and a complete `agentCapabilities` object
  - how: Pipe `{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}` into `recursive acp`. Assert response contains `result.protocolVersion` equal to `1` and `result.agentCapabilities` is a non-empty object with at minimum the keys that declare what this server supports (e.g. `mcp`, `session`, `tools`).
- [C3] Malformed JSON input produces a JSON-RPC ParseError (-32700) with a descriptive message
  - how: Pipe `not json` into `recursive acp`. Assert stdout contains `{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"..."}}`. The message must describe the problem (e.g. 'Parse error'), not a generic string.
- [C4] JSON-RPC requests missing the `jsonrpc` field produce InvalidRequest (-32600)
  - how: Pipe `{"id":1,"method":"initialize"}` into `recursive acp`. Assert error code `-32600` with a message indicating the `jsonrpc` field is required.
- [C5] Unknown method names produce MethodNotFound (-32601) with the offending method name in the message
  - how: Pipe `{"jsonrpc":"2.0","id":1,"method":"nonexistent","params":{}}` into `recursive acp`. Assert error code `-32601` and the message includes the method name (e.g. "Method not found: nonexistent").
- [C6] `initialize` with an unsupported `protocolVersion` returns a descriptive InvalidParams (-32602) error stating which versions are accepted
  - how: Pipe `{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":99}}` into `recursive acp`. Assert error code `-32602` and the message includes both the rejected version and the supported range (e.g. "unsupported protocol version 99; server supports 1").
- [C7] A batch of requests (JSON array) produces a batch of responses, with each response matching its request by `id`
  - how: Pipe `[{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}},{"jsonrpc":"2.0","id":2,"method":"initialize","params":{}}]` into `recursive acp`. Assert stdout is a JSON array of length 2, each element has the matching `id` and a valid `result`.
- [C8] A notification (JSON-RPC message without an `id` field) is consumed silently — no response is written to stdout
  - how: Pipe `{"jsonrpc":"2.0","method":"someNotification","params":{}}` into `recursive acp`, immediately followed by a valid initialize request with `id: 1` to flush the pipe. Assert stdout contains exactly one response (the initialize result), with no entry for the notification.
- [C9] All logging, diagnostics, and tracing output is directed to stderr only; stdout contains exclusively newline-delimited JSON-RPC messages
  - how: Run `recursive acp` with `--log-level debug` (or `RUST_LOG=debug`), pipe a valid initialize request. Capture stdout and stderr separately. Assert every stdout line is valid JSON (parsable by `jq .`) and contains at minimum `jsonrpc: "2.0"`. Assert stderr contains log output (e.g. timestamps, level markers, or message bodies).
- [C10] The stdin/stdout read loop reuses patterns from `McpServerRunner::run()` — async line-by-line read, per-message JSON deserialization, method dispatch, and write-back
  - how: Code review: open `src/acp/server.rs` and confirm the top-level `run()` or `serve()` method follows the same structure as the MCP server loop (async stdin BufReader, `serde_json::from_str` per line, match on method, `serde_json::to_string` + writeln to stdout).
- [C11] The transport contract is documented in a module-level doc comment at the top of `src/acp/server.rs`, covering: stdio framing (newline-delimited JSON), request/response matching by `id`, notification handling (no response), batch semantics (array in → array out), and the stderr-is-for-logs guarantee
  - how: Read the first ~50 lines of `src/acp/server.rs`. Assert a `//!` module doc comment exists and contains at minimum: (a) mention of newline-delimited JSON, (b) request/response id correlation, (c) notification behaviour, (d) batch array semantics, (e) stderr/stdout contract.
- [C12] `initialize` response is idempotent — calling it twice with different request ids returns identical capability declarations both times
  - how: Pipe `[{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}},{"jsonrpc":"2.0","id":2,"method":"initialize","params":{}}]` into `recursive acp`. Assert both `result.protocolVersion` are `1` and both `result.agentCapabilities` are deeply equal (same keys, same values).
- [C13] Large or deeply nested JSON-RPC payloads do not crash or hang the server
  - how: Pipe an initialize request with a `params` object containing a 10 KB string field into `recursive acp`. Assert the server still returns a valid initialize response within 2 seconds without panicking or truncating output.