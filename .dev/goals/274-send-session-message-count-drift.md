# Goal 274 — Update session message_count on turn failure too

**Roadmap**: Phase 17 (Production Hardening) — P0 from
`docs/review/architecture-review-2026-06-15.md` (NEW-CORE-15)

**Design principle check**:
- Implemented as: route `MessageAppended` / `MessageAppendedWithAudit`
  events through the same forwarder task that owns the broadcast
  channel; have the forwarder maintain the `non_system_message_count`
  atomic as events flow past.
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- ❌ Does NOT add a new feature flag

## Why

`src/http/handlers.rs:870-875` (`send_session_message`) only updates
`msg_count_arc` on the *success* path:

```rust
let new_count = runtime.transcript().iter()
    .filter(|m| m.role != Role::System).count();
msg_count_arc.store(new_count, Ordering::Relaxed);
```

But the forwarder task on line 810-834 has *already* fired
`MessageAppended` events for any messages appended before the LLM
call errored out. The message stream and the atomic count are owned
by different tasks — they can drift when an error happens mid-turn.

Diagnostic symptom: `GET /sessions` shows message_count=N, but
`GET /sessions/:id` shows N+2 messages (the user message + first
assistant partial that errored). Users think it's a race or a bug in
the persistence layer.

The fix: stop recomputing count from the transcript in the handler.
Maintain it incrementally in the forwarder, which is the single
owner of the message stream.

## Scope (do exactly this, no more)

### 1. Move count tracking into the forwarder

In `src/http/handlers.rs::send_session_message`, the forwarder
task (around line 810) is the only path that converts AgentEvents
into SseEvents. Have it also maintain the count:

```rust
let forward_handle = tokio::spawn(async move {
    let mut tool_start_times: HashMap<String, Instant> = HashMap::new();
    let mut count: usize = initial_count;
    while let Some(ref agent_event) = event_rx.recv().await {
        // existing ToolCall / ToolResult handling ...
        match agent_event {
            AgentEvent::MessageAppended { message, .. }
            | AgentEvent::MessageAppendedWithAudit { message, .. } => {
                if message.role != Role::System {
                    count += 1;
                    msg_count_arc.store(count, Ordering::Relaxed);
                }
            }
            _ => {}
        }
        if let Some(sse_event) = map_agent_event(agent_event) {
            let _ = broadcast_tx.send(sse_event);
        }
    }
});
```

The `initial_count` should be the `msg_count_arc` value at the
moment the forwarder starts (so resumes don't double-count).
Capture it before the spawn:

```rust
let initial_count = msg_count_arc.load(Ordering::Relaxed);
```

### 2. Delete the post-handler recount

Remove the block at `src/http/handlers.rs:870-875`. The atomic is
now maintained by the forwarder; the handler does not touch it.

Keep `msg_count_arc.store(...)` for the *initial* seed when a
session is created (`src/http/handlers.rs:265`) — that's a
separate one-time set, not a recount.

### 3. Test

Add to `src/http/handlers.rs` `#[cfg(test)]` (or extend
`tests/http.rs` if the forwarder can't be exercised in-process):

```rust
#[tokio::test]
async fn message_count_updates_on_failure_path() {
    // Build a runtime whose LLM errors out mid-turn after producing
    // an assistant message with tool_calls.
    // Run a turn. Assert:
    //   1. The runtime returns Err.
    //   2. msg_count_arc >= 2 (user + assistant).
    //   3. The transcript contains those messages.
}
```

The test must use the real forwarder wiring (event channel →
forwarder → atomic store), not a fake. Per g268's test discipline
note, prefer deterministic serde / direct struct assertions over
runtime dance.

## Acceptance

- `cargo test --workspace` — green
- `cargo clippy --all-targets --all-features -- -D warnings` —
  clean
- `cargo fmt --all` — applied
- `grep "msg_count_arc.store" src/http/handlers.rs` — only the
  initial seed in `create_session` (one match), not the
  post-turn recount
- The new test passes; an existing turn-success test still
  passes (regression)

## Notes for the agent

- The `Arc<AtomicUsize>` lives in `SessionState`; the forwarder
  must `clone()` the Arc to move into the spawned task. Don't
  move the original.
- The forwarder must drain `event_rx` until the channel closes;
  do not break out early on first error event.
- The handler currently does `runtime.set_event_sink(Arc::new(NullSink))`
  after the turn (line 855). That disconnects the sink from the
  runtime, which causes `event_tx.send` to start failing. The
  forwarder should drain whatever it received up to that point
  before the channel closes; that is the existing behavior.
- Estimated diff: 1 file, ~15 lines net (forwarder + new test).
- **Test discipline reminder (from g268 post-mortem)**: avoid
  `runtime.run()` in unit tests if a struct-level test suffices.
  But here we *do* need the forwarder wiring, so use a minimal
  test runtime with a MockProvider that errors after the first
  assistant message.

**Disjoint file guarantee**: This goal touches
`src/http/handlers.rs`. Goal 275 touches `src/runtime.rs`. No
overlap — safe to run in parallel.