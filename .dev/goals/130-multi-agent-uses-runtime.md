# Goal 130 — Multi-Agent pool uses AgentRuntime

**Roadmap**: Kernel Architecture Refactor — Phase 4b (interface adaptation)

**Design principle check**:
- AgentPool.run_with_role() uses AgentRuntime instead of Agent
- Each role invocation gets its own ephemeral AgentRuntime
- SharedMemory injection moves to runtime's system prompt
- Pipeline and TeamOrchestrator continue to work

## Why

Multi-Agent (`src/multi.rs`) currently builds a fresh `Agent` per role invocation.
Since each role runs a single turn (or a short conversation), `AgentRuntime` is
the right abstraction — it manages the transcript and can potentially support
multi-turn role interactions in the future.

## Scope (do exactly this, no more)

### 1. Modify `AgentPool::run_with_role()`

Replace:
```rust
let mut agent = Agent::builder()
    .llm(self.provider.clone())
    .system_prompt(system_prompt)
    .max_steps(role.max_steps)
    .build()?;
agent.run(goal).await
```

With:
```rust
let mut runtime = AgentRuntime::builder()
    .llm(self.provider.clone())
    .system_prompt(system_prompt)
    .max_steps(role.max_steps)
    .build()?;
let outcome = runtime.run(goal).await?;
// Convert RuntimeOutcome to AgentOutcome for backward compat
```

### 2. Return type consideration

`run_with_role()` currently returns `Result<AgentOutcome>`. Options:
- Keep returning AgentOutcome by converting from RuntimeOutcome
- Or change to return RuntimeOutcome (breaking change for Pipeline)

**Preferred**: Keep returning AgentOutcome for now. Create a conversion:
```rust
impl From<RuntimeOutcome> for AgentOutcome {
    fn from(rt: RuntimeOutcome) -> Self { ... }
}
```
Or build AgentOutcome manually from RuntimeOutcome fields.

### 3. Update imports

Replace `use crate::{Agent, AgentOutcome, ...}` with runtime imports.
Keep AgentOutcome for the return type.

### 4. Tests

Existing multi-agent tests should pass unchanged.

## Acceptance

- `cargo test` green (527+ tests)
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `AgentPool::run_with_role()` uses AgentRuntime internally
- Pipeline and TeamOrchestrator still work
- All existing multi.rs tests pass

## Notes for the agent

- Read `src/multi.rs` — focus on `run_with_role()` (lines 267-293).
- Read `src/runtime.rs` for `AgentRuntimeBuilder` API.
- The memory injection (`memory_ctx`) is currently appended to `system_prompt`. Keep this pattern — it works with AgentRuntime's system prompt.
- `AgentOutcome` has fields: final_message, transcript, steps, finish, total_usage, total_llm_latency_ms.
- `RuntimeOutcome` (from runtime.rs) may have different field names — check and map accordingly.
- **Only modify `src/multi.rs`**. DO NOT touch agent.rs, main.rs, runtime.rs, kernel.rs.
- If you need to add a `From` impl, put it in multi.rs (not in runtime.rs).
