# Manual edit: goal-363 — `recursive loop` honors exit codes; broadened `exit_for_finish`

**Date**: 2026-08-02
**Goal**: Make `recursive loop` propagate a non-zero exit when the agent terminates
abnormally (budget / stuck / wall-clock / transcript-limit / permission-denial /
provider-stop), and broaden `exit_for_finish` so every non-success, non-cancel
`FinishReason` exits 1 instead of 0. CLI-only; the agent kernel, run loop, tools, and
`FinishReason` enum are untouched.

## Files touched

- `crates/recursive-cli/src/main.rs` (1 line)
  - `run_loop` ending: `let _ = cli::output::exit_for_finish(...)` →
    `return cli::output::exit_for_finish(...)`. Previously the result was discarded and
    the trailing `Ok(())` made `recursive loop` exit 0 unconditionally. Now mirrors
    `run_once` (main.rs:2419) and `cmd_resume` (cli/resume.rs:639).
- `crates/recursive-cli/src/cli/output.rs` (+118 lines)
  - `exit_for_finish` rewritten: explicit arms for all 8 `FinishReason` variants.
  - Tests: 11 new unit tests in the existing `#[cfg(test)] mod tests`.
- `.dev/journal/manual-20260802-goal363-loop-exit-codes.md` (this file).

## Design decisions

1. **`FinishReason` is `#[non_exhaustive]`** (src/agent/types.rs:51). A match on it
   from `recursive-cli` (a different crate) MUST include a wildcard arm or rustc
   rejects the build (E0004). The goal text asked for "no `_ =>` catch-all", which is
   impossible cross-crate here, so all 8 variants are matched explicitly by name and
   the required wildcard is present but *conservative*: `_ => bail!("agent finished
   with unknown reason: {finish}")`. A supervisor must never see exit 0 for an
   unrecognised terminal state. This is the minimal deviation from the letter of the
   goal while fully satisfying its intent (grep still shows all 8 named arms).

2. **`ProviderStop` success set = `"stop"`, `"end_turn"`, `""`.** Verified in
   `src/run_core.rs:626` (`handle_no_tool_calls`): normal completions with
   `finish_reason` `"stop"` or `"end_turn"` are classified as `NoMoreToolCalls`, and
   `ProviderStop` is only constructed for other reason strings. So in practice
   `ProviderStop("stop")` never occurs, but treating it (and `end_turn`, and empty) as
   `Ok(())` keeps the function robust against provider-specific bare stops. Everything
   else (`rate_limited`, `404`, `context_length_exceeded`, `length`, …) → `bail!`.

3. **`Cancelled` stays `Ok(())`** — intentional (doc'd at output.rs:118-123): SIGINT /
   SIGTERM is user-initiated; the self-improve flow keys auto-resume off the exit code,
   so a non-zero exit here would re-run something the user explicitly stopped. Preserved.

4. **Exit code numbers unchanged** — `Ok(())` → 0, `Err` → 1 via anyhow's main handler.

## Tests added (all in `crates/recursive-cli/src/cli/output.rs`)

Pure unit tests on `exit_for_finish` (no runtime / no process spawn):

- `exit_for_finish_success_returns_ok` — `NoMoreToolCalls` → `Ok`.
- `exit_for_finish_cancelled_returns_ok` — `Cancelled` → `Ok` (pins auto-resume contract).
- `exit_for_finish_budget_exceeded_errors` — `Err`, msg contains "step budget" + steps.
- `exit_for_finish_stuck_errors` — `Err`, msg contains "stuck" + tool name + repeats.
- `exit_for_finish_wallclock_errors` — `Err`, msg contains "wall-clock" + secs.
- `exit_for_finish_transcript_limit_errors` — `Err`, msg contains size + limit.
- `exit_for_finish_permission_denial_limit_errors` — `Err`.
- `exit_for_finish_provider_stop_stop_is_ok` / `_end_turn_is_ok` / `_empty_is_ok` — `Ok`.
- `exit_for_finish_provider_stop_error_fails` — `ProviderStop("rate_limited")` → `Err`,
  msg contains "provider stopped" + reason.

## Verification

- `cargo test -p recursive-cli output::` → 14 passed (11 new + 3 existing).
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` → clean.
- `cargo fmt --all` → clean.
- `rg "let _ = .*exit_for_finish" crates/recursive-cli/` → 0 hits.
- All 8 `FinishReason` variants matched by name in `output.rs`.
- `cargo test --workspace` → green (run after this journal was drafted).

## Notes

- `run_once` / `cmd_resume` / HTTP / AG-UI handlers and `.dev/flows/` untouched.
- No new dependencies; no production code outside `recursive-cli` touched.
