# Goal 78 — Multi-turn Planning (plan → confirm → execute)

**Roadmap**: Phase 8.4 — Multi-turn Planning

**Design principle check**:
- Implemented as: new `PlanMode` in `src/agent.rs`. This modifies the
  agent loop but in a well-defined way: a new state that gates execution.
- Preserves the existing loop — adds a phase before tool execution.

## Why

Currently the agent immediately executes every tool call. For complex
tasks, it's better to first output a plan ("I will: 1. read X, 2. modify Y,
3. run tests"), get confirmation, then execute. This gives the user
visibility and control over multi-step operations.

## Scope (do exactly this, no more)

### 1. `src/agent.rs` — add planning mode

Add a configuration option to the Agent:

```rust
pub enum PlanningMode {
    /// Execute immediately (current behavior, default)
    Immediate,
    /// Output plan first, wait for confirmation, then execute
    PlanFirst,
}
```

Add to AgentBuilder:
```rust
pub fn planning_mode(mut self, mode: PlanningMode) -> Self { ... }
```

### 2. Plan-first execution flow

When `PlanningMode::PlanFirst` is active:

1. **Plan phase**: Agent runs normally but when it wants to call tools,
   instead of executing them, it outputs a plan message:
   ```
   [PLAN] I will execute the following steps:
   1. read_file("src/main.rs") — understand current structure
   2. apply_patch(...) — add the new feature
   3. run_shell("cargo test") — verify changes
   
   Confirm? (The agent will wait for user input)
   ```

2. **Confirmation**: Emit a `StepEvent::PlanAwaitingConfirmation { plan: String }`
   event. The caller (CLI/library user) can then:
   - Confirm → agent proceeds to execute the plan
   - Reject → agent receives rejection message and replans
   - Modify → agent receives modification instructions

3. **Execute phase**: After confirmation, execute the planned tool calls
   in order.

### 3. Implementation approach

The simplest approach that preserves the existing loop:

- Add a `plan_buffer: Vec<ToolCall>` to Agent state
- In plan mode, when LLM returns tool_calls:
  - First iteration: buffer them, format as plan text, emit event, return
  - After confirmation: execute buffered calls, continue normally
- A new method `Agent::confirm_plan(&mut self)` and
  `Agent::reject_plan(&mut self, reason: &str)` control the flow

### 4. CLI integration

In `src/main.rs`, when `--plan-first` flag is set:
- On `StepEvent::PlanAwaitingConfirmation`: print the plan, read stdin
  for "y/n/modify"
- On "y": call `agent.confirm_plan()`
- On "n" or other: call `agent.reject_plan("user rejected")`

### 5. StepEvent addition

```rust
pub enum StepEvent {
    // ... existing variants ...
    /// Agent has produced a plan and is waiting for confirmation.
    PlanAwaitingConfirmation {
        plan: String,
        tool_calls: Vec<ToolCall>,
    },
    /// Plan was confirmed, execution proceeding.
    PlanConfirmed,
    /// Plan was rejected.
    PlanRejected { reason: String },
}
```

### 6. Tests

- Test: `PlanningMode::Immediate` works exactly as before (regression)
- Test: `PlanningMode::PlanFirst` buffers tool calls and emits event
- Test: confirming plan executes the buffered calls
- Test: rejecting plan sends rejection to LLM for replanning
- Test: plan format includes tool names and summaries
- Test: multi-step plan (3+ tools) formats correctly

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- Plan mode works: agent outputs plan, waits, then executes
- Immediate mode unchanged (regression safe)
- `--plan-first` CLI flag works

## Notes for the agent

- Read `src/agent.rs` for the main loop. Find where `tool_calls` are
  processed after LLM returns them.
- The plan buffer approach is simplest: intercept tool_calls before
  execution, store them, and return a special message to the caller.
- For the CLI stdin confirmation: use `std::io::stdin().read_line()`.
  This blocks — that's fine for CLI mode.
- The `StepEvent::PlanAwaitingConfirmation` is how library users (not
  CLI) get notified about plans. They can then call confirm/reject.
- Don't make planning mode affect the LLM prompt — the LLM doesn't
  know about plan mode. It just outputs tool calls normally; WE
  intercept and present them as a plan.
- Keep `PlanningMode::Immediate` as the default so nothing breaks.
