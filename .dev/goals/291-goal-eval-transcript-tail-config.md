# Goal 291 — Make GOAL_EVAL_TRANSCRIPT_TAIL configurable via AgentConfig

**Roadmap**: Post-Phase (Arch-review cleanup) — D6 from arch-review 2026-06-16

**Design principle check**:
- Implemented as: add `goal_eval_transcript_tail: usize` to `AgentConfig`
  (default 12, matching current hard-coded value), use it in `evaluate_goal`
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- ❌ Does NOT add a new feature flag (uses existing config mechanism)

## Why

`src/runtime.rs:216` has:
```rust
const GOAL_EVAL_TRANSCRIPT_TAIL: usize = 12;
```

And line 817:
```rust
let tail = self.transcript_tail(GOAL_EVAL_TRANSCRIPT_TAIL);
```

The tail window (12 messages) determines how much transcript the
`GoalEvaluator` sees. For agents running long tasks, 12 messages may
miss important context (the relevant state might be 20+ messages back).
For agents with expensive reasoning tokens, 12 messages might be far
too many for a quick goal-completion check.

There is no way to adjust this without recompiling — it must become
configurable.

## Scope (do exactly this, no more)

### 1. `src/config.rs` — add field to `AgentConfig`

Find `pub struct AgentConfig` (or equivalent config struct for the agent).
Add:

```rust
/// Number of most-recent messages passed to the goal evaluator.
/// Smaller values reduce cost; larger values improve accuracy for long sessions.
/// Default: 12.
#[serde(default = "default_goal_eval_transcript_tail")]
pub goal_eval_transcript_tail: usize,

fn default_goal_eval_transcript_tail() -> usize { 12 }
```

If `AgentConfig` doesn't exist or the config struct is differently named,
find the relevant struct (read `src/config.rs` to confirm the name).

### 2. `src/runtime.rs` — thread config value through `evaluate_goal`

Remove the `const GOAL_EVAL_TRANSCRIPT_TAIL: usize = 12;` line.

In `evaluate_goal()` (or wherever `transcript_tail(GOAL_EVAL_TRANSCRIPT_TAIL)`
is called), replace the constant with `self.config.goal_eval_transcript_tail`
(or however the runtime holds a reference to its config).

Verify by checking: how does `AgentRuntime` currently access `AgentConfig`?
Read `AgentRuntime` struct fields for a `config` field.

### 3. `recursive.toml` / docs — document the new key (optional)

If there's a `recursive.toml` example or `ConfigBuilder` doc-test, add
a commented example line for `goal_eval_transcript_tail = 12`.

### 4. Tests

Add a unit test in `src/runtime.rs` that constructs a runtime with
`goal_eval_transcript_tail = 3`, sends 6 messages, and verifies that
`evaluate_goal` only sees 3 (not 12 or 6). This can be a source-level
check or a mock-based test.

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `GOAL_EVAL_TRANSCRIPT_TAIL` constant no longer exists in `runtime.rs`
- `goal_eval_transcript_tail` field exists in the config struct
- `serde(default)` set to 12 (backward-compatible with existing config files)

## Notes for the agent

- Read `src/config.rs` first to confirm the config struct name.
- Read `src/runtime.rs` around `GOAL_EVAL_TRANSCRIPT_TAIL` (line 216) and
  `evaluate_goal` (line ~817) to understand the data flow.
- Check how `AgentRuntime` holds a reference to `AgentConfig` — it may be
  `self.config`, `self.settings`, or injected via `Builder` pattern.
- The change may be 2-5 lines in runtime.rs + a few lines in config.rs.
- **DO NOT modify** `src/agent.rs`, `src/kernel.rs`, `src/run_core.rs`.
