# ACP Protocol Support for Recursive Agent

Enable Recursive to serve as an ACP v1 server, communicating with editors (Zed, JetBrains, etc.) via stdio JSON-RPC. This makes Recursive a drop-in coding agent for any ACP-compatible client. Delivered across 7 sprints covering transport, session management, tool visibility, cancellation safety, permission bridging, editor FS integration, MCP ecosystem, session persistence, CLI, and end-to-end validation. AI features woven throughout: smart failure explanations, permission intent summaries, and context-aware session resume.

## Sprints
### 1. Sprint 1 — ACP Transport Foundation
- As a Zed user, I can point my editor to `recursive acp` and it connects via stdio JSON-RPC
- As a client developer, `initialize` returns `protocolVersion: 1` and complete `agentCapabilities` so I know which features are available before making any calls
- As an operator, malformed JSON-RPC or unsupported protocol versions return spec-compliant error codes with descriptive messages, not generic 500s
- As a developer integrating ACP, the transport loop handles concurrent requests gracefully — batching, out-of-order responses, and notification-only messages all work
- As a debugger, all logging and diagnostics go to stderr so stdout stays a clean newline-delimited JSON-RPC stream

Foundation sprint. Reuse patterns from McpServerRunner::run() for the stdin/stdout loop. Implement `initialize` with full capability declaration upfront — this handshake gates all future sprints. Verify with a hand-crafted JSON-RPC request piped through stdin/stdout. Decision 4c prep: document the transport contract at the top of src/acp/server.rs from day one.

### 2. Sprint 2 — Agent Sessions & Streaming Response
- As a developer, `session/new` creates an isolated agent session with my project directory as the sandbox root, so the agent can only touch files within my project
- As a developer, `session/prompt` sends my coding question to Recursive and I see the LLM response stream character-by-character via `agent_message_chunk` notifications
- As a developer, when the agent finishes its turn, I receive an `end_turn` event with a clear `stopReason` so my editor knows the response is complete
- As a developer, I can send follow-up prompts in the same session and the agent maintains full conversation context from all previous exchanges
- As an SDK developer, `session/update` notifications arrive in causal order with monotonically increasing sequence numbers, making client-side rendering deterministic

First end-to-end integration: connect AgentRuntime to ACP wire. EventSink maps internal events to session/update notifications. Decision 2: stable messageId via content hash (SHA-256 of role + content + tool info) — no transcript schema changes needed. No tool calls yet, pure text conversation. The 'wow' moment: ask Recursive a question through Zed and watch the answer stream in.

### 3. Sprint 3 — Tool Visibility & Intelligent Status
- As a developer, I can see what the agent is doing — reading files, editing code, running commands, searching — with semantic icons and labels in my editor's activity panel
- As a developer, tool execution shows a live progress lifecycle: pending → in_progress → completed (or failed), so I know what's happening at a glance
- As a developer, clicking on file locations from tool_call events navigates me directly to the affected code in my editor
- As a developer, long-running commands like `cargo build` stream partial output via `tool_call_update` so I can see build progress in real-time without waiting
- As a developer, when a tool fails, I get a human-readable summary of what went wrong (e.g. 'compilation error: missing semicolon at src/main.rs:42') rather than a raw stderr dump
- As a tool author, I declare my tool's `kind()` once in the Tool trait impl and it propagates automatically through the ACP notification layer

Tool trait gets a default `kind() -> ToolKind` method. Tools override: Read→read, Write/Edit→edit, Bash→execute, Glob/Grep→search, WebFetch→fetch, WebSearch→search. AI angle: the agent's own LLM generates concise failure summaries from raw tool output, so the editor shows actionable error messages rather than opaque stack traces. Locations extracted from tool path/pattern parameters.

### 4. Sprint 4 — Interrupt Safety & LLM Stream Abort
- As a developer, hitting 'stop' in my editor immediately cancels the agent's current work — I don't wait for the next token or tool completion
- As a developer, cancelled sessions show a calm 'cancelled' status in my editor, not a scary red error banner that makes me think something broke
- As a developer, after cancelling mid-edit, my conversation transcript is intact — every tool call has its corresponding result, no orphaned messages breaking the chat view
- As a developer, if the agent is waiting for my permission when I cancel, the permission prompt dismisses and the session cleanly terminates
- As an extension developer, I can read the documented 'collaborative cancel' contract in the code to understand exactly what guarantees hold after cancellation
- As a developer, cancellation works regardless of what the agent was doing — thinking (LLM stream), executing a tool, waiting for permission, or between steps

