# Goal 249 — Make stuck-detection window and threshold configurable

**Roadmap**: Arch-review bugfixes (medium severity)

**Design principle check**:
- Implemented as: add `stuck_window` and `stuck_error_rate` to Config
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

`STUCK_WINDOW=10` and `STUCK_ERROR_RATE=0.8` are hard-coded constants in
`src/run_core.rs`. For short tasks this may trigger too early; for long tasks
with occasional errors it may never trigger. There is no way to tune the
detection without editing source.

## Scope (do exactly this, no more)

### 1. `src/config.rs` — add two new fields

```rust
/// Number of recent steps to check for stuck detection. Default 10.
pub stuck_window: usize,
/// Fraction of steps that must be errors to declare "stuck". Default 0.8.
pub stuck_error_rate: f64,
```

Load from env vars:
- `RECURSIVE_STUCK_WINDOW` → parse as usize, default 10
- `RECURSIVE_STUCK_ERROR_RATE` → parse as f64, default 0.8

Add both fields with their defaults to all `Config { ... }` struct literals
in `src/` and `tests/` that need them.

### 2. `src/run_core.rs` — read from Config

Find `STUCK_WINDOW` and `STUCK_ERROR_RATE` (or their inline values) in
`src/run_core.rs`. Replace the hard-coded constants with values from the
`Config` passed to `RunCore` (or `GoalEvaluator`, whichever holds the
stuck-detection logic).

Check how `RunCore` currently receives `Config` (likely passed at
construction). Use `config.stuck_window` and `config.stuck_error_rate`
in place of the constants.

### 3. Tests

Add a unit test in `src/run_core.rs` `#[cfg(test)]` that verifies stuck
detection triggers correctly with a custom window/rate (e.g. window=3,
rate=1.0 means 3 consecutive errors triggers stuck).

Also add a test in `src/config.rs` `#[cfg(test)]` that verifies both env
vars parse correctly.

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `STUCK_WINDOW` and `STUCK_ERROR_RATE` constants removed from `src/run_core.rs`
- Values read from `Config` at runtime

## Notes for the agent

- Read `src/run_core.rs` to find where `STUCK_WINDOW`/`STUCK_ERROR_RATE` are
  used and how `Config` is passed in.
- Read `src/config.rs` for existing f64 / usize env-var parse patterns.
- Add `stuck_window: 10, stuck_error_rate: 0.8` to all existing
  `Config { ... }` literals in `src/` and `tests/`.
- **DO NOT modify** `src/agent.rs`, `src/runtime.rs`, `src/llm/`.
- **DO NOT call `exit_plan_mode` or `request_plan_mode`.** You are running
  headless; the plan gate has no reviewer. Just read and edit directly.
