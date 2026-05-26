# Goal 85 — Fix PlanFirst Infinite Loop

**Roadmap**: pre-publication fix (C-2)

**Design principle check**:
- Implemented as: bug fix in `src/agent.rs` planning logic.
- Does NOT add new features — fixes broken existing feature.

## Why

The `PlanFirst` planning mode has a critical bug: after `confirm_plan()`
is called, the subsequent `run()` call re-enters the planning state because
`confirm_plan()` just clears `plan_buffer` to `None` — which is the same
initial state that triggers plan proposal. This creates an infinite loop:
propose → confirm → run → propose → confirm → ...

## The Bug (in detail)

1. `run()` sees `PlanFirst` mode + `plan_buffer.is_none()` → proposes plan
2. Returns `FinishReason::PlanPending`
3. CLI calls `confirm_plan()` → sets `plan_buffer = None`
4. CLI calls `run(goal)` again
5. `plan_buffer.is_none()` is true again → back to step 1 (infinite loop)

The confirmed tool calls are silently discarded. They should be executed.

## Fix

Add a `plan_confirmed: bool` field to `Agent`:

```rust
pub struct Agent {
    // ... existing fields ...
    plan_buffer: Option<Vec<ToolCall>>,
    plan_confirmed: bool,  // NEW
}
```

Change `confirm_plan()`:
```rust
pub fn confirm_plan(&mut self) {
    self.plan_confirmed = true;
    // Do NOT clear plan_buffer — we need the calls for execution
}
```

Change the planning guard in `run()`:
```rust
// If plan was confirmed, execute the buffered calls directly
if self.planning_mode == PlanningMode::PlanFirst && self.plan_confirmed {
    if let Some(calls) = self.plan_buffer.take() {
        self.plan_confirmed = false;
        // Execute calls directly (skip re-proposing)
        for call in calls {
            // ... dispatch to tool registry ...
        }
    }
}

// Original guard: only propose if no buffer AND not confirmed
if self.planning_mode == PlanningMode::PlanFirst
    && self.plan_buffer.is_none()
    && !self.plan_confirmed
{
    // ... buffer tool calls, return PlanPending ...
}
```

## Tests

- Test: `confirm_plan()` followed by `run()` executes the buffered tools
  (not an infinite loop)
- Test: `reject_plan()` + `run()` asks for a revised plan
- Test: Normal non-planning mode is unaffected

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all -- --check` clean
- The infinite loop is gone

## Notes for the agent

- The planning logic is around line 390-420 in `src/agent.rs`.
- `confirm_plan()` is at line ~233.
- The key insight: `plan_buffer: None` means BOTH "no plan seen yet" AND
  "plan was confirmed" — these should be distinct states.
- Don't change `reject_plan()` — it correctly adds error messages to
  transcript and clears buffer, which triggers re-proposal on next run.
- Remember to init `plan_confirmed: false` in the builder.
