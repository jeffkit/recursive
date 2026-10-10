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
   seed with client-supplied tool results, and — for a thread that already
   has a persisted transcript — seed from *that* transcript instead of the
   request's `messages` (issue #147, see below). Transport-free: its input is
   a `RunAgentInput` plus a workspace path; its errors are a typed enum the
   HTTP adapter maps onto 400/409/500.
4. **`build_agui_runtime`** — assembles the per-run `AgentRuntime`: registers
   frontend-owned tools (`input.tools`) as stubs, installs the deny-before-
   dispatch permission hook that turns a frontend tool call into an
   interrupt, seeds the transcript, wires per-turn checkpoints.
5. **`spawn_agui_run`** — the driver task. Runs the agent, records metrics,
   and persists the run into the thread's native session as it goes: the
   driver holds the thread's `SessionWriter`
   (`agui_session::open_thread_writer`) for the whole run and shares it with a
   `SessionPersistenceSink`, so every completed step is a row in
   `transcript.jsonl` the moment the kernel appends it;
   `agui_session::finalize_run` then records what the run as a whole
   concluded (`.meta.json` status / `finish_reason` / `error`, plus
   `cost.json`). It persists open interrupts **before** emitting them (crash
   safety), and emits `RunFinished` — always last, with `Interrupt` /
   `Success` / `Error` outcome as appropriate. The driver also holds the
   admission permit and the per-thread run-fence guard for the whole
   background run (issue #57 §④ / #66), and removes the thread's cancel token
   from `AppState::agui_active_runs` when it finishes.

## The thread id is the session (issue #147)

An AG-UI thread *is* a native session (#57), and that session's transcript is
the conversation of record. What a client sees:

- **`messages` is this turn's prompt, not the history.** For a thread the
  server has already persisted, the run is seeded from the thread's
  `transcript.jsonl`; only the request's last user message is used, as the
  turn's goal (with `input.context[0]` and, for an empty `messages`, a neutral
  continuation directive as the fallbacks — `messages: []` is legal there). A
  thread the server has **never** seen keeps the pre-#147 behaviour: standard
  clients (CopilotKit, `@ag-ui/client`) resend the full history, it seeds the
  run, and it is written to the thread's transcript before the run — so the
  *next* request may drop it.
- **Documented cost:** a client-side rewrite or trim of the history no longer
  takes effect. History is server-held; a client that must curate it starts a
  new thread id.
- **Crash window is one message.** Rows land as the kernel appends them, so a
  host killed mid-run keeps every completed step (and its token cost) instead
  of nothing. A tail the kill left unpaired — an assistant `tool_calls` whose
  result never arrived — is answered before the next run with the same marker
  `recursive resume --orphans=skip` writes, because an unanswered call is an
  HTTP 400 (Invariant #8) and would otherwise leave the thread unusable.


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
Invariant #8), the thread-seed loading (#147: system / orphan handling) and the
runtime assembly are all unit-tested in `src/http/agui.rs` with a temp dir and
no HTTP server. The through-HTTP regression (full `messages` history seeding
across turns) stays in `src/http/handlers.rs` tests, and the thread-id contract
— `messages: []` on an existing thread, one row per message, seeded history
written down before the run — is driven end to end in `tests/agui_e2e.rs`.

## See also

- `docs/INTERNALS.md` — HTTP server overview and admission gate.
- `docs/architecture/sessions.md` — transcript/session storage shared with
  the AG-UI session layer.
- `docs/architecture/invariants.md` — Invariant #8 (tool-call pairing),
  which the seed mapping exists to preserve.
