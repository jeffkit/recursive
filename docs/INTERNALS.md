# Recursive Internals — Call Chain & Lock Hierarchy

**Last updated**: 2026-07-05 (P1-2 from the architecture review)

This document describes how a request flows from the HTTP / TUI / CLI
surfaces down to the kernel's ReAct loop, and which synchronization
primitive guards which piece of state at each layer. Read this before
adding a new entry point, introducing a new piece of cross-turn state,
or wiring a new interactive surface (websocket, MCP server, etc.) onto
`AgentRuntime`.

For the *what* and *why* of the runtime/kernel split, read
[`docs/architecture/agent-loop.md`](architecture/agent-loop.md) first.
This document is the *how-is-it-wired* companion.

---

## 1. Layered call chain

```
              HTTP request                 TUI action               CLI args
                  │                            │                       │
                  ▼                            ▼                       ▼
        ┌──────────────────┐        ┌──────────────────┐    ┌──────────────────┐
        │ axum handlers    │        │ tui::backend     │    │ cli::commands    │
        │ (http/handlers)  │        │ (worker task)    │    │ (cli::run_*)     │
        └────────┬─────────┘        └────────┬─────────┘    └────────┬─────────┘
                 │                           │                       │
                 │                           │ both call             │
                 ▼                           ▼                       ▼
        ┌──────────────────────────────────────────────────────────────────┐
        │ AgentRuntimeBuilder::new()...build() -> AgentRuntime              │
        │ (src/runtime.rs)                                                  │
        └────────┬─────────────────────────────────────────────────────────┘
                 │
                 ▼
        ┌──────────────────────────────────────────────────────────────────┐
        │ AgentRuntime (pub API: run, enqueue, set_event_sink, ...)        │
        │   owns: transcript, event_sink, kernel, checkpoints, ...         │
        └────────┬─────────────────────────────────────────────────────────┘
                 │ run() / enqueue() per turn
                 ▼
        ┌──────────────────────────────────────────────────────────────────┐
        │ AgentKernel::run(ctx: TurnContext) -> TurnOutcome                │
        │ (src/kernel.rs) — stateless turn executor                         │
        └────────┬─────────────────────────────────────────────────────────┘
                 │ builds RunCore, calls run_inner()
                 ▼
        ┌──────────────────────────────────────────────────────────────────┐
        │ RunCore::run_inner() — the actual ReAct step loop                │
        │ (src/run_core.rs) — 117-line loop, 7 sibling phase helpers       │
        │   dispatch_llm_step, handle_no_tool_calls, process_tool_results, │
        │   check_shutdown, enforce_transcript_budget, drain_mailbox,      │
        │   make_outcome                                                   │
        └────────┬─────────────────────────────────────────────────────────┘
                 │ per step
                 ▼
        ┌──────────────────────────────────────────────────────────────────┐
        │ ChatProvider::complete() + ToolRegistry::invoke()                │
        │ (src/llm/*, src/tools/*)                                         │
        └──────────────────────────────────────────────────────────────────┘
```

### Key shapes

- **`AgentRuntime`** is stateful — it owns the transcript and accumulates
  state across turns. Constructed per-session (HTTP, TUI) or per-process
  (CLI).
- **`AgentKernel`** is "stateless w.r.t. the transcript" — it holds
  long-lived config (LLM provider, tool registry, hooks) but takes the
  transcript in via `TurnContext` each turn. `AgentKernel::run` builds a
  fresh `RunCore` per turn, `RunCore` is consumed by `run_inner`, and the
  outcome is folded back into `AgentRuntime`'s transcript.
