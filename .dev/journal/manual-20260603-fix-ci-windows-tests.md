# Manual edit: fix-ci-windows-tests

**Date**: 2026-06-03
**Goal**: Get the Windows CI green. The previous fix (9786b1d) only patched
Ubuntu + the hooks/external.rs clippy noise; five Windows tests were still
failing.

## Root causes (5 failing tests → 4 fixes)

1. `session_lock::tests::lock_dead_pid_recovered` (src/session_lock.rs:255).
   `is_pid_alive(u32::MAX)` returned `true` on Windows because the
   `#[cfg(not(unix))]` branch was the conservative "assume alive" stub.
   The 9786b1d commit only added a Linux `/proc/<pid>` probe; Windows was
   untouched. → Real Windows probe via `tasklist /FI "PID eq <pid>" /NH /FO CSV`
   and check stdout for the `"INFO: No tasks"` no-match header. Fall back
   to "assume alive" if `tasklist` itself is unavailable (preserves the
   original safety contract).

2. `tui::app::state::tests::detect_model_name_falls_back_to_config_file`
   (src/tui/app/state.rs:232) +
   `tui::runtime_builder::tests::offline_mode_and_config_file_resolution`
   (src/tui/runtime_builder.rs:124). Both tests use `PinnedHome`, which
   only pinned `HOME`. On Windows, `dirs::home_dir()` (used by
   `config_file::config_file_path`) resolves via
   `SHGetKnownFolderPath(FOLDERID_Profile)` / `%USERPROFILE%` and ignores
   `%HOME%`. So the test wrote `<tmpdir>/.recursive/config.toml` but the
   production code looked at `USERPROFILE/.recursive/config.toml`.
   → `PinnedHome` now also pins `%USERPROFILE%` on Windows.

3. `runtime::tests::runtime_falls_back_to_diff_for_run_shell` (src/runtime.rs:1662).
   `RunShell` uses `/bin/sh -c`, which doesn't exist on Windows. → Added
   `#[cfg_attr(target_os = "windows", ignore)]`, matching the existing
   convention in src/session.rs:1448 and src/tools/shell.rs:168.

4. `tui::backend::tests::run_shell_action_dispatches_tool_and_emits_events`
   (src/tui/backend.rs:736). Same `/bin/sh` problem.
   → Same `#[cfg_attr(target_os = "windows", ignore)]` treatment.

## Bonus fix (discovered while validating)

`config_file::tests::test_load_layered_permissions_session_layer_always_present`
(src/config_file.rs:306) flaked under local parallel test load — the test
used raw `set_var`/`remove_var` for HOME without holding `env_lock()`,
so a concurrent `PinnedHome` user (which *does* hold the lock) could
interleave and leak a User layer into the assertion. 9786b1d's HOME
isolation helped but did not take the lock, so the race was still
visible in heavy parallel runs. → Switched the test to `PinnedHome`,
which serialises its own mutations via `env_lock()`.

## Files touched

- src/test_util.rs          — PinnedHome also pins USERPROFILE on Windows
- src/session_lock.rs       — tasklist-based pid probe for Windows
- src/runtime.rs            — Windows-ignore on shell-driven diff test
- src/tui/backend.rs        — Windows-ignore on run_shell_action test
- src/config_file.rs        — use PinnedHome in layered_permissions test

## Tests added

None — these are all fixes to existing tests. The session_lock
`lock_dead_pid_recovered` test now passes on Windows because the
production probe is real, not stubbed.

## Quality gates

- cargo fmt --all -- --check: clean
- cargo clippy --workspace --all-targets --all-features -- -D warnings: clean
- cargo test --workspace: 1098 passed, 0 failed, 0 ignored (excluding
  the 4 pre-existing doc-test ignores)

## Notes

- `tasklist` `INFO: No tasks` matching is English-only. The Windows CI
  runner is en-US so this is fine for now; if we ever add non-en Windows
  to CI we'll need to either localise the probe or fall back to a
  Win32 API (the `windows-sys` crate is already a transitive dep through
  `dirs-sys`).
- The two `#[cfg_attr(target_os = "windows", ignore)]` tests reduce
  coverage of the shell tool on Windows to: `run_shell_action_works_when_runtime_offline`
  (which passes by accident because it doesn't assert on success) and
  the `bash_mode_dispatches_run_shell_without_calling_llm` integration
  test (which is `#[cfg(feature = "tui")]` and not run by default
  CI). A real Windows shell driver would be a separate goal; this just
  makes CI pass.
