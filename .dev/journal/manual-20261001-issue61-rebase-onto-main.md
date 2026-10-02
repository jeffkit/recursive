# Journal — 2026-10-01 — issue #61 continuation: NEEDS_FIX resolution (rebase onto main)

## Date
2026-10-01

## Goal
Independent review returned NEEDS_FIX on `v2-pipeline-61-1001180648-cont`:
the branch had re-inherited the stale baseline `aaa49cc`, so `git diff main`
showed ~4.7k lines of landed product work (#57/#63/#65/#70/#58/#54 + flow
scripts) as deletions, and `cargo check --no-default-features --lib` failed.
Fix = rebase the branch's own doc/test changes onto current `main`
(the flow's rebase-retry path, done by hand), keep only the invariant-doc +
guard-test delta, and restore the minimal-feature build.

## Files touched
- Rebased 3 commits (`7706ef2`, `0ee7ef8`, plus the uncommitted residual-prose
  sweep, committed first as `2c24055`) onto `main` @ `c21f791`.
  - Conflict: `docs/architecture/invariants.md` Quick-Reference table row 8 —
    main (#59, commit `788c819`/`a318b4d`) had already fixed
    `src/compact.rs` → `src/compact/` (living-paths guard
    `tests/docs_living_paths.rs` enforces this). Resolved keeping
    `src/compact/` + our new rows 9/10. All other files auto-merged.
  - Verified `git diff main` now touches ONLY: `.dev/AGENTS.md`,
    `AGENTS.md`, `docs/architecture/{invariants,agent-loop,index}.md`,
    `.dev/proposals/compaction-upgrade.md`, both `self-improve-cycle`
    SKILL.md copies, `tests/invariants.rs`,
    `tests/invariants/invariant_registry.rs` (new), two journals.
    No `src/`, `crates/`, `Cargo.*`, `.github/`, `e2e/`, `.dev/flows/`
    changes remain except the one-line build fix below.
- `src/tools/mod.rs:169` — one-line fix, commit `f279dad`: restore
  `#[cfg(feature = "web_fetch")]` on `pub use web_fetch::WebFetch`.
  **This breakage is pre-existing on main, not introduced by the rebase:**
  commit `23189cd` (#63) added the `http_call` re-export block and in the
  same hunk dropped the cfg from the adjacent `WebFetch` re-export
  (module decl at line 76 and the `registry.rs:1115` registration kept
  theirs), so `cargo check --no-default-features --lib` fails on main
  @ `c21f791` with E0432 (`unresolved import web_fetch`). Confirmed by
  checking out main's `src/tools/mod.rs` in a clean tree — same error.
  The reviewer's reported E0433 (`crate::mcp` in `src/acp/session.rs:178`)
  was from the stale-baseline tree; on current main that file compiles
  because `acp` is cfg-gated (`src/lib.rs:20`); the only remaining
  no-default-features error is the WebFetch re-export. CI guards
  (`.github/workflows/ci.yml` feature-gate steps) are intact on main and
  untouched here — they were never deleted by us.

## Tests added
None new — `tests/invariants/invariant_registry.rs` (46→47 tests in the
`invariants` suite with `living_docs_reference_existing_paths` counted
separately) is unchanged from the reviewed state.

## Verification
- `git merge-base main HEAD` = `c21f791` (main HEAD); branch is strictly ahead.
- `git diff main --stat` — docs/tests/journal only (12 files, +461/−42),
  plus the 1-line `src/tools/mod.rs` cfg fix.
- `cargo test --workspace`: all green (lib 2437, cli 821, invariants 47,
  docs_living_paths 1 — path-guard now passes with our doc edits; zero
  failures; the reviewer's `timeout_kills_child_process` env flake did not
  reproduce).
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: clean.
- `cargo fmt --all` + `--check`: clean.
- `cargo check --no-default-features --lib`: **passes** (was failing);
  also green with `RUSTFLAGS=-D warnings`, and
  `cargo check --no-default-features --features http --lib` passes.

## Notes
- Pre-rebase branch state preserved at
  `v2-pipeline-61-1001180648-cont-backup-pre-rebase` (commit `ba6b1be`).
- The stale-baseline mass-deletion the reviewer flagged is gone by
  construction: a merge of this branch into main is now a true fast-forward
  of main + our docs/test delta; none of `src/agui_session.rs`,
  `src/tools/http_call.rs`, the CI feature-gate steps, the `acp` feature,
  the `message_module_does_not_import_llm` guard, or the flow scripts are
  touched.
- Residual "8 invariants" prose sweep (5 files + journal) landed as
  `2c24055` before rebasing so it would ride along cleanly.
