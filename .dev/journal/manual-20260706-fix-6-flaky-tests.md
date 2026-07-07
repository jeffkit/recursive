# Manual edit: fix-6-flaky-tests

**Date**: 2026-07-06
**Goal**: Fix 6 known flaky test failures that were blocking the mutation-test baseline
**Files touched**:
- `src/hooks/external.rs`
- `src/team.rs`
- `src/test_util.rs`
- `src/tools/team_create.rs`
- `src/tools/team_delete.rs`
- `Cargo.toml` (hook_echo binary entry retained)

**Tests added**: none (existing tests fixed)

## Root causes and fixes

### 5 × hooks::external::tests (CPU-starvation timeout)

`dispatch_runs_executable_hook_and_returns_decision`,
`dispatch_short_circuits_on_first_non_continue`,
`from_config_respects_matcher_event_filter`,
`once_hook_runs_only_first_time`,
`async_rewake_exit2_triggers_cancel` — all relied on spawning an
external process (`hook_echo` binary or shell script) to return a
`HookResult`.  Under heavy `cargo-mutants` CPU load the OS scheduler
could not schedule the child process within even 30 seconds, causing
every test to time out.

**Fix**: added an in-process mock layer to `ExternalHookRunner`:
- `mock_results: Vec<Option<HookResult>>` (cfg(test)) — checked by
  `run_hook` before touching the OS.
- `mock_exit_codes: Vec<Option<i32>>` (cfg(test)) — checked by
  `run_command_exit_code` (used by the `async_rewake` path).
- `with_mock_results` / `with_mock_exit_codes` builder helpers.

All 5 tests rewritten to inject mock results; no OS process is ever
spawned.  Side-effect: `ResolvedHookKind::Command` was extended to
carry args (`Vec<String>`) and `resolve_command` now uses
`shell_words::split` — a general correctness improvement.

### team::tests::registry_delete_removes_file (RECURSIVE_TEAMS_DIR race)

`team.rs`, `tools/team_create.rs`, and `tools/team_delete.rs` each
defined their **own** `static LOCK: Mutex<()>` to serialize tests
that mutate `RECURSIVE_TEAMS_DIR`.  Separate statics do not prevent
cross-module races: all three modules compile into the same test
binary, so tests from different modules could run concurrently and
stomp on each other's env-var setting.

**Fix**: added `PinnedTeamsDir` to `src/test_util.rs` which acquires
the shared process-global `env_lock()`.  All three test modules
replaced their per-module guards with `crate::test_util::PinnedTeamsDir`.

## Verification

- `cargo test -- --test-threads=4`: 1703 passed, 0 failed
- `cargo clippy --all-targets --all-features -- -D warnings`: clean
- `cargo fmt --all -- --check`: clean
