# Goal 129 — SubAgent uses AgentKernel directly

**Roadmap**: Kernel Architecture Refactor — Phase 4a (interface adaptation)

**Design principle check**:
- SubAgent creates AgentKernel instead of Agent for sub-task execution
- Sub-agents are single-turn → perfect fit for kernel (no multi-turn state)
- Parent agent still works via Agent::run() (no change there)

## Why

SubAgent currently builds a fresh `Agent` for each invocation. Since sub-agents
are single-turn (one goal → one result), they should use `AgentKernel::run()`
directly — it's simpler, faster (no wrapper overhead), and validates the kernel
API for real use.

## Scope (do exactly this, no more)

### 1. Modify `src/tools/sub_agent.rs`

Replace `Agent::builder()...build()...run(goal)` with:
```rust
use crate::kernel::{AgentKernel, TurnContext};
use crate::event::NullSink;

// In execute():
let kernel = AgentKernel::builder()
    .llm(self.provider.clone())
    .tools(sub_registry)
    .max_steps(sub_max_steps)
    .build()?;

let ctx = TurnContext {
    messages: vec![
        Message::system(system_prompt),
        Message::user(goal),
    ],
    event_sink: Box::new(NullSink),
    tool_specs: kernel.tools().specs(),
    streaming: false,
    permission_hook: self.permission_hook.clone(),
    planning_mode: PlanningMode::default(),
};

let outcome = kernel.run(ctx).await?;
// Use outcome.final_text as the result
```

### 2. Remove Agent import from sub_agent.rs

Remove `use crate::agent::{Agent, ...}` — replace with kernel imports.
Keep `FinishReason` import if needed for error handling.

### 3. Tests

Existing sub_agent tests (in agent.rs test module) should still pass.
The behavior is identical — just the internal implementation changed.

## Acceptance

- `cargo test` green (527+ tests)
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- SubAgent no longer imports or creates `Agent`
- SubAgent uses `AgentKernel::run()` directly
- All existing sub-agent tests pass unchanged

## Notes for the agent

- Read `src/tools/sub_agent.rs` completely — it's only ~120 lines.
- Read `src/kernel.rs` for `AgentKernel::builder()` and `TurnContext`.
- The sub-agent currently builds a system prompt from `default_system_prompt()` or a custom one. Keep that logic.
- The sub-agent uses `FinishReason::Stuck` and `FinishReason::BudgetExceeded` for error detection. After switching to kernel, check `outcome.finish_reason` instead of the AgentOutcome equivalent.
- The current code checks depth limit (`current_depth >= max_depth`). Keep that.
- `TurnOutcome.final_text` maps to what was `AgentOutcome.final_message`.
- **Only modify `src/tools/sub_agent.rs`**. DO NOT touch agent.rs, main.rs, kernel.rs, etc.
