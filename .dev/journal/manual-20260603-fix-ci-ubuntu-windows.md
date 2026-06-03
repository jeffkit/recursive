# Manual edit: fix-ci-ubuntu-windows

**Date**: 2026-06-03
**Goal**: Fix three recurring CI failures on Ubuntu and Windows that macOS never hits.
**Files touched**: src/hooks/external.rs, src/config_file.rs, src/session_lock.rs

**Root causes and fixes**:

1. **Windows Clippy: `unused variable: runner`** (`hooks/external.rs`)
   Five test functions declared `let runner = ExternalHookRunner::discover(...)` outside
   any `#[cfg(unix)]` guard but only used it inside one. Windows saw an unused variable
   and `-D warnings` turned it into a hard error.
   Fix: promoted `#[cfg(unix)]` to the whole test function for
   `dispatch_runs_executable_hook_and_returns_decision`,
   `dispatch_treats_timeout_as_continue`, `dispatch_treats_bad_output_as_continue`,
   `dispatch_short_circuits_on_first_non_continue`, and `discover_collects_executable`.

2. **Ubuntu test flap: `test_load_layered_permissions_session_layer_always_present`** (`config_file.rs`)
   This test asserted `layers.len() == 1` (Session only), but the parallel test
   `test_load_layered_permissions_loads_user_and_project` mutates the global `HOME`
   env var mid-run. If both ran concurrently, the first test could see a non-empty
   `HOME` pointing at a tmp dir that had a real `.recursive/config.toml`, adding an
   unexpected User layer.
   Fix: gave the first test its own isolated `fake_home` tempdir and bracketed it with
   the same `set_var/restore` pattern the second test already used.

3. **Ubuntu test failure: `lock_dead_pid_recovered`** (`session_lock.rs`)
   The test forged a stale lock with pid `u32::MAX` and expected `is_pid_alive` to
   return false so recovery could proceed. On Linux, `/bin/kill -0 4294967295` exits
   with code 2 (`EINVAL`, not `ESRCH`) on some kernel/coreutils versions, and the exit
   status interpretation was fragile.
   Fix: on `target_os = "linux"` replaced the `kill -0` probe with a direct
   `/proc/<pid>` existence check, which is unambiguous for any PID value.

**Tests added**: none (fixed existing tests)
**Notes**: `cargo fmt --all` also cleaned up minor tui formatting drift from prior commits.
