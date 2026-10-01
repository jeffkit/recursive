---
type: Architecture
title: AG-UI — Frontend Protocol Layer
description: How Recursive serves the AG-UI protocol — protocol crate, client crate, transport-free server session layer, and the thin HTTP adapter.
tags: [agui, frontend, sse, interrupt, resume, copilot]
timestamp: 2026-10-01T15:30:00Z
---

# AG-UI

[AG-UI](https://docs.ag-ui.com) is the protocol Recursive uses to expose agent
runs to frontend clients (CopilotKit, `@ag-ui/client`, and the bundled
`agui-tui` reference frontend). The implementation is deliberately layered so
the protocol logic never depends on a transport:

| Layer | Location | Depends on |
|---|---|---|
| Protocol types + SSE parser | `crates/agui-protocol/` | nothing transport-related |
| Client (HTTP/SSE) | `crates/agui-client/` | `agui-protocol`, `reqwest` |
| Reference frontend (TUI) | `crates/agui-tui/` | `agui-protocol`, `agui-client` — **not** `recursive` |
| Server session layer | `src/http/agui.rs` | `agui-protocol`, core runtime — **no axum types** |
| HTTP adapter | `src/http/handlers.rs::agui_run` | axum (`State` / `Json` / `Sse`) |

The symmetry rule (Issue #56): the client side earns its transport freedom by
being a separate crate; the server side earns it by being a module whose only
axum-aware piece is the ~110-line `agui_run` adapter that parses the body,
maps errors onto status codes, and frames the event stream as SSE.

## Server session layer (`src/http/agui.rs`)

Everything AG-UI server-side lives here, in five pieces:

1. **`AguiConverter`** — stateful `AgentEvent → agui_protocol::Event`
   translator. Opens/closes `TextMessage*` framing across `PartialToken`
   deltas, anchors `ToolCall*` events to the last assistant message, and
   forwards hook/todo lifecycle as `Custom` events.
2. **Thread ↔ session mapping** — `agui_session::thread_session_key` maps an
   arbitrary client thread id onto the native session directory
   (`<sessions>/<workspace-slug>/agui-<blake3-16>/`, issue #57 — distinct
   thread ids cannot collide, and the thread IS a first-class session:
   `.meta.json`, cost, `SessionLock`). Pre-#57 flat `agui-<sanitized>/`
   directories are migrated on first resolve; the old lossy sanitiser
   survives only as `legacy_sanitize_thread_id` for those lookups.
   Open interrupts persist as `.interrupts.json` next to the transcript.
3. **`prepare_run`** — the resume/interrupt state machine (steps 1–3 of a
   run): derive the goal, validate resume coverage ("a resume must address
   every open interrupt"), reject runs that arrive while interrupts are open
   (`InterruptBeforeConflict` → HTTP 409), load + rewrite the transcript
   seed with client-supplied tool results. Transport-free: its input is a
   `RunAgentInput` plus a workspace path; its errors are a typed enum the
   HTTP adapter maps onto 400/409/500.
4. **`build_agui_runtime`** — assembles the per-run `AgentRuntime`: registers
   frontend-owned tools (`input.tools`) as stubs, installs the deny-before-
   dispatch permission hook that turns a frontend tool call into an
   interrupt, seeds the transcript, wires per-turn checkpoints.
5. **`spawn_agui_run`** — the driver task. Runs the agent, records metrics,
   persists the run into the thread's native session (`agui_session::persist_run`
   — uuid-chained transcript lines, `.meta.json` status/cost, `cost.json`),
   persists open interrupts **before** emitting them (crash safety), and
   emits `RunFinished` — always last, with `Interrupt` / `Success` / `Error`
   outcome as appropriate. The driver also holds the admission permit and
   the per-thread run-fence guard for the whole background run (issue #57
   §④ / #66), and removes the thread's cancel token from
   `AppState::agui_active_runs` when it finishes.

## Wire flow

```
POST /agui (RunAgentInput JSON)
  └─ handlers::agui_run            ← the ONLY axum-aware piece
      ├─ parse body → RunAgentInput (400 on shape errors)
      ├─ agui::prepare_run          (400/409/500 via typed error)
      ├─ admission.try_acquire_run  (503 when saturated, never waits)
      ├─ agui::build_agui_runtime
      ├─ agui::spawn_agui_run ────► mpsc<agui_protocol::Event>
      └─ frame as SSE ◄──────────── RunStarted … RunFinished
```

## Interrupt / resume round-trip

A frontend tool (`RunAgentInput.tools`) or a test-only `interrupt_before`
tool name triggers the interrupt path:

1. The permission hook denies the call before dispatch; the deny marker with
   the real `tool_call_id` lands in the transcript.
2. The driver persists the `OpenInterrupt` record, emits
   `StateSnapshot` + `MessagesSnapshot`, then `RunFinished{outcome:
   interrupt}` with a `response_schema` for the frontend's answer.
3. The frontend executes the tool and POSTs again with `resume:
   [{interruptId, status, payload}]`. `prepare_run` verifies the resume
   covers **all** open interrupts, rewrites the denied tool result in the
   seed transcript (payload, or a cancelled sentinel), and clears the store.

## Testing

Because everything except `agui_run` is transport-free, the resume coverage
rule, the interrupt-before conflict, the seed mapping (tool-role skipping per
Invariant #8), and the runtime assembly are all unit-tested in
`src/http/agui.rs` with a temp dir and no HTTP server. The through-HTTP
regression (full `messages` history seeding across turns) stays in
`src/http/handlers.rs` tests.

## See also

- `docs/INTERNALS.md` — HTTP server overview and admission gate.
- `docs/architecture/sessions.md` — transcript/session storage shared with
  the AG-UI session layer.
- `docs/architecture/invariants.md` — Invariant #8 (tool-call pairing),
  which the seed mapping exists to preserve.
