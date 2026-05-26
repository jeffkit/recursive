# Goal 90 — `recursive loop` CLI Subcommand

**Roadmap**: Phase 10.4 — Loop Mode (part 4/4)

**Design principle check**:
- Implemented as: new Clap subcommand in `src/main.rs`. Minimal wiring.
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

Users need a CLI entry point for loop mode. `recursive loop "monitor X"`
starts the agent in loop mode — it runs, sleeps, re-runs, until it decides
to stop (by not calling `schedule_wakeup`).

## Scope (do exactly this, no more)

### 1. `src/main.rs` — add `Loop` subcommand

```rust
/// Run the agent in loop mode: agent self-schedules wakeups until it stops.
Loop {
    /// Initial goal to start the loop with.
    #[arg(trailing_var_arg = true, required = true)]
    goal: Vec<String>,
},
```

### 2. Handler

```rust
Cmd::Loop { goal } => {
    let wakeup_slot = Arc::new(std::sync::Mutex::new(None));
    // Register schedule_wakeup tool with the slot
    // Build agent with the extra tool
    // Use AgentRunner::run_loop() or run_event_loop()
    // Print outcomes
}
```

Key implementation detail: the `ScheduleWakeup` tool needs to be
registered in the tool registry with the shared WakeupSlot. Then
pass the same slot to `AgentRunner::run_loop()`.

### 3. Tests

- Test: CLI parses `loop` subcommand
- Test: `loop --help` shows usage

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `recursive loop --help` works
- The loop subcommand wires schedule_wakeup tool + AgentRunner

## Notes for the agent

- Read `src/main.rs` for existing Clap subcommand structure (Run, Repl, Serve).
- Read `src/runner.rs` for `AgentRunner::run_loop`.
- Read `src/tools/schedule_wakeup.rs` for `ScheduleWakeup::new(slot)` and `WakeupSlot`.
- Read `src/tools/mod.rs` for `ToolRegistry` and how tools are registered
  (e.g., `registry.register(Arc::new(tool))`).
- The handler is ~30-40 LOC. Mostly boilerplate connecting existing pieces.
- Reuse `build_agent()` if possible, or construct a minimal agent inline.
- **DO NOT modify any tool file. Only modify `src/main.rs`.**
