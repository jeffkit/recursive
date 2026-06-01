# Manual edit: Goal 165 — Plan Mode 2.0

**Date**: 2026-06-01
**Goal**: Implement agent-driven read-only planning mode (Plan Mode 2.0)
**Files touched**:
- `src/tools/plan_mode.rs` (new) — PlanApprovalGate, EnterPlanModeTool, ExitPlanModeTool
- `src/agent.rs` — added `exploring_plan_mode: Arc<AtomicBool>` to `RunCore`; plan mode check in `execute_tool_calls`
- `src/kernel.rs` — added `exploring_plan_mode` to `TurnContext`; passed to `RunCore`
- `src/runtime.rs` — added `plan_approval_gate` field; updated `confirm_plan`, `reject_plan`, `build`, `set_event_sink`, `run`
- `src/tools/mod.rs` — added `pub mod plan_mode`; registered tools in `build_standard_tools`; added re-exports
- `src/tools/sub_agent.rs` — added `exploring_plan_mode` to `TurnContext` construction (default off)
- `src/multi.rs` — added `exploring_plan_mode` to `TurnContext` construction (default off)
- `src/config.rs` — added `## Planning Mode` section to `default_system_prompt`; updated size test limit
- `src/lib.rs` — re-exported `EnterPlanModeTool`, `ExitPlanModeTool`, `PlanApprovalGate`, `PlanApprovalResult`

**Tests added**: 6 unit tests in `src/tools/plan_mode.rs`
1. `enter_plan_mode_returns_confirmation_message` — verifies flag set + JSON response
2. `exit_plan_mode_blocks_until_confirmed` — spawns approver task, verifies blocks+unblocks
3. `exit_plan_mode_returns_rejection_reason` — spawns rejector task, verifies reason propagated
4. `gate_approve_wakes_waiter` — gate API level test
5. `gate_reject_wakes_waiter_with_reason` — gate API level test
6. `gate_response_cleared_after_wait` — ensures gate is reusable

**Notes**:
- `PlanningMode::PlanFirst` / `Immediate` kept entirely intact — no regressions
- `exploring_plan_mode` is a separate `Arc<AtomicBool>` inside `PlanApprovalGate`, orthogonal to `PlanningMode`
- `wait_for_approval` uses loop + `Notify::notified().await`, never holds a lock across `.await`
- Deprecated `Agent` path gets `Arc::new(AtomicBool::new(false))` as default (plan mode 2.0 not wired there)
- `build_standard_tools` registers placeholder tools (with isolated default gate) that are overridden by `AgentRuntimeBuilder::build()` with the real coordinated gate
- `confirm_plan` / `reject_plan` on `AgentRuntime` now cover both PlanFirst and Plan Mode 2.0 flows
- `set_event_sink` re-registers `ExitPlanModeTool` with new sink, consistent with `TodoWriteTool` pattern
- Config test limit bumped to 6 KiB (was 4 KiB) to accommodate added Planning Mode section
