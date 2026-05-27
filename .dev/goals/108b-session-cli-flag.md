# Goal 108b — Wire SessionWriter into CLI via on_message callback

**Roadmap**: Phase 14.1 — Session Persistence (part 2b/4)

**Design principle check**:
- Implemented as: new `--session` CLI flag in `src/main.rs`
- Connects SessionWriter (Goal 107) to the on_message callback (Goal 108a)
- Does NOT modify agent.rs (already done in 108a)

**Depends on**: Goal 108a (on_message callback exists)

## Why

This connects the plumbing: SessionWriter (already built in Goal 107)
gets wired to Agent's on_message callback (built in 108a) through the
CLI's `--session` flag.

## Scope (do exactly this, no more)

### 1. Add `--session` flag to the `Run` CLI command

In `src/main.rs`:

```rust
/// Enable JSONL session persistence
#[arg(long, env = "RECURSIVE_SESSION")]
session: bool,
```

### 2. Create SessionWriter when flag is set

In the `run` command handler, before building the agent:

```rust
let session_writer = if args.session {
    let writer = SessionWriter::create(
        &config.workspace,
        &goal,
        &config.model,
        "openai",  // or config.provider_type
    )?;
    Some(Arc::new(std::sync::Mutex::new(writer)))
} else {
    None
};
```

### 3. Pass as on_message callback

```rust
let on_msg: Option<OnMessageFn> = session_writer.clone().map(|w| {
    Box::new(move |msg: &Message| {
        if let Ok(mut writer) = w.lock() {
            let _ = writer.append(msg);  // fire-and-forget
        }
    }) as OnMessageFn
});

let agent = Agent::builder()
    // ... existing config ...
    .on_message(on_msg)  // None if session disabled
    .build()?;
```

Wait — `on_message` takes `OnMessageFn` not `Option<OnMessageFn>`.
Adjust: either make the builder accept Option, or only call `.on_message()`
when Some:

```rust
let mut builder = Agent::builder()
    .llm(...)
    .tools(...)
    // ...;

if let Some(cb) = on_msg {
    builder = builder.on_message(cb);
}

let agent = builder.build()?;
```

### 4. Finalize session on exit

After `agent.run()` returns:

```rust
if let Some(w) = session_writer {
    let status = match &outcome.finish {
        FinishReason::NoMoreToolCalls => "completed",
        FinishReason::BudgetExceeded => "budget_exceeded",
        _ => "stopped",
    };
    let _ = w.lock().unwrap().finish(status);
}
```

### 5. Tests

- **Test A**: Running without `--session` produces no session files
- **Test B**: Integration: a mock agent run with `--session` creates JSONL

## Acceptance

- `cargo build` green.
- `cargo test` green.
- `cargo clippy --all-targets -- -D warnings` green.
- Only `src/main.rs` is modified.
- Running `cargo run -- run "hello" --session` creates files in
  `<workspace>/.recursive/sessions/`.

## Notes for the agent

- Do NOT touch `agent.rs` — that was done in 108a.
- Do NOT touch `http.rs` or `runner.rs` — that's a future goal.
- The `SessionWriter::create` API exists in `src/session.rs` (Goal 107).
- Import `OnMessageFn` from the agent module.
- Use `Arc<Mutex<SessionWriter>>` for the shared writer.
- Errors in session writing should be silently ignored (log if tracing
  is available, but never abort the run).
- This is a ~40 line change in main.rs. Keep it tight.
