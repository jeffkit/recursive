# Journal — 2026-10-02 — issue #61 guard verification pass

## Date
2026-10-02

## Goal
Re-verify the #61 fix (invariant numbering conflict, resolved earlier as
route B — ten invariants) against the issue's acceptance list, with fresh
negative tests of both guard tests and the full quality gate. No product
code changes were needed: the fix and guards from `7706ef2` / `0ee7ef8`
(+ the 2c24055d prose sweep) were already in place on this branch.

## Files touched
- `.dev/issues/09-invariant-numbering-conflict.md` — created (the local
  archive the issue references; it did not exist in any prior worktree).
- `.dev/journal/manual-20261002-issue61-guard-verification.md` — this file.
- No changes to `.dev/AGENTS.md`, `docs/architecture/invariants.md`,
  root `AGENTS.md`, or `tests/` — verified byte-identical before/after the
  negative tests (backup + `diff`).

## Negative tests (both guards proven to fire)
1. `invariant_numbering_agrees_across_documents`: temporarily renamed
   `## Invariant #2 — Orthogonality` → `… BROKEN-FOR-NEGATIVE-TEST` in the
   architecture doc → test FAILED with the full (number → normalized-title)
   diff of both maps (left #2 `orthogonality` vs right
   `orthogonalitybrokenfornegativetest`). Restored → green.
2. `every_numbered_invariant_has_automated_enforcement`: two probes on
   `.dev/AGENTS.md`:
   - renaming entry `7.`'s marker so the parser sees a different number →
     guard still green (parser keyed on `N. **`, not on specific numbers —
     renaming a title does not trip it, correctly);
   - replacing entry #6's `Automated test: …` lines with prose ("See scripts
     dir for the checker.") → FAILED with
     `invariant #6 in .dev/AGENTS.md has no automated enforcement listed`.
   Restored from backup → `diff` confirms byte-identical.

## Environment finding (why the suite "failed" before this session's probes)
Running `cargo test --workspace` with the flow's ambient env exposes two
**pre-existing, environment-sensitive test isolation bugs** — NOT #61
regressions; both are green once the offending env is unset:
- `RECURSIVE_SESSIONS_DIR` set (as `launch-flow.sh` does for pipeline
  isolation) leaks into `src/agui_session.rs` tests: the legacy-migration
  test asserts `SessionReader::list_sessions(ws).len() == 1` but the hard
  override points the listing at the shared pipeline sessions dir (observed
  17→25→26 entries, growing as other tests write there). It also makes the
  test's own `agui-legacy-thread/` fixture and temp-dir workspace-slug
  sessions land **inside the repo**, requiring manual cleanup afterwards.
  `IsolatedWorkspace` pins `RECURSIVE_HOME` but never
  `RECURSIVE_SESSIONS_DIR`, and `paths::user_sessions_dir` treats
  `RECURSIVE_SESSIONS_DIR` as a hard override — so the TUI test that
  saved/restores that var (modal.rs:1839) is not enough when the var is
  set process-wide.
- `RECURSIVE_WORKSPACE` set leaks into
  `config::from_env_injects_memory_and_scratchpad_layers`
  (config.rs:3215 saves/restores it — but the *episodic recall* lookup
  reads the real workspace's sessions when the test's own workspace has
  none... actually the inverse: with `RECURSIVE_WORKSPACE` pointing at the
  flow worktree, the test's session writes go to the pinned-home copy while
  the summary reads through the env workspace → episodic layer empty).
  Also fixed by `env -u RECURSIVE_WORKSPACE`.
Filed observation here for a future isolation goal: test-side env pinning
should cover `RECURSIVE_SESSIONS_DIR` the same way it covers
`RECURSIVE_HOME` (e.g. clear it inside `IsolatedWorkspace`).

## Tests added
None — verification-only session.

## Verification (quality gates, with offending env unset)
- `cargo test --workspace` → 58/58 suites `test result: ok`, 0 failures
  (lib alone: 2437 passed).
- `cargo clippy --all-targets --all-features -- -D warnings` → Finished,
  no warnings.
- `cargo fmt --all --check` → clean.
- `git status` clean after removing the stray fixture dirs the polluted-env
  run had dropped (`agui-legacy-thread/`, `var-folders-…tmp*/`).

## Notes
- Acceptance criteria from the issue are all met and recorded in the new
  local archive `.dev/issues/09-invariant-numbering-conflict.md`.
- The two isolation bugs above are the only remaining (pre-existing) gap;
  they do not affect the #61 acceptance criteria.
