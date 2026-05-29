# Goal 144 — Wire `agui-tui/checkpoint_post` Custom event from /agui

> **Roadmap**: follow-up to g141 (per-turn shadow checkpoints) +
> g143 (AG-UI integration). The `agui-tui` client already knows how
> to render `agui-tui/checkpoint_post`; the server side just hasn't
> been emitting it yet. This goal closes that loop.
> **Design principle check**: Pure additive change to `src/http.rs`'s
> `/agui` handler. No protocol/library changes. The Custom event was
> already specified in `agui-protocol` at goal 143.

## Why

After g141, every turn produces a workspace snapshot whose id is
exposed on `RuntimeOutcome.checkpoint_id`. After g142, that snapshot
lives under `~/.recursive/workspaces/<hash>/shadow-git/`. After g143,
the recursive HTTP server can drive an AG-UI client.

But the `/agui` handler in g143 builds the `AgentRuntime` via
`AgentRuntimeBuilder` directly and **never calls
`enable_checkpoints`**, so `outcome.checkpoint_id` is always `None`.
Even if it were populated, the handler doesn't surface it to the
client.

Concrete user pain: an `agui-tui` user sees the agent modify files
but has no way to know what checkpoint id corresponds to "the state
before this turn". To rewind they must leave the TUI, run
`recursive sessions list`, count turns, then run
`recursive sessions rewind`. With this goal, the State sidebar shows
the latest checkpoint id live, and a future TUI feature can offer
in-app rewind.

## What

Two narrow changes inside `src/http.rs::agui_run`:

1. Call `runtime.enable_checkpoints(...)` after builder.build(), using:
   - `workspace = state.config.workspace.clone()`
   - `session_id = sanitize(input.thread_id)` — the AG-UI thread is
     the natural session boundary
   - `log_path = ~/.recursive/workspaces/<hash>/sessions/agui-<thread>/checkpoints.jsonl`
   - `touched_slot = runtime.kernel().tools().touched_files()`
   - On error: log a warning and proceed without checkpoints (so
     git-less environments still work).

2. After `runtime.run()` returns, before the converter task emits
   `RunFinished`, emit a Custom event:

   ```json
   {
     "type": "Custom",
     "name": "agui-tui/checkpoint_post",
     "value": {
       "turn": <turn_index>,
       "postId": "<short-sha>"
     }
   }
   ```

   The `turn` value is `runtime.turn_index() - 1` (the just-finished
   turn). The `postId` is `outcome.checkpoint_id.0`. If
   `checkpoint_id` is `None` (snapshot failed or was disabled), skip
   the Custom event entirely.

## Architecture detail: where the event is injected

The `agui_run` handler today has two cooperating tokio tasks:

- **converter**: drains AgentEvent → forwards AG-UI events → emits
  `RunFinished` once the AgentEvent stream closes.
- **driver**: calls `runtime.run()`, then disconnects the sink so the
  converter sees `recv() = None`.

The driver task is the only place that can see `outcome`. So:

1. The driver gets `outcome.checkpoint_id`.
2. It pushes the Custom event onto `sse_tx` *before* dropping its
   handle to the converter.
3. The converter's `RunFinished` is sent after, just like today.

To make this race-free, the driver must coordinate with the converter
to ensure the Custom event arrives between the last AgentEvent and
RunFinished. The cleanest way: have the driver send the Custom event
via the same channel the converter writes to (`sse_tx`), but only
**after** awaiting `converter_handle` … which is what the current
code does already! It awaits the converter handle to drain, but
RunFinished was emitted by the converter on `recv = None`. So we
need to flip the order: the converter no longer emits RunFinished
itself; the driver does.

Refactor:

- Converter task: just translate AgentEvents and exit when channel
  closes. No RunFinished.
- Driver task: after run() returns, await converter, then optionally
  emit Custom checkpoint_post, then emit RunFinished.

This is a small, local refactor. Two events move from the converter
to the driver. The converter shrinks.

## Tests

Add to `tests/agui_e2e.rs`:

- `agui_endpoint_emits_checkpoint_post_before_run_finished`:
  - Spawn the server with a MockProvider that returns one
    completion. Use the workspace defaulted to a tempdir so a
    real shadow-git repo can be created (gate on `has_git()`).
  - POST a RunAgentInput.
  - Collect events, find a Custom with name "agui-tui/checkpoint_post".
  - Assert: it appears, it precedes RunFinished, value.turn = 0,
    value.postId is a 12-char-ish string.
  - If git is unavailable, assert no checkpoint_post fires but the
    run still completes (RunFinished arrives).

- `agui_endpoint_uses_thread_id_as_session_for_checkpoints`:
  - Same setup. Run twice with the same `thread_id`. Confirm both
    runs produce checkpoint_post events with monotonically
    increasing `turn` values (0 then 1).

## Acceptance

- `cargo build --workspace` green.
- `cargo test --workspace` green; ≥ 2 new e2e tests.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo fmt --all -- --check` clean.

## Out of scope

- `permission_request` Custom events — needs an async permission hook
  redesign (see follow-up).
- `heartbeat` and `file_artifact` events — separate goals.
- WebSocket transport.
- Exposing the workspace path in the AG-UI request body (we use the
  server's configured workspace; per-call override is a future
  feature).