Most complex sprint — touches provider layer and run loop. Three changes: (1) ChatProvider::stream gets tokio::select! on CancellationToken in SSE loop — reqwest::Response drops on cancel, closing TCP connection. (2) session/cancel sets AgentRuntime interrupt token; run_inner already maps this to FinishReason::Cancelled (Invariant #7 satisfied). (3) ACP bridge maps FinishReason::Cancelled → stopReason: 'cancelled' (not error). Decision 4b: implemented in src/llm/openai.rs and src/llm/anthropic.rs SSE parse functions. Decision 4a: in-flight agent→client fs/* RPCs get CancellationToken + 30s timeout. Decision 4c: src/acp/server.rs doc section on collaborative cancel semantics. Invariant #8 verified: tool-call↔tool-result pairing preserved post-cancel.

### 5. Sprint 5 — Permission Bridge & Editor FS Integration
- As a developer, when the agent wants to run a shell command, I get an in-editor permission prompt showing what command it wants to run and why, and I can approve or deny with one click
- As a developer, I can configure per-session auto-approval rules so routine operations like `cargo check` don't interrupt me, while destructive commands still require confirmation
- As a developer, the agent reads my unsaved editor buffers — I can highlight code and ask 'explain this function' without saving first
- As a developer, the agent writes changes back to my editor as uncommitted diffs I can review, accept, or reject before saving to disk
- As a security-conscious developer, all editor FS access still passes through sandbox validation — the agent can never escape my project root, even through editor buffers
- As a developer using a simpler editor, if my client doesn't support editor FS (`fs.readTextFile` not declared), the agent gracefully falls back to reading from disk

Two user-facing capabilities: permissions and editor FS. PermissionHook bridges to ACP session/request_permission → PermissionOutcome → PermissionDecision round-trip. Client FS: ClientReadFile and ClientWriteFile tools that check client capabilities first, fall back to local Read/Write with full sandbox enforcement. Decision 1 implemented. AI angle: permission prompts include a brief 'why' explanation generated by the agent ('I need to run cargo test to verify my refactoring didn't break anything'), helping users make informed trust decisions.

### 6. Sprint 6 — Session Persistence & MCP Ecosystem
- As a developer, I close Zed at the end of the day, reopen it tomorrow, and my entire conversation with Recursive is restored — every message, tool call, and result
- As a developer, `session/resume` picks up exactly where I left off with full context, but without replaying old messages into my chat view
- As a developer, resuming a session shows me a brief AI-generated summary of what we were working on ('You were refactoring the auth module; I had just renamed validateUser to authenticateUser…') so I instantly regain context
- As a developer, my configured MCP servers from `~/.recursive/mcp.json` are available to Recursive alongside its built-in tools, and per-session MCP configs override global ones when names conflict
- As a developer, MCP servers connect over stdio (local subprocess), HTTP, or SSE (remote URL) depending on my config, giving me flexibility for both local and remote tools
- As a developer, when I close or reload a session, all MCP stdio subprocesses are killed — no zombie processes silently consuming resources
- As a developer, resuming a session with updated `mcpServers` in the config kills the old set of servers and starts the new ones before the agent continues, so my tool set is always current

Two large features bundled: session history and MCP bridge, because they share lifecycle concerns (both must clean up on close/resume). Decision 2: stable messageId via SHA-256 content hash. Decision 5.1-5.4: MCP bridge supports stdio (spawn subprocess), http, sse (remote connect). Session-scoped MCP registry with cleanup on session/close and session/load. Decisions 5.3/5.4: kill old stdio servers before starting new ones. AI angle: the session summary on resume uses the agent's own LLM to generate a concise, context-rich recap from the last few turns of the transcript.

### 7. Sprint 7 — CLI, E2E & Ship
- As a developer, I type `recursive acp` and Recursive starts as an ACP server on stdio, ready for any ACP-compatible editor to connect
- As a Zed user, 'Recursive' appears in the agent selection dropdown and works as a first-class coding agent alongside the built-in options
- As a developer, `recursive acp --help` shows all available options (log level, home dir, config path) consistent with the `mcp` and `http` subcommands
- As a release engineer, the full ACP flow passes automated E2E tests covering: initialize → new session → prompt with tool calls → cancel mid-execution → load history → resume
- As a maintainer, invariants tests verify that: (1) ACP code never branches inside run_inner, (2) all ACP-initiated FS operations go through resolve_within, (3) tool-call↔tool-result pairing survives cancellation
- As an early adopter, I can use `recursive acp` with any ACP v1 client — not just Zed — and it follows the spec faithfully with no implementation-specific quirks

Ship sprint. CLI: Acp variant in crates/recursive-cli with same arg conventions as Mcp/Http. E2E: argusai-based test driving a scripted ACP client through the full lifecycle, asserting correct notification sequences and event types at each stage. Invariants: (1) AST grep confirms no ACP references in src/run_core.rs::run_inner, (2) resolve_within trace confirms all FS ops are sandboxed, (3) transcript integrity test confirms tool-call/tool-result pairing after cancel. Final manual acceptance: Zed connects to recursive acp and completes a real coding task end-to-end.
