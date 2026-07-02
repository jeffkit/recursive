# Manual edit: win-ci-fix

**Date**: 2026-07-02
**Goal**: Fix GitHub CI `test (windows-latest)` failures in
`crates/recursive-tui`. Two unit tests panicked on Windows due to
OS-specific path/separator handling; ubuntu/macos passed.

**Files touched**:
- `crates/recursive-tui/src/completion.rs`
- `crates/recursive-tui/src/runtime_builder.rs`

**Root causes**:
1. `collect_files` built relative paths via `Path::strip_prefix(..).to_string_lossy()`,
   which on Windows yields backslash separators (`d1\d2\l3.txt`). The test
   `collect_files_walks_four_levels_deep` (and `@`-completion callers) expect
   forward-slash rel paths (`d1/d2/l3.txt`). Failed assertion:
   `depth-1 file should be collected; got ["d1\\d2\\d3\\l3.txt", ...]`.
2. `discover_loaded_skills` parsed `RECURSIVE_SKILL_PATHS` with
   `env_paths.split(':')`. On Windows a single drive path like
   `C:\Users\...\tmp.XX` splits at the drive-letter colon into `["C", "\\Users\\..."]`,
   so no real skill dir is scanned → `expected demo-skill in []`.

**Fixes**:
1. Normalize rel paths to `/` in `collect_files` (`replace('\\', "/")`).
2. Replace `split(':')` with `std::env::split_paths` (OS-native separator:
   `;` on Windows, `:` on Unix). Doc comment updated.

**Tests added**:
- `completion::debt_tests::collect_files_emits_forward_slash_relative_paths`
  — pins the forward-slash contract across platforms (no backslashes, exact
  rel-path equality).
- Extended `runtime_builder::tests::discover_loaded_skills_reads_env_paths`
  — now sets two skill roots joined via `std::env::join_paths` and asserts
  both `demo-skill` and `other-skill` are discovered, pinning the
  OS-native separator split. Kept as a single test to avoid the env-var
  parallel race (per `.dev/AGENTS.md` env-test rule); serialized by
  `PinnedRecursiveHome`'s global env lock.

**Quality gates** (all clean):
- `cargo test --workspace` — all pass (recursive-tui: 661 passed).
- `cargo clippy --all-targets --all-features -- -D warnings` — clean.
- `cargo fmt --all -- --check` — clean.
- `.dev/scripts/tui-test-presence.sh` — PASS (test-bearing change detected).

**Notes**:
- `.dev/scripts/tui-mutants.sh` has a pre-existing bug (`ARGS[@]: unbound
  variable` under `set -u` when no extra args) that prevents it from running
  locally without args. It is not exercised by `.github/workflows/ci.yml`
  (CI only runs fmt/clippy/build/test), and `.dev/` is out of scope per
  `CLAUDE.md` unless explicitly requested, so left unfixed here. The
  Flowcast `tui-mutants` flow gate invokes the script with explicit args
  and is unaffected.
- No new dependencies (std `std::env::split_paths` / `join_paths`).
