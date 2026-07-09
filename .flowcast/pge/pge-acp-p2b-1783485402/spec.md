# ACP Protocol Support for Recursive Agent

Make Recursive a first-class ACP v1 server that any ACP-compatible editor (Zed, JetBrains, Neovim) can use as a coding agent via stdio JSON-RPC. The 7-sprint plan covers: stdio transport with initialize handshake, full session lifecycle (new/prompt/cancel/load/resume), tool-call streaming with live status updates, permission bridging, editor filesystem proxy with sandbox enforcement, MCP multi-transport bridge, and CLI integration with comprehensive E2E + invariants tests. Sprint 0 (protocol types, 916-line implementation with 64 round-trip tests) is already committed.

## Sprints
### 1. Sprint 1 — stdio JSON-RPC loop + initialize handshake
- As a developer, I can run `recursive acp` and have it listen on stdin for newline-delimited JSON-RPC 2.0 messages, so any ACP client can establish a connection using the spec-mandated stdio transport.
- As an ACP client, I can send an `initialize` request and receive back `protocolVersion: 1`, `agentInfo` (name: recursive, version from Cargo.toml), and complete `agentCapabilities` (including session, load/resume, cancel, fs, mcp, permissions), so I know exactly what features this agent supports before creating a session.
- As a developer debugging ACP integration, I can see log output on stderr while stdout remains strictly protocol-only, so I never accidentally corrupt the JSON-RPC stream.
- As the Recursive codebase, the ACP server runner is a standalone adapter (`src/acp/server.rs`) that mirrors `McpServerRunner`'s stdin/stdout loop pattern without touching `run_core.rs::run_inner`, so Invariant #1 stays clean.

Mirror `src/mcp_server.rs::McpServerRunner::run()` pattern. Only `initialize` method. No sessions yet. AgentCapabilities must declare all planned features (even ones not yet implemented) so clients can discover them immediately.

### 2. Sprint 2 — session/new + session/prompt (text-only with streaming)
- As a developer using an ACP editor, I can call `session/new` with a `cwd` parameter, and Recursive creates a sandboxed session that treats that cwd as the filesystem root, so my project files are accessible but the agent cannot escape the workspace.
- As a developer, when I send a text prompt via `session/prompt`, I receive real-time `agent_message_chunk` notifications as the LLM generates text, so I see the agent's thinking appear incrementally in my editor.
- As a developer, when the agent finishes responding, I receive a `session/update` notification with `stopReason` (end_turn) and the completed message, so the editor knows the turn is complete and can render the final output.
- As the system, every message sent through ACP gets a stable `messageId` derived from content hash, so historical messages can be referenced consistently across load/resume cycles without modifying the transcript schema.
- As the codebase, the ACP session adapter converts `ContentBlock[]` from ACP wire format into Recursive's internal `Message` type using an `EventSink` that translates internal events into `session/update` notifications, so the bridge is a pure translation layer with no LLM-level awareness.

