# Manual edit: Promote `src/compact.rs` to `src/compact/` module directory

**Date**: 2026-07-23
**Goal**: Goal 329 — move the monolith `src/compact.rs` to `src/compact/mod.rs`
to establish a directory for the compaction upgrade series (goals 329–342:
microcompactor, reinject, retry, prompt submodules). Behavior-preserving rename
only — no internal splitting.
**Files touched**:
- `src/compact.rs` → `src/compact/mod.rs` (`git mv`, 100% similarity, 0 content delta)
- `tests/invariants/test_coverage.rs` — updated path from `"src/compact.rs"` to
  `"src/compact/mod.rs"` in `MUST_HAVE_TESTS` list (the test reads the file to
  verify `#[cfg(test)]` presence; the new path is what exists on disk after the move).
**Tests added**: none (existing tests are the proof — all pass unchanged).
**Notes**:
- `crate::compact` module path is identical whether the file is `src/compact.rs`
  or `src/compact/mod.rs` — no call site needed updating. Verified via `rg`.
- Quality gates: `cargo build --workspace` ✅, `cargo test --workspace` ✅ (all
  3000+ tests pass), `cargo clippy --all-targets --all-features -- -D warnings` ✅,
  `cargo fmt --all` ✅.
- `git diff --stat` shows `src/{compact.rs => compact/mod.rs} | 0` (rename)
  and `test_coverage.rs | 2 +-` (path update). `git diff` confirms 100% similarity
  on the rename.
