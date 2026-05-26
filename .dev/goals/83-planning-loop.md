# Goal 83 — Planning Mode: Loop Integration

**Roadmap**: Phase 8.4 part 2/3 — Plan-first loop logic

**Design principle check**:
- Implemented as: modification to the tool execution section in
  `src/agent.rs::Agent::run()`. Minimal, targeted change.

## Why

This adds the actual plan-first behavior: when PlanningMode::PlanFirst
is active, tool calls are buffered and a PlanProposed event is emitted
instead of executing immediately.

## Scope (do exactly this, no more)

### 1. Find the tool execution point in `run()`

In `src/agent.rs`, find where tool_calls from the LLM are about to be
executed. It should look something like:

```rust
// After getting completion with tool_calls:
for tc in &tool_calls {
    // execute tool...
}
```

### 2. Add planning mode intercept BEFORE execution

```rust
// Right before tool execution:
if self.planning_mode == PlanningMode::PlanFirst && self.plan_buffer.is_none() {
    // First time seeing tool calls in plan mode — buffer them
    let plan_text = format_plan(&tool_calls);
    self.plan_buffer = Some(tool_calls.clone());

    if let Some(ref tx) = self.events_tx {
        let _ = tx.send(StepEvent::PlanProposed {
            plan_text: plan_text.clone(),
            tool_calls: tool_calls.clone(),
        });
    }

    // Return a special outcome that signals "waiting for confirmation"
    return Ok(AgentOutcome {
        finish: FinishReason::PlanPending,
        steps: self.steps,
        transcript: self.transcript.clone(),
    });
}
```

### 3. Add FinishReason::PlanPending variant

```rust
pub enum FinishReason {
    // ... existing variants ...
    /// Agent proposed a plan and is waiting for confirmation.
    PlanPending,
}
```

### 4. Add `resume_after_confirm()` method

After the user calls `confirm_plan()`, they need a way to resume:

```rust
impl Agent {
    /// Resume execution after a plan was confirmed.
    /// Executes the buffered tool calls and continues the loop.
    pub async fn resume(&mut self) -> Result<AgentOutcome> {
        if let Some(tool_calls) = self.plan_buffer.take() {
            // Execute the buffered tool calls
            // ... (same code as normal tool execution)
            // Then continue the normal run loop
        }
        // Continue with the rest of the agent loop
        self.continue_run().await
    }
}
```

Actually simpler approach: just set a flag and re-enter `run()`:

```rust
pub fn confirm_plan(&mut self) {
    // plan_buffer stays — next run() iteration will see it and execute
    self.plan_confirmed = true;
}

// In run(), the check becomes:
if self.planning_mode == PlanningMode::PlanFirst
    && !self.plan_confirmed
    && self.plan_buffer.is_none()
{
    // Buffer and return PlanPending
}
// If plan_confirmed, fall through to normal execution
```

### 5. `format_plan` helper function

```rust
fn format_plan(tool_calls: &[ToolCall]) -> String {
    let mut plan = String::from("Plan:\n");
    for (i, tc) in tool_calls.iter().enumerate() {
        plan.push_str(&format!("  {}. {} ({})\n", i + 1, tc.name,
            serde_json::to_string(&tc.arguments).unwrap_or_default()));
    }
    plan
}
```

### 6. Tests

- Test: PlanFirst mode buffers tool calls and returns PlanPending
- Test: After confirm, re-running executes the buffered calls
- Test: After reject, re-running sends rejection to LLM
- Test: Immediate mode unchanged (regression)
- Test: format_plan produces readable output

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets -- -D warnings` clean
- Agent with PlanFirst buffers and pauses at first tool call
- Agent with Immediate works exactly as before
- plan_confirmed flow resumes execution

## Notes for the agent

- Read `src/agent.rs` carefully. Find the EXACT point where tool_calls
  are processed. It's after `let completion = self.llm.complete(...)`.
- The change is SMALL: an `if` block that checks planning_mode BEFORE
  the tool execution loop. Everything else stays the same.
- Don't refactor the entire run() method. Just add the intercept.
- The `plan_confirmed` flag approach is simpler than `resume()` —
  the user calls confirm_plan() then calls run() again.
- Use MockProvider for tests — script it to return tool_calls on first
  completion, then "stop" on second.
