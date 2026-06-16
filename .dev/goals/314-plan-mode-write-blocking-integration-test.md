# Goal 314 — Integration test: plan mode write-tool blocking

## Why

The plan mode write-blocking logic lives in `src/run_core.rs` (around
line 268–285) and is covered only by individual unit tests in
`src/tools/plan_mode.rs`. There is **no integration-level test** that
drives a real `AgentRuntime` through:

1. `enter_plan_mode` → agent calls a write tool → gets blocked with
   `"ERROR: Cannot execute '...' in plan mode."`
2. `exit_plan_mode` with a plan string → write tool is accepted again
   (or the run ends)

The `exploring_plan_mode` flag is set via `EnterPlanModeTool` and
checked in `run_core.rs`. This coupling is non-trivial enough to
warrant an integration test to prevent regressions.

## Scope

Add one new integration-style test (can go in `tests/integration.rs`
or a new file `tests/plan_mode_integration.rs`):

```
plan_mode_write_tool_blocked_until_exit
```

### Test outline

1. Build a `MockProvider` that emits a two-turn script:
   - Turn 1: call `enter_plan_mode` (to enter plan mode)
   - Turn 2: call `write_file` (or `Edit`) with some content → the
     result should contain `"Cannot execute"` or `"plan mode"` and the
     file should NOT be created
   - Turn 3 (optional): call `exit_plan_mode` with a plan string, then
     verify the run ends cleanly with `FinishReason::Done` or similar

2. Run `AgentRuntime::run()` with these mock turns.

3. Assert:
   - The write-tool result in Turn 2 contains the expected blocking
     message (`"Cannot execute"` or `"plan mode"`).
   - (Optional) After `exit_plan_mode`, any subsequent write tool
     should be allowed (write succeeds).

### Mock provider

Use `MockProvider` from `tests/integration.rs` or the shared test
helper. The provider needs to be able to return a `ToolUse` call for
`enter_plan_mode`, then a `ToolUse` call for `write_file` / `Edit`.

The `exploring_plan_mode` flag is `Arc<AtomicBool>` in `RunCore` — it
is flipped to `true` by `EnterPlanModeTool::execute()`.

### Tool registration

`enter_plan_mode` and `exit_plan_mode` are NOT in the default registry
(see test `default_registry_has_no_plan_mode_tools` in
`src/tools/mod.rs`). They must be added explicitly when building the
test `ToolRegistry`:

```rust
use recursive::tools::{EnterPlanModeTool, ExitPlanModeTool};
// ...
registry.register(Box::new(EnterPlanModeTool::new(
    gate.clone(),
    exploring_plan_mode.clone(),
)));
registry.register(Box::new(ExitPlanModeTool::new(
    gate.clone(),
    exploring_plan_mode.clone(),
)));
```

The `PlanApprovalGate` and `Arc<AtomicBool>` need to be shared with
`RunCore` — look at how `cli/builder.rs` constructs them.

## Acceptance criteria

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` no diff
- At least one new test `plan_mode_write_tool_blocked_until_exit` that:
  - Enters plan mode via `enter_plan_mode` tool
  - Attempts a write tool and gets a blocking error result
  - (Optional) Exits plan mode via `exit_plan_mode`

## Notes for the agent

- Look at `tests/integration.rs` for examples of how to use
  `MockProvider` and build `AgentRuntime` for testing.
- `EnterPlanModeTool` requires `PlanApprovalGate` and
  `Arc<AtomicBool>` — check `src/tools/plan_mode.rs` for the
  constructor signature.
- The `PlanApprovalGate::approve()` might need to be called in a
  background task so the test doesn't deadlock waiting for a human to
  approve. Look at existing plan mode unit tests (around line 547 in
  `src/tools/plan_mode.rs`) to see how they handle the async blocking
  with a `tokio::spawn`.
- `exit_plan_mode` also blocks on `PlanApprovalGate`. If you include
  the exit step, auto-approve it from a spawned task.
- Write tool to try: `WriteFile` (simplest) — check its constructor in
  `src/tools/fs.rs`.
- The `AgentRuntime` fixture pattern from `tests/integration.rs`
  shows the full setup including mock LLM provider.
