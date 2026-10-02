# 2026-10-02 — issue #61 pipeline NEEDS_FIX: merge main, drop phantom deletions

## Date
2026-10-02

## Goal
Reviewer rejected `v2-pipeline-61-1002165637-cont` @ `b6b4acfc` (NEEDS_FIX):
branch was 30 commits behind `main` (`a95ee999`), so `git diff main`
showed +2125/−5472 with every main-side feature (#56 agui server layer,
#66 cancel/streaming, #74 SkillSource, session-id collision guard, #49②
env-schema, flow fixes, v3 distributed host) appearing as deletions —
the documented "cross-PR landing phantom deletions" failure mode.
Required fix: merge current main, keep only the issue-#61 invariant
delta, drop the `web_fetch` cfg line if already on main, rework the
test-isolation edits onto main's newer guards, re-verify diff + gates.

## What was done
1. Backed up uncommitted isolation edits to a stash
   (`pre-merge backup: uncommitted isolation edits v2-pipeline-61`),
   plus a session checkpoint (`06296a2d08b2`).
2. `git merge main` (a95ee999). One conflict:
   `docs/architecture/index.md` — resolved to main's AG-UI line **plus**
   the branch's "ten invariants" wording.
3. Re-applied the isolation edits from the stash against main's code:
   - `src/test_util.rs`: dropped the local `PinnedNoSessionsDir`
     entirely — main's `SessionEnvPins` already pins
     `RECURSIVE_SESSIONS_DIR` inside the isolated home under a single
     `env_lock` acquisition. This also removes the field-order drop
     hazard the reviewer flagged (branch's `_pin` dropped before
     `_sessions`, so the Drop impl mutated env without the lock).
     File is now byte-identical to main.
   - `tests/incremental_writes.rs`, `tests/resume_by_id.rs`: dropped
     local sessions-dir save/restore guards for the same reason
     (main's `HomePin`/`SessionEnvGuard` already cover it). Identical
     to main.
   - `src/paths.rs`: kept the branch's `env_lock()` fixes — hold the
     lock around the `RECURSIVE_SESSIONS_DIR` override probe, and use
     `PinnedRecursiveHomeNoLock` + the held guard in
     `user_sessions_dir_creates_dir_when_absent`.
   - `src/http/handlers.rs`: kept save/restore of an inherited
     `RECURSIVE_SESSIONS_DIR` around
     `agui_non_resume_turn_seeds_full_messages_history`
     (main removes the var but never restores it; the pipeline sets it).
   - `tests/agui_e2e.rs`: main's `HomeOverride` already pins the
     sessions dir; kept only the exact
     `agui_session::thread_session_key(...)` matches for the
     `vis-thread` / `round-trip` finds (a bare `agui-` prefix can
     match a sibling slug) and the clearer `Interrupted` assert
     message. Comment wording tightened to not claim an ambient env
     cause (the guard itself already pins the var).
4. `web_fetch` cfg line: main already has the identical fix
   (`3a2a2fef`); merge made `src/tools/mod.rs` identical — no delta left.
5. `cargo fmt --all` (clean), committed the reworked edits as
   `22196934 fix(test): rework isolation edits onto main's session env pins`.

## Result: `git diff main` (after)
- 16 files, +561/−46 → all in the issue-#61 surface:
  ten-invariant renumbering (`docs/architecture/invariants.md`,
  `.dev/AGENTS.md`, `AGENTS.md` demoted to unnumbered pointer,
  `index.md`/`agent-loop.md` prose, `tests/invariants.rs` module wiring)
  + the new guard test `tests/invariants/invariant_registry.rs` (+243)
  + the reworked isolation edits (`src/paths.rs` env-lock, `handlers.rs`
  sessions-dir save/restore, `agui_e2e.rs` exact-key matches).
- `git diff main --diff-filter=D --name-only` → empty (zero deleted
  files). `src/`, `crates/`, `.dev/flows`, `.dev/scripts` deltas vs
  main: only the two `M` files above.
- Spot checks that main's landed work survived the merge: `writer.rs`
  collision test, flow `timeout_secs=5400`, `_eval` throws, s18
  parse-guard, `run_host_v3`/`note_retry`, `SkillSource`/`HttpSkillSource`,
  `agui_active_runs`/`CancelOnDrop`, gate-prompt no-internal-quotes.

## Tests added
None new (guard tests already on the branch: invariant_registry 243
lines). Re-verified the existing gates.

## Verification
- `cargo test --workspace` → **3848 passed, 0 failed**
  (one flaky first run: `cmd_resume_accepts_matching_tool_registry_hash`
  failed in the cold build, passed on re-run — pass-row shows 0).
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` → clean.
- `cargo fmt --all -- --check` → clean.
- `cargo check --no-default-features --lib` → clean (web_fetch cfg intact).
- `cargo test -p recursive-agent --test invariants` → 47 passed incl.
  the new `invariant_registry` guards.

## Notes
- Merge commit: `18a4c444`; rework commit: `22196934`.
- Backing stash was dropped after clean re-application; session
  checkpoint `06296a2d08b2` (pre-merge) remains as a restore point.
- Flaky-test observation for future goals:
  `cmd_resume_accepts_matching_tool_registry_hash` (recursive-cli)
  failed once on a cold parallel build, passed in isolation and in the
  full re-run — likely a cold-build/parallelism artifact, not tracked
  further here.
