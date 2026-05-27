# Goal 108 — Wire JSONL session into Agent run loop

**Roadmap**: Phase 14.1 — Session Persistence (part 2/4)

**Design principle check**:
- Implemented as: `SessionWriter` integration in `src/main.rs` and `src/runner.rs`
- Minimal touch on `agent.rs` — only to emit messages through a callback
- The Agent itself remains unaware of persistence

## Why

Goal 107 built the SessionWriter; now we connect it to the actual agent
execution so every run produces a live JSONL transcript.

## Scope (do exactly this, no more)

### 1. Add `--session` flag to CLI

In `src/main.rs`, the `Run` command gains:
```rust
/// Enable session persistence (writes to ~/.recursive/sessions/)
#[arg(long, default_value_t = false)]
session: bool,
```

When `--session` is set (or env `RECURSIVE_SESSION=1`), create a
`SessionWriter` before calling `agent.run()`.

### 2. Message callback on Agent

Add an optional callback to `Agent` that fires on every message
appended to the transcript:

```rust
pub type MessageCallback = Box<dyn Fn(&Message) + Send + Sync>;

// In AgentBuilder:
pub fn on_message(mut self, cb: MessageCallback) -> Self;
```

The callback fires:
- After system prompt is added
- After user goal is added
- After each assistant response
- After each tool result

### 3. Wire in main.rs

```rust
let writer = if session_enabled {
    Some(Arc::new(Mutex::new(SessionWriter::create(...)?)))
} else {
    None
};

let on_msg = {
    let writer = writer.clone();
    move |msg: &Message| {
        if let Some(w) = &writer {
            let _ = w.lock().unwrap().append(msg);
        }
    }
};

let agent = Agent::builder()
    .on_message(Box::new(on_msg))
    // ... rest
    .build()?;
```

### 4. Finalize on exit

After `agent.run()` returns, call `writer.finish(status, steps)`.

### 5. Wire into HTTP server

In `src/http.rs`, when a session is created via `POST /sessions`,
attach a `SessionWriter`. On each message exchange, append. This makes
HTTP sessions persistent across server restarts.

### 6. Resume from JSONL

Update the `Resume` CLI command to detect JSONL sessions:
```rust
// If path ends with .jsonl, use SessionReader
// Otherwise fall back to legacy SessionFile::read_from
```

### 7. Tests

- **Test A**: Integration test — run agent with `--session`, verify .jsonl created
- **Test B**: Resume from JSONL restores transcript correctly
- **Test C**: HTTP session persists after simulated server restart
- **Test D**: Multiple sessions for same workspace don't interfere

## Acceptance

- `cargo build` green.
- `cargo test` green (4+ new tests).
- `cargo clippy --all-targets -- -D warnings` green.
- Running `recursive run "hello" --session` produces a .jsonl file in
  `~/.recursive/sessions/<workspace-slug>/`.
- `recursive sessions list` shows it.

## Notes for the agent

- The `on_message` callback must be `Send + Sync` because agent runs
  are async. Use `Arc<Mutex<SessionWriter>>`.
- Don't make `--session` the default yet. We'll flip the default in a
  later goal after validation.
- The HTTP wiring is the most complex part — `SessionState` in http.rs
  needs to hold an `Option<SessionWriter>`.
- Keep changes to `agent.rs` minimal. The callback is fire-and-forget;
  errors in writing should be logged, not propagated.
