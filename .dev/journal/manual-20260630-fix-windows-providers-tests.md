# Manual edit: fix-windows-providers-tests

**Date**: 2026-06-30
**Goal**: Windows CI `test (windows-latest)` job has been failing on every
main push. Diagnose the root cause and fix it for real, instead of the
earlier incomplete patch.

## Diagnosis

The Windows job failed exactly two unit tests in `src/providers.rs`:

- `providers::tests::additional_presets_loads_valid_file` (line 368:
  `assert_eq!(loaded.len(), 1, "expected exactly one override")`)
- `providers::tests::find_preset_extended_finds_user_override`

Both wrote a `providers.d` override into `<tmp>/.recursive/providers.d/`
and then called `additional_presets()`, which reads from
`providers_d_dir()` → `paths::user_data_dir()`.

`user_data_dir()` short-circuits on `RECURSIVE_HOME`, then falls back to
`dirs::home_dir().join(".recursive")`. The tests pinned `HOME` (and,
since the 2026-06-03 journal, `USERPROFILE` on Windows) via `PinnedHome`
but did **not** set `RECURSIVE_HOME`.

Root cause: **`dirs` 5.0.1's `home_dir()` on Windows resolves only via
`SHGetKnownFolderPath(FOLDERID_Profile)` and ignores both `HOME` and
`USERPROFILE` env vars at runtime.** Verified against the `dirs-sys`
0.4.1 and `dirs` 5.0.1 source on docs.rs. So `PinnedHome` cannot
redirect `dirs::home_dir()` on Windows — the production code kept
reading `C:\Users\runneradmin\.recursive\providers.d` (which doesn't
exist on the runner) and returned 0 overrides, failing the `== 1`
assertion. The earlier journal (`manual-20260603-fix-ci-windows-tests.md`)
assumed pinning `USERPROFILE` would be honored by `dirs`; that assumption
was wrong. The TUI tests had already discovered this and switched to
`PinnedRecursiveHome` (see comment at
`crates/recursive-tui/src/runtime_builder.rs:335`), but `providers.rs`
was never updated.

The two "empty/invalid" providers tests passed on Windows only
vacuously (the real home has no `providers.d`), so they were silently
testing the wrong thing too.

## Fix

- `src/providers.rs`: all four Goal-254 `providers.d` tests now use
  `PinnedRecursiveHome::new(tmp.path().join(".recursive"))` instead of
  `PinnedHome::new(tmp.path())`. `RECURSIVE_HOME` is the first
  short-circuit branch in `user_data_dir()`, so the pin takes effect on
  every platform (Windows included) without depending on `dirs`' env-var
  behavior. The tests still write to `tmp/.recursive/providers.d`, which
  now equals `providers_d_dir()` on Windows too.
- `src/test_util.rs`: rewrote the `PinnedHome` doc comment to state
  accurately that `dirs::home_dir()` honors **neither** `HOME` nor
  `USERPROFILE` on Windows, and to redirect future authors to
  `PinnedRecursiveHome` for any `dirs`-based path. The struct/impl are
  unchanged (still pins `HOME` + `USERPROFILE`, which is needed by
  product code that reads `HOME` directly via `std::env::var_os`).

## Files touched

- `src/providers.rs` — 4 tests switched to `PinnedRecursiveHome`.
- `src/test_util.rs` — corrected/m expanded `PinnedHome` doc comment.
- `crates/recursive-tui/src/skill_commands.rs` — one-line clippy fix
  (`needless_borrows_for_generic_args`: drop `&` on a generic arg) in
  **pre-existing uncommitted WIP** (symlink-resolution test). Not part
  of the Windows fix, but it blocked the workspace clippy gate
  (`-D warnings`). Mechanical fix applied per clippy's own suggestion.

## Tests added

None — these are fixes to existing tests plus a doc-comment correction.

## Quality gates

- `cargo fmt --all -- --check`: clean
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`:
  clean
- `cargo test --workspace`: all green (0 failed), including the 4
  providers tests that were failing on Windows CI

## Notes

- Could not reproduce the Windows behavior on macOS (dirs honors `HOME`
  on Unix), but the fix is logically cross-platform: `RECURSIVE_HOME` is
  checked before the `dirs` fallback on every target, so the tests no
  longer depend on `dirs`' env-var behavior at all.
- Latent inconsistency (not fixed here, out of scope):
  `paths::user_data_dir()` treats `RECURSIVE_HOME` as the `.recursive`
  dir itself (`PathBuf::from(custom)`), while
  `config_file::config_file_path()` treats it as the HOME root
  (`custom.join(".recursive").join("config.toml")`). Worth a separate
  goal if it ever bites; left untouched to keep this change surgical.
