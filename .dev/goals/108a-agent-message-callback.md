# Goal 108a — Add message callback to Agent (minimal agent.rs change)

**Roadmap**: Phase 14.1 — Session Persistence (part 2a/4)

**Design principle check**:
- Implemented as: one optional field + callback invocation in `src/agent.rs`
- Minimal: 4 lines in Agent struct, 4 lines in AgentBuilder, ~8 lines of callback calls
- Does NOT change the run loop logic, only emits events

## Why

Goal 108 failed 3 times because it tried to change too many files at once
(agent.rs + main.rs + runner.rs + http.rs). This goal isolates the smallest
possible change: add an optional `on_message` callback to Agent that fires
whenever a message is appended to the transcript.

## Scope (do exactly this, no more)

### 1. Add callback type and field to Agent

In `src/agent.rs`:

```rust
/// Optional callback fired whenever a message is added to the transcript.
pub type OnMessageFn = Box<dyn Fn(&Message) + Send + Sync>;

// In the Agent struct, add:
on_message: Option<OnMessageFn>,

// In AgentBuilder, add:
on_message: Option<OnMessageFn>,

pub fn on_message(mut self, f: OnMessageFn) -> Self {
    self.on_message = Some(f);
    self
}
```

### 2. Fire callback at transcript append points

In `Agent::run()`, after each `self.transcript.push(msg)` call, add:

```rust
if let Some(cb) = &self.on_message {
    cb(&msg);
}
```

There should be 4-5 push sites:
- System prompt message
- User goal message
- Assistant response message
- Tool result messages

Find them by searching for `self.transcript.push` in agent.rs.

### 3. Tests

- **Test A**: Agent with no callback works as before (None case)
- **Test B**: Agent with callback receives all messages in order
- **Test C**: Callback errors don't abort the agent (it's fire-and-forget)

## Acceptance

- `cargo build` green.
- `cargo test` green.
- `cargo clippy --all-targets -- -D warnings` green.
- Only `src/agent.rs` is modified.

## Notes for the agent

- Do NOT touch `main.rs`, `runner.rs`, `http.rs`, or any other file.
- The callback type must be `Send + Sync` because Agent::run is async.
- If the callback panics, catch it with `std::panic::catch_unwind` or
  simply don't — document that panicking callbacks are UB.
  Simplest: just call it, don't wrap. Panics are caller's problem.
- Search for all `self.transcript.push` sites in agent.rs. There may be
  some in the compaction path too — those should NOT fire the callback
  (compacted messages are synthetic, not new).
- Keep the diff minimal. This should be < 30 lines of real change.
