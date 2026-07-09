# ACP Protocol Support — Sprint 2–5 (P4–P7)

Complete the ACP v1 server implementation for Recursive, starting from P4 (the most complex sprint involving LLM stream abort and permission bridge) through P7 (CLI, E2E, invariants). Each sprint builds on the prior one: first the cancel/permission coordination layer, then history replay and editor-side filesystem access, then MCP multi-transport with lifecycle cleanup, and finally integration testing. AI-native features woven throughout include predictive cancellation heuristics, adaptive permission debouncing, auto-retry MCP transport with exponential backoff, and anomaly detection on tool output to warn the user of unexpected patterns.

## Sprints
### 1. Sprint 1 — Cancel Coordination & Permission Bridge (P4)
- As an agent user, I want to cancel a running prompt immediately so I don't wait for a response I no longer need.
- As an agent user, I want the LLM stream to abort within one chunk cycle (not wait for the next SSE event) so cancellation feels instant.
- As an ACP client, I want `session/cancel` to return `stopReason: "cancelled"` as a normal finish reason, not an error, so I can distinguish cancellation from failure.
- As an ACP client, I want the transcript after cancellation to preserve all tool-call↔tool-result pairings so the conversation history remains coherent.
- As an ACP client, I want `session/request_permission` notifications to be sent for every tool/shell action that requires approval, with structured context (tool name, path, command).
- As an agent user, I want permission decisions to be debounced so rapid-fire tool calls (e.g., 3 Read calls) consolidate into a single prompt instead of three separate popups.
- As an ACP client, I want `PermissionOutcome` responses to translate deterministically to `PermissionDecision` so the agent can proceed or abort without ambiguity.
- As a developer, I want the cancel token to be wired into `parse_sse_stream` in both OpenAI and Anthropic providers via `tokio::select!` so the stream dies on drop, not on the next chunk boundary.

This is the highest-risk sprint. Key non-obvious traps: (1) `reqwest::Response::drop()` must close the TCP connection immediately — verify with a slow-stream test that the HTTP body stops within 50ms of cancel; (2) Invariant #8 means after cancel we may need to synthesize a `ToolResult` for any tool that was in-flight — the EventSink must not leave orphan `Role::Assistant` with pending `tool_calls`; (3) Permission debouncing is an AI feature: gather all pending permission requests within a 500ms window, deduplicate, and present one consolidated prompt with counts. Document the cooperative cancel semantics in a doc comment block at `src/acp/server.rs` top per decision 4c. Also wire `CancellationToken` + 30s timeout for in-flight agent→client fs RPCs (decision 4a).

### 2. Sprint 2 — History Replay, Resume, & Editor Filesystem (P5 + P6 editor fs)
- As an ACP client, I want `session/load` to replay the full transcript via `session/update` notifications (user_message_chunk, agent_message_chunk, tool_call) so the client can reconstruct conversation history without storing it locally.
- As an ACP client, I want `session/resume` to restore the agent's context without replaying notifications so I can continue an interrupted session with low latency.
- As an agent user, I want resumed sessions to carry forward MCP server connections from the original session so I don't need to re-register tools.
- As an ACP client, I want `session/load` and `session/resume` to kill stale stdio MCP subprocesses from the previous run before starting new ones, preventing process leaks.
- As an agent user, I want the agent to read files from the editor's buffer when `fs.readTextFile=true` is declared, falling back to local disk read otherwise, so unsaved edits are visible to the agent.
- As an agent user, I want sandbox enforcement to run on every editor-file read/write regardless of the source (client buffer or local disk), preventing path traversal even through the editor path.
- As an ACP client, I want `messageId` to be a stable content hash so I can deduplicate messages and match `session/load` notifications against my local cache.
- As a developer, I want session/load to return `result=null` after replay is complete, not the conversation result, so the client knows replay finished without confusion.

Session/load replay is the most notification-heavy operation — each message is one or more `session/update` notifications, so a 200-turn conversation sends 400+ JSON-RPC messages. The ACP client must handle backpressure. The stable messageId (SHA-256 of content bytes, truncated to first 16 bytes as hex) is a design decision that's already approved (decision 2) — do not revisit. Editor fs integration (decision 1) adds two new tools (`ClientReadFile`, `ClientWriteFile`) that sit alongside the existing `Read`/`Write` tools — both paths hit `resolve_within`. AI feature: during replay, inject an AI-generated summary notification at the end (`agent_message_chunk` with a 2-sentence recap of what happened before) to help the resumed agent orient itself faster — this is optional and gated on `RECURSIVE_ACP_SUMMARIZE_REPLAY=1`.

