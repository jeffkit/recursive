# Issue #47 hardening — unbounded awaits bounded (drain, agent modes, plan gate)

- Date: 2026-09-29
- Goal: issue #47 root-cause follow-up (#40 hang was non-reproducible, original
  signature undiagnosable) — harden the 5 unbounded-await points identified in
  the analysis so no turn can park forever without external interruption.
- Files touched:
  - `src/tools/transport.rs` — SSH exec / SSH write / local exec_shell drain
    stdout/stderr via `drain_with_grace` (2s grace) with a mirror buffer for
    partial output; unused `read_capped` folded into `spawn_reader` (#47①).
  - `src/tools/agent.rs` — `execute_single` / `execute_sequential` gain the
    same child-token cancel + deadline `select!` as `execute_parallel` (#47②);
    parallel deadline always set via `effective_deadline()` (wall timeout or
    3600s fallback, `with_fallback_deadline_secs` for tests), the `(None, None)`
    unbounded branch deleted (#47③).
  - `src/tools/plan_mode.rs`, `src/runtime.rs`, `src/runtime/builder.rs`,
    `crates/recursive-cli/src/main.rs` — `ExitPlanModeTool::with_approval_wait_timeout`;
    REPL opts in at 300s, timeout routes through `gate.reject()` so the turn
    finishes (#47④). TUI/SDK keep wait-forever default.
  - `tests/issue47_local_drain.rs` (new) — local drain timeout-branch
    promptness; `tests/integration.rs` — agent-mode bounds.
- Tests added: `local_exec_shell_timeout_branch_returns_promptly`,
  `execute_parallel_unconfigured_wall_timeout_still_bounded`,
  `execute_single_cancel_returns_within_budget`,
  `execute_single_wall_timeout_returns`,
  `execute_sequential_wall_timeout_returns`,
  `wait_for_approval_times_out_and_rejects`.
- Notes: companion records in `manual-20260929-issue47-fixes.md` and
  `manual-20260929-issue47-triage-criteria.md` (triage criteria + #40 errata).
