# Manual edit: fix-plan-mode-headless

**Date**: 2026-06-04
**Goal**: Prevent plan mode deadlock in headless/batch runs by making plan mode tools opt-in per channel.
**Files touched**:
- `src/runtime.rs` — added `with_plan_mode_tools: bool` field to `AgentRuntimeBuilder` (default `false`); wrapped plan mode tool registration in `if self.with_plan_mode_tools`; updated test to opt in
- `src/cli/builder.rs` — added `interactive: bool` parameter; wired `.with_plan_mode_tools(interactive)` into builder chain
- `src/cli/resume.rs` — added missing `interactive: true` argument to `build_runtime` call
- `src/main.rs` — updated all 5 `build_runtime` call sites with correct `interactive` value (`true` for CLI/REPL, `false` for WeChat daemon and tests)
- `src/tui/runtime_builder.rs` — added `.with_plan_mode_tools(true)` to both `build_runtime()` and `build_runtime_with_skill_tx()` builder chains

**Tests added**: Updated `runtime::tests::runtime_builder_has_plan_mode_tools` to call `.with_plan_mode_tools(true)` before `.build()`

**Notes**: Root cause of the deadlock — `ExitPlanModeTool::execute()` calls `gate.wait_for_approval().await` which loops on `Notify::notified()`. In headless mode the `PlanProposed` event goes to `NullSink`, so nobody ever calls `gate.approve()` or `gate.reject()`, causing the process to hang forever. The fix follows the architectural principle: the kernel registers only core non-blocking tools; interactive channels (TUI, CLI) opt in to blocking/interactive tools like plan mode by calling `.with_plan_mode_tools(true)`.