- **`RunCore::run_inner`** is a 117-line `loop {}` over phase helpers
  (see P1-1, PR #8). The body must stay small — gated by the
  `run_inner_function_body_stays_small` invariant test (≤ 150 lines).

---

## 2. Synchronization primitives — what guards what

| Primitive | Where declared | What it guards |
|---|---|---|
| `tokio::sync::Mutex<AgentRuntime>` | `http/handlers.rs`, `tui/backend.rs` | **The whole `AgentRuntime`**, held for the duration of a single `run()` / `enqueue()` / accessor call. Serial agent execution per session. |
| `std::sync::Arc<tokio::sync::RwLock<HashMap<String, SessionState>>>` | `http/mod.rs::AppState.sessions` | HTTP's session map — many readers (`GET /sessions/:id`, `fork`) vs. rare writers (`POST /sessions`, reaper). |
| `std::sync::RwLock<HashMap<String, broadcast::Sender<SseEvent>>>` | `http/mod.rs::AppState.event_channels` | SSE fan-out channels per session. Readers spawn sender halves; writers create on session-start, drop on session-end. |
| `tokio::sync::Semaphore` | `http/mod.rs::AppState.run_semaphore` | Caps concurrent agent runs across the whole server. Acquired in `run_agent` / `send_session_message` before constructing `AgentRuntime`. `MAX_PERMITS` when `max_concurrent_runs = 0` (unlimited). |
| `Arc<RwLock<Vec<TodoItem>>>` | `runtime.rs::AgentRuntime.todo_list` | The agent's task list. Held by `TodoWriteTool` and read back by `AgentRuntime::current_todos`. Shared via `Arc` so the tool can mutate without going through the runtime mutex. |
| `Arc<RwLock<Option<GoalState>>>` | `runtime.rs::AgentRuntime.goal_state` | Active goal. `set_goal`/`clear_goal` take write; `current_goal`/`run_goal_loop` take read. Shared via `Arc` so HTTP's `force_clear_goal_when_runtime_busy` can clear without the runtime mutex. |
| `Arc<PlanApprovalGate>` / `Arc<PlanModeRequestGate>` | `runtime.rs::AgentRuntime.plan_approval_gate` / `plan_mode_request_gate` | Plan-mode 2.0 gates. Internal `Notify`-based primitives; the `Arc` lets HTTP and TUI clone a handle and call `approve`/`reject` without holding the runtime mutex. |
| `Arc<Mutex<CheckpointLogWriter>>` | `runtime.rs::CheckpointState.writer` | Append-only `checkpoints.jsonl` writes. Mutex because writes are short and serial; not async because the file I/O is blocking. |
| `Arc<Mutex<TouchedFiles>>` | `runtime.rs::CheckpointState.touched_files` | Per-turn file-change tracking shared with `checkpoint_save` tool. |
| `Arc<AtomicUsize>` | `runtime.rs::CheckpointState.turn_index` | 0-indexed turn counter. Shared with `checkpoint_save` so the tool can stamp checkpoint entries without a lock. |
| `Arc<AtomicBool>` | `runtime.rs` / kernel | Goal-165 plan-mode exploration flag. Set by `EnterPlanModeTool`, read by every tool's `is_readonly_for_call` permission check. |

### Why the mix

The `tokio::sync::Mutex<AgentRuntime>` is the heavy hammer — holding it
serialises *everything* on a session, including reads. The `Arc<RwLock<...>>`
fields on `AgentRuntime` exist precisely so that **callers that don't need
to drive the kernel** (current goal, plan gate approve, todo list) can
operate without taking the runtime mutex. This is what
`http::handlers::force_clear_goal_when_runtime_busy` relies on.

If you're adding a new piece of session state, ask: **does the new state
need to be readable while a turn is in flight?** If yes → wrap it in its
own `Arc<RwLock<>>` and expose accessor methods on `AgentRuntime`. If no
→ leave it as a plain field mutated only via `&mut self` methods.

---

## 3. Lock acquisition order

When a code path takes multiple locks, do it in this order to avoid
deadlock:

1. **HTTP layer**: `AppState.run_semaphore` (acquired in handler) →
   `AppState.sessions.write()` (when creating/destroying session) →
   `AppState.event_channels.write()` (when churning channels) →
   per-session `tokio::Mutex<AgentRuntime>` (when driving a turn).
2. **Runtime layer**: take the runtime mutex first; once inside, the
   `Arc<RwLock<>>` fields are independent and never nest.
3. **Kernel layer**: `RunCore::run_inner` only reads its own `&mut self`
   state. It does not reach back up into `AgentRuntime`. The forwarder
   task spawned in `execute_kernel_turn` runs in parallel with the
   kernel but only emits events — it does not touch runtime state.

### Known ordering-sensitive paths

- `force_clear_goal_when_runtime_busy` (`http/handlers.rs`): tries to
  acquire the runtime mutex with `try_lock`; on failure, falls back to
  `goal_state.write()` directly. This is *why* `goal_state` lives behind
  its own `Arc<RwLock<>>` rather than as a plain field — so an externally
  driven abort can fire without waiting on the running turn.
- `execute_kernel_turn` (`runtime.rs`): spawns a forwarder task that
  consumes the kernel's step-event channel and drains into the runtime's
  `event_sink`. The forwarder holds a clone of the `Arc<dyn EventSink>`,
  not a borrow on `AgentRuntime`, so the runtime mutex does not need to
  stay held across the spawned task's lifetime.

---

## 4. The four execution modes on `AgentRuntime`

| Method | What it does | When to use |
|---|---|---|
| `run(user_text)` | Append user message, run one kernel turn, emit assistant messages, return `RuntimeOutcome`. | One-shot prompts (CLI default, `POST /run`). |
| `enqueue(text)` | Push onto `message_queue`, drain in FIFO order, return last outcome. | Interactive surfaces where messages may arrive while a turn is in flight (TUI, `POST /sessions/:id/messages`). |
| `run_goal_loop(prompt, condition, max_turns)` | Loop `run` + a judge call until condition met or budget exhausted. | `/goal` command, `POST /sessions/:id/goal`. |
| `run_event_loop(initial, wakeup_slot, bg_manager)` | Loop `run` until no wakeup and no completed background job. | `recursive loop` CLI subcommand. |

All four routes funnel through `execute_kernel_turn` →
`AgentKernel::run(ctx)` → `RunCore::run_inner`. The kernel doesn't know
which mode it's in; the runtime decides how to chain turns.

---

## 5. Per-turn state vs. per-session state

| Owned by | State | Lifetime |
|---|---|---|
| `AgentKernel` | `llm`, `tools`, `compactor`, `hooks`, `storage`, `session_store`, `max_steps`, `max_transcript_chars`, `stuck_window` config | Process / runtime |
| `AgentRuntime` | `transcript`, `event_sink`, `streaming`, `compactor`, `message_queue`, `deferred_turn_finished`, `goal_eval_transcript_tail` | One session |
| `AgentRuntime::checkpoints: CheckpointState` | `session_id`, `turn_index`, `shadow`, `writer`, `touched_files`, `log_path` | One session, only after `enable_checkpoints` |
| `AgentRuntime` Arc-shared | `todo_list`, `plan_approval_gate`, `plan_mode_request_gate`, `goal_state` | One session, but `Arc`-cloned out so tools / handlers can mutate without `&mut self` |
| `RunCore` (per turn) | `messages` (cloned from `transcript`), per-turn counters | One turn — built by `AgentKernel::run`, consumed by `run_inner` |

If you add state that *should reset every turn*, it goes on `RunCore` or
in `TurnContext`. If it *should persist across turns but reset per
session*, it goes on `AgentRuntime`. If it should *persist across
sessions* (rare — basically only config + storage), it goes on
`AgentKernel`.

---

## 6. Adding a new interactive surface

Suppose you're adding an MCP server or websocket endpoint. The shape is:

1. Construct an `AgentRuntime` via `AgentRuntimeBuilder` in the
   connection-setup handler.
2. Wrap it in `Arc<tokio::sync::Mutex<AgentRuntime>>`.
3. Clone any `Arc<dyn EventSink>`-like handles you need for streaming
   events out-of-band.
4. Call `runtime.lock().await.enqueue(text).await` per request. Hold
   the lock only across the call.
5. For interactive controls (approve plan, abort, current goal), use
   the `Arc`-shared accessors (`plan_approval_gate()`,
   `current_goal()`, `clear_goal()`) so you don't block on a running
   turn.
6. Make sure your event sink correctly forwards
   `AgentEvent::MessageAppended`, `TurnFinished`, `CompactionBoundary`,
   and `ToolResult` — these are the SDK protocol events. See
   `http/sse.rs` for the canonical mapping.

---

## 7. Where to look in source

| File | What's there |
|---|---|
| `src/runtime.rs` | `AgentRuntime`, `AgentRuntimeBuilder`, `RuntimeOutcome`, `CheckpointState`, `run` / `enqueue` / `run_goal_loop` / `run_event_loop` |
| `src/kernel.rs` | `AgentKernel`, `AgentKernelBuilder`, `TurnContext`, `TurnOutcome`, `AgentKernel::run` |
| `src/run_core.rs` | `RunCore`, `run_inner`, the seven phase helpers (P1-1) |
| `src/http/handlers.rs` | `run_agent`, `create_session`, `agui_run`, `force_clear_goal_when_runtime_busy` |
| `src/http/mod.rs` | `AppState`, `SseEvent`, run semaphore |
| `crates/recursive-tui/src/backend.rs` | TUI worker task, runtime lifetime in TUI |
| `crates/recursive-tui/src/runtime_builder.rs` | `build_runtime()` factory used by TUI |

## Related documents

- [`docs/architecture/agent-loop.md`](architecture/agent-loop.md) — the
  ReAct loop conceptually, finish reasons, stuck detection.
- [`docs/architecture/invariants.md`](architecture/invariants.md) —
  the eight inviolable rules.
- [`docs/architecture/sessions.md`](architecture/sessions.md) — JSONL
  transcript format, session lifecycle.
- `.dev/AGENTS.md` — the source-code invariant list this document
  paraphrases.
