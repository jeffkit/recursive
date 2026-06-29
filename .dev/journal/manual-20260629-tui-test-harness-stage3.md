# Manual edit: tui-test-harness stage 3 (mutation-testing effectiveness loop)

**Date**: 2026-06-29
**Goal**: Close the "effectiveness loop" — give the AI (and CI) a way to
measure whether TUI tests actually pin down the changed behaviour, not
just pass. Uses `cargo-mutants` scoped to the files a change touches.

**Files touched**:
- `.dev/scripts/tui-mutants.sh` (new, executable) — scoped mutation runner.
- `crates/recursive-tui/src/harness.rs` — module doc extended with the
  "改某文件 → 杀该文件变异点" rule and script usage.

**Tooling**:
- `cargo install cargo-mutants --locked` (global, v27.1.0). This is a
  developer/CI tool, not a product dependency — `Cargo.toml` is unchanged.
  self-improve / CI environments are expected to have it on PATH.

**The script** (`.dev/scripts/tui-mutants.sh`):
- `--in-place` (uses the real target cache → fast incremental rebuilds).
- `--features recursive/test-utils` so the test-utils dev-dep is active.
- `--no-shuffle` for reproducible ordering.
- Modes:
  - no args → auto-detect files changed vs `main` (`git diff main...HEAD`
    + uncommitted) under `crates/recursive-tui/src/`, mutate those only.
  - `<file>...` → mutate the given files.
  - `--dir <path>` → mutate a directory recursively.
  - `--all` → whole crate (slow).
- Exit non-zero if any mutant survives → can gate a commit.

**Demos run** (in the worktree):
1. `tui-mutants.sh crates/recursive-tui/src/keymap.rs` → 2 mutants, 1
   caught, 1 unviable, 0 survived → exit 0. Confirms the mechanism kills
   mutants when tests are adequate.
2. `tui-mutants.sh` (auto-detect: harness.rs + lib.rs) → 6 mutants, all
   survived (MISSED), all in `lib.rs`:
   - `run` / `run_with_backend` → `Ok(())`
   - `RawModeGuard::drop` → `()`
   - `handle_mouse` + `ScrollUp` / `ScrollDown` arms
   These are **real coverage gaps**: `lib.rs`'s terminal-IO layer (raw
   mode, alternate screen, mouse scroll) has no in-process tests, because
   it is exactly the layer the stage-1 harness cannot reach. Stage 4's
   PTY harness covers it. The mutation result thus independently
   motivates stage 4. `harness.rs` produced no mutants (it is
   `#[cfg(test)]`-only; cargo-mutants targets product code by default).
3. Exit-code check: no-survivors run → exit 0; survivors run → exit 2.
   Gate behaviour confirmed.

**Design notes**:
- The default scope is "files changed vs main", not the whole crate. A
  full-crate run is slow (every mutant = build + test); scoping to the
  touched surface keeps the effectiveness check in the minutes range, so
  it is affordable as a per-commit gate.
- cargo-mutants generates control-flow mutants (return values, `if`
  conditions, deleted match arms). It does NOT emit the specific
  `with_list_offset(2) → with_list_offset(0)` mutation that the stage-2
  manual check used. The two mechanisms are complementary: cargo-mutants
  catches generic logic mutants; the harness visual tests catch the
  visual/logic-specific regressions (proven to bite in stage 2).

**Quality gates** (in `.worktrees/feat-tui-test-harness`):
- `cargo fmt --all --check` — clean
- `cargo clippy -p recursive-tui --all-targets -- -D warnings` — clean
- `cargo test -p recursive-tui --features recursive/test-utils` — 276
  passed, 0 failed

**Next**: stage 4 builds the `tui-pty` harness binary (portable-pty + vt100)
to cover exactly the `lib.rs` terminal-IO layer that stage 3 flagged as
untested.
