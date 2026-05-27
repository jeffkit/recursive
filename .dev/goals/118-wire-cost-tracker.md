# Goal 118 — Wire CostTracker into main.rs run path

**Roadmap**: Phase 15.3 — Cost Tracking (part 2/2, wiring)

**Design principle check**:
- Implemented as: instantiate CostTracker in main.rs, call finish() after agent.run()
- ❌ Does NOT modify agent.rs or cost.rs

## Why

Goal 116 created `src/cost.rs` with `CostTracker` but it's not used
anywhere yet. This goal connects it to the `run` command so every
agent execution persists `cost.json` to the session directory.

## Scope (do exactly this, no more)

### 1. Import and instantiate CostTracker in main.rs

In the `run` command handler, after creating the session_writer:

```rust
use recursive::cost::CostTracker;

let cost_tracker = if !args.no_session {
    if let Some(ref w) = session_writer {
        let session_dir = w.lock().unwrap().session_dir().to_path_buf();
        Some(CostTracker::new(
            session_dir,
            &config.model,
            config.provider_type.as_deref().unwrap_or("openai"),
            &external_pricing,
        ))
    } else {
        None
    }
} else {
    None
};
```

### 2. Call finish() after agent.run() returns

```rust
if let Some(tracker) = cost_tracker {
    let _ = tracker.write_cost_json();  // fire-and-forget
    eprintln!("cost: ${:.4} ({})", 
        tracker.cost_usd().unwrap_or(0.0), config.model);
}
```

### 3. Tests

- **Test A**: Running with session produces cost.json file
- **Test B**: Running with --no-session does NOT produce cost.json

## Acceptance

- `cargo build` green.
- `cargo test` green.
- Only `src/main.rs` is modified.
- `recursive run "hello"` produces a `cost.json` alongside the session.

## Notes for the agent

- `CostTracker` is in `src/cost.rs`, exported via `recursive::cost::CostTracker`.
- The `session_writer` variable already exists in the run path.
- `external_pricing` is already computed before building the agent.
- The `config.model` and provider type are available from `Config`.
- Do NOT modify cost.rs or agent.rs.
- This is a ~15 line change in main.rs.
