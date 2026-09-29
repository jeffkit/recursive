# Issue #47 fixes — bounded drains, agent-mode deadlines, REPL plan-approval timeout

- Date: 2026-09-29
- Goal: issue #47 — eliminate every unbounded await that can park a turn
  forever without external interruption.
- Files touched:
  - `src/tools/transport.rs` — all three exec paths (SSH exec, SSH write,
    local exec_shell) now drain their stdout/stderr reader tasks through
    `drain_with_grace` (DRAIN_GRACE = 2s). A mirror buffer recovers partial
    output when an orphaned descendant holds the pipe write ends. The
    unused `read_capped` was folded into `spawn_reader`.
  - `src/tools/agent.rs` — `execute_single` / `execute_sequential` gain the
    same child-token cancel + deadline select as `execute_parallel`
    (issue #47②). `execute_parallel`'s deadline is now always `Some` via
    `effective_deadline()` (configured wall timeout, else fallback 3600s
    const), and the `(None, None)` unbounded branch is deleted (#47③);
    fallback is overridable via `with_fallback_deadline_secs` for tests.
  - `src/tools/plan_mode.rs` / `src/runtime.rs` / `src/runtime/builder.rs` /
    `crates/recursive-cli/src/main.rs` — `ExitPlanModeTool` gains an optional
    `approval_wait_timeout`; the REPL loop opts in with 300s; on timeout the
    plan is rejected with "plan approval timed out" (#47④). TUI/SDK default
    stays wait-forever.
  - `tests/issue47_local_drain.rs` — promoted from the wip repro; adds the
    timeout-branch promptness case.
- Tests added: `local_exec_shell_timeout_branch_returns_promptly`,
  `execute_parallel_unconfigured_wall_timeout_still_bounded`,
  `execute_single_cancel_returns_within_budget`,
  `execute_single_wall_timeout_returns`,
  `execute_sequential_wall_timeout_returns`,
  `wait_for_approval_times_out_and_rejects`.
- Notes: drain-grace partial output is recovered from a mutex mirror rather
  than dropped, so `sleep 8 & echo hi` still surfaces `hi`. Sequential mode
  stops at the first cut-off worker and surfaces the label as data (#7).