Text-only — no tool_call notifications yet (that's Sprint 3). The EventSink translation layer is the key architectural piece; it takes Recursive's internal `Event` stream and maps it to ACP wire notifications. sessionId uses existing session infrastructure from `src/session/`.

### 3. Sprint 3 — tool_call notifications + ToolKind + live status tracking
- As a developer, when the agent invokes a tool (Read, Edit, Bash, Glob, etc.), I see a `tool_call` notification appear in my editor showing the tool name, arguments, and a location hint (e.g., the file being edited), so I can follow along with what the agent is doing in real time.
- As a developer, I see tool executions progress through status states — `pending` when the tool call is first made, `in_progress` while it runs, and `completed` (or `failed`) when it finishes — so I can distinguish between tools that are queued, running, and done.
- As a tool implementer in the Recursive codebase, every `Tool` has a default `kind() -> ToolKind` method (Read→read, Edit→edit, Bash→execute, Glob→search, Write→write, WebFetch→fetch, WebSearch→web_search) so new tools automatically get reasonable ACP kind classification without boilerplate.
- As a developer, when a tool produces output, I see a `tool_call_update` notification with the result content, so I can inspect what the tool returned without digging through logs.
- As a developer, `locations` metadata (file paths, URLs) is extracted from tool arguments and attached to `tool_call` notifications, so my editor can show clickable links to the affected files.

Add `kind() -> ToolKind` as a default method on the `Tool` trait (returning `Other`), overridden by each concrete tool. The event→ACP mapping in `src/acp/events.rs` (or inline in server.rs) handles the state machine: first tool_call → pending, result arrival → completed/failed. ToolCallState tracking lives in the ACP session adapter, not in the kernel.

### 4. Sprint 4 — session/cancel + LLM stream abort + permission bridging
- As a developer, when I hit Escape or cancel in my editor, the ACP client sends `session/cancel`, Recursive immediately aborts the in-flight LLM stream at the SSE/transport level (reqwest::Response drop closes the TCP connection), and the turn finishes with `stopReason: "cancelled"`, so I never wait for the next chunk — cancellation is instant.
- As a developer, after I cancel a turn, the transcript remains structurally valid with every tool_call paired to its tool_result (failed tool calls get a synthetic error result), so subsequent turns and session/load work correctly without orphans.
- As a developer, when the agent wants to edit a file or run a command, my editor shows a permission dialog (via `session/request_permission`), I can Allow/Deny/Always Allow, and the agent continues or aborts based on my decision, so I stay in control of all filesystem and shell operations.
- As a developer, when the agent sends an `fs/read_text_file` request to read my editor buffer, the in-flight RPC is protected by a CancellationToken + 30-second timeout, so the agent never hangs indefinitely waiting for a response from a disconnected editor.
- As a developer maintaining Recursive, the cancel semantics are documented in a "Cooperative Cancel" section at the top of `src/acp/server.rs`, explaining that cancellation uses tokio::select! on a CancellationToken at both the SSE read loop level (provider streaming) and the ACP client RPC level (fs/* calls), so future contributors understand the two-phase abort design.
- As the codebase, `ChatProvider::stream` in both `src/llm/openai.rs` and `src/llm/anthropic.rs` uses `tokio::select!` on the cancel token alongside the SSE read, so when cancel fires, the reqwest::Response is dropped, the TCP connection closes, and the stream returns `Err(Error::Cancelled)` which `run_inner` already knows how to translate to `FinishReason::Cancelled`.

This is the most complex sprint. The critical path is: (1) modify both SSE parse loops to add tokio::select! on CancellationToken, (2) wire session/cancel → AgentRuntime::set_interrupt_token, (3) build PermissionHook→ACP bridge, (4) add 30s timeout to all agent→client RPCs, (5) write the cooperative-cancel documentation. May need to be split into P4a (cancel + stream abort) and P4b (permission bridge + RPC timeout) if scope is too large.

### 5. Sprint 5 — session/load + session/resume with history replay
- As a developer, when I reopen my editor after a break, `session/load` replays my entire conversation history as `session/update` notifications (user_message_chunk, agent_message_chunk, tool_call, tool_call_update), so my editor reconstructs the full context exactly as it was.
- As a developer, `session/resume` restores the agent's in-memory context without replaying the full history (for clients that already have it cached), so I can continue from where I left off without redundant data transfer.
- As a developer, when I call `session/load` or `session/resume` and my session had MCP servers configured, Recursive kills any stale MCP subprocesses from the previous session, starts fresh ones from the session's `mcpServers` config, and only then returns the result, so I never encounter ghost processes or stale connections.
- As an ACP client discovering capabilities, `SessionCapabilities` in the initialize response declares `resume: {}` and `loadSession: true`, so I know I can use both lifecycle methods.
- As the system, every replayed message gets a stable `messageId` (content hash), so the client can correlate replayed messages with its own cache and avoid duplicates.

Load replays from SessionStore transcript; resume just restores AgentRuntime context. Both must handle MCP lifecycle (kill old, start new). The messageId stability guarantee from Sprint 2 is critical here — without it, the client can't deduplicate replayed messages.

### 6. Sprint 6 — editor filesystem proxy + MCP multi-transport + session/close cleanup
- As a developer, when my editor declares `fs.readTextFile=true` in `session/new`, the agent reads from my editor's buffer (via `fs/read_text_file` RPC) instead of the on-disk file, so the agent sees my unsaved changes without me having to save first. The sandbox path check still runs, so the agent can't escape the workspace.
- As a developer, when the agent writes a file and my editor supports it, the write goes to my editor buffer via `fs/write_text_file` RPC, so I can review the changes in-editor before they hit disk.
- As a developer, when my editor doesn't support fs capabilities, the agent gracefully falls back to local `Read`/`Write` tools, so the session still works — just without the editor-buffer integration.
- As a developer configuring MCP servers in `session/new`, I can specify stdio servers (Recursive spawns the subprocess), HTTP servers (Recursive connects to a URL), or SSE servers (Recursive connects to an SSE endpoint), and all three transports work using a single unified MCP bridge adapter.
- As a developer, when I call `session/close`, Recursive kills all MCP subprocesses spawned for that session, removes the session from the registry, and leaves no zombie processes behind, so I can open and close many sessions without resource leaks.
- As a developer with both global MCP config and session-level MCP servers, session-level servers take priority when names conflict, so my session-specific tool configs override global defaults without manual merging.

ClientReadFile/ClientWriteFile are new tools that wrap the fs/* ACP RPC. Sandbox validation always runs: the agent→client RPC path is checked against resolve_within before the client even sees it. MCP bridge in `src/acp/mcp_bridge.rs` handles config format conversion from ACP's mcpServers to Recursive's MCP client format. session/close cleanup must be idempotent (safe to call multiple times).

### 7. Sprint 7 — CLI integration + E2E tests + invariants enforcement
- As a developer, I can run `recursive acp` as a top-level CLI subcommand alongside `recursive mcp` and `recursive http`, and `recursive --help` shows all three transport modes, so the ACP entrypoint is discoverable from the command line.
- As a QA engineer, there is an E2E test that runs a scripted ACP client through the full lifecycle — initialize → session/new → session/prompt (tool-calling prompt) → observe tool_call notifications → session/cancel → session/load → verify history replay — so every sprint's behavior is regression-proofed end-to-end.
- As the codebase, invariants tests verify: (1) no ACP code branches into `src/run_core.rs::run_inner` (AST-level check), (2) all ACP fs operations pass through `resolve_within` sandbox validation, (3) cancel paths preserve tool-call↔tool-result pairing in the transcript (Invariant #8), so the architectural guarantees are mechanically enforced, not just documented.
- As a developer using Zed, I can configure `recursive acp` as my coding agent and complete a real editing task — the agent reads files, makes edits, responds to cancellation, and the editor shows tool progress live — so the integration is production-usable.
- As the release process, `cargo test --workspace`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo fmt --all -- --check` all pass clean with zero warnings and zero unwrap/expect in non-test code, so the ACP feature meets all Recursive quality gates.

E2E tests follow CLAUDE.md rules: use argusai + recursive-e2e container, Port 9095 (next available in the registry), test binary is the container's `recursive`. Invariants tests live in `tests/invariants/` alongside the existing loop_size_orthogonality test. The Zed manual acceptance test is the final smoke test but is not automated (it requires a real Zed instance).