### 3. Sprint 3 — MCP Multi-Transport & Session Lifecycle Cleanup (P6 remainder)
- As an agent user, I want the agent to connect MCP servers over stdio (local subprocess), HTTP (remote API), and SSE (streaming remote) so any MCP server topology works with ACP.
- As an ACP client, I want `mcpCapabilities` to declare `{ http: true, sse: true }` so I know which transports the agent supports without probing.
- As an ACP client, I want MCP servers declared in `session/new`'s `mcpServers` config to override global MCP config when names conflict, giving me fine-grained control per session.
- As an agent user, I want `session/close` to gracefully kill all stdio MCP subprocesses (SIGTERM → 3s grace → SIGKILL) so no zombie processes remain after a session ends.
- As an agent user, I want `session/load` and `session/resume` to also trigger MCP cleanup + re-init so stale connections don't survive across sessions.
- As a developer, I want the MCP bridge to retry failed transport connections with exponential backoff (100ms, 500ms, 2s, 5s cap) so transient network issues don't kill the session.
- As a developer, I want the transport retry logic to have an AI plausibility check before each retry — if the server returned a permanent error (401, 403, incompatible protocol version), stop retrying immediately and report the failure.

The transport retry with AI plausibility check is an AI-native pattern: classify error responses as transient (network-level) vs permanent (auth, protocol mismatch). Permanent errors skip retry and surface immediately to the user. This saves 10+ seconds of futile retry on misconfigured servers. The process cleanup must use process-group kill (killpg) because stdio MCP subprocesses may fork their own children. Test: `ps --ppid <mcp-pid> --forest` after session/close must show zero descendants. Use `src/mcp.rs` as the extension point — add HTTP and SSE transport variants alongside the existing stdio logic (decision 5.1), not a new module. Session-scoped MCP registry lives in `src/acp/mcp_bridge.rs` and shadows global config entries per decision 5.2.

### 4. Sprint 4 — CLI Subcommand, E2E Tests & Invariants (P7)
- As a developer, I want `recursive acp` to be a CLI subcommand alongside `mcp` and `http` so I can start the ACP server with `recursive acp --provider openai` and pipe stdio JSON-RPC.
- As a developer, I want a scripted E2E test that sends an `initialize` → `session/new` → `session/prompt` → cancel sequence over stdio and asserts the exact notification order, so regressions are caught automatically.
- As a developer, I want the E2E test to verify that after cancel, the transcript JSONL has `FinishReason::Cancelled` and all tool-call→tool-result pairs are intact (Invariant #8).
- As a developer, I want an invariants AST test that confirms no ACP branch exists inside `run_inner`, so Invariant #1 stays enforced across refactors.
- As a developer, I want an invariants test that traces every fs operation in the ACP module through `resolve_within`, so sandbox escapes are caught at compile-adjacent time.
- As a developer, I want `RECURSIVE_ACP_SANDBOX_STRICT=1` to switch from warn-mode to deny-mode for sandbox violations, so production deployments are locked down by default.
- As a Zed user, I want to configure `recursive acp` as my ACP agent and see tool calls, file edits, and terminal commands stream into the editor UI in real time.

The E2E test follows the `argusai` framework pattern from `e2e/tests/` — see rules in CLAUDE.md about `unset RECURSIVE_SESSIONS_DIR` and port isolation. The test binary should be a Rust `#[cfg(test)]` integration test or a standalone Python script — prefer Rust to avoid Python dependency issues in the worktree. The invariants AST tests use `syn` to parse the source and assert no function/method with name containing `run_inner` exists in any file under `src/acp/`. This is cheap (<50ms) and runs as part of `cargo test`. Add `RECURSIVE_ACP_SANDBOX_STRICT` env var check in `src/acp/server.rs` init — if set and a violation occurs, the tool returns an error instead of a warning. AI feature in this sprint: an intelligent failure-mode detector that, when the agent detects repeated ACP transport errors (3+ in 60s), auto-suggests the most likely fix (e.g., "It looks like your MCP server at localhost:3000 is returning 502. Try restarting it with `brew services restart my-mcp`"). This is a `ToolResult`-level annotation that appears as a machine-readable `faultCode: string` field in the end_turn notification.
