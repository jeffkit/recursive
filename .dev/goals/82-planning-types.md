# Goal 82 — Planning Mode: Types + StepEvent

**Roadmap**: Phase 8.4 part 1/3 — Planning mode types

**Design principle check**:
- Implemented as: new types added to `src/agent.rs`. Does NOT change the
  run loop logic. Only adds enum variants and a builder method.

## Why

Multi-turn Planning (g78) failed because it tried to modify the complex
agent loop in one shot. This goal adds ONLY the type definitions and
builder method — no loop changes. The loop change comes in goal 83.

## Scope (do exactly this, no more)

### 1. `src/agent.rs` — add PlanningMode enum

Add BEFORE the Agent struct definition:

```rust
/// Controls whether the agent executes tools immediately or presents a plan first.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum PlanningMode {
    /// Execute tool calls immediately (current behavior).
    #[default]
    Immediate,
    /// Buffer tool calls and emit a plan for confirmation before executing.
    PlanFirst,
}
```

### 2. `src/agent.rs` — add StepEvent variants

Add to the existing `StepEvent` enum:

```rust
    /// Agent has produced a plan and is waiting for confirmation.
    PlanProposed {
        /// Human-readable plan description
        plan_text: String,
        /// The buffered tool calls
        tool_calls: Vec<ToolCall>,
    },
    /// Plan was confirmed, execution will proceed.
    PlanConfirmed,
    /// Plan was rejected with a reason.
    PlanRejected { reason: String },
```

### 3. `src/agent.rs` — add field to Agent struct + builder method

Add to the Agent struct:
```rust
    planning_mode: PlanningMode,
    plan_buffer: Option<Vec<ToolCall>>,
```

Add to AgentBuilder:
```rust
    planning_mode: PlanningMode,
```

Add builder method:
```rust
    pub fn planning_mode(mut self, mode: PlanningMode) -> Self {
        self.planning_mode = mode;
        self
    }
```

Wire it in `build()`:
```rust
    planning_mode: self.planning_mode,
    plan_buffer: None,
```

### 4. `src/agent.rs` — add plan control methods (no-op for now)

```rust
impl Agent {
    /// Confirm a proposed plan, allowing execution to proceed.
    pub fn confirm_plan(&mut self) {
        self.plan_buffer = None;  // Will be used in g83
    }

    /// Reject a proposed plan with a reason.
    pub fn reject_plan(&mut self, _reason: &str) {
        self.plan_buffer = None;  // Will be used in g83
    }
}
```

### 5. `src/lib.rs` — export new types

```rust
pub use agent::PlanningMode;
```

### 6. Tests

- Test: PlanningMode default is Immediate
- Test: AgentBuilder with planning_mode(PlanFirst) builds successfully
- Test: Agent with Immediate mode runs exactly as before (regression)
- Test: StepEvent::PlanProposed can be constructed
- Test: confirm_plan/reject_plan don't panic

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets -- -D warnings` clean
- **NO changes to the run() loop logic** — the loop still ignores PlanFirst
- All existing tests pass unchanged

## Notes for the agent

- Read `src/agent.rs` for the Agent struct, StepEvent enum, and
  AgentBuilder.
- The KEY RULE: do NOT modify the `run()` method or the tool execution
  loop. Only add types, fields, and no-op methods.
- The `plan_buffer` field is Option<Vec<ToolCall>> — None means no plan
  pending, Some means a plan was proposed.
- PlanningMode goes in the same section as FinishReason/AgentOutcome.
- Make sure the Default for PlanningMode is Immediate (backward compat).
