# Journal — 2026-10-02 — `RECURSIVE_SESSIONS_DIR` test isolation (fixes `agui_session` legacy-migration failure + 3 sibling suites)

## Date
2026-10-02

## Goal
`cargo test --workspace` failed with
`agui_session::tests::legacy_flat_thread_is_migrated_and_becomes_visible`
(`assert_eq!(list_sessions(ws).len(), 1)` → saw 42+). This is the
environment-sensitivity gap flagged in
`manual-20261002-issue61-guard-verification.md` ("test-side env pinning
should cover `RECURSIVE_SESSIONS_DIR`"): the orchestrator process sets
`RECURSIVE_SESSIONS_DIR` (self-improve bridge, `self_improve_bridge_v2.py:113`)
and `paths::user_sessions_dir` treats it as a **hard override that beats
`RECURSIVE_HOME`**, so any test that pinned only `RECURSIVE_HOME` still
listed/wrote the pipeline's shared store.

## Root cause (three layers)
1. **`IsolatedWorkspace` (src/test_util.rs) pinned `RECURSIVE_HOME` only.**
   With the override set, `agui_session` tests saw the pipeline store's 42+
   sessions and dropped their `agui-legacy-thread/` fixture *inside the
   shared store* (leak, grew on every run).
2. **`tests/agui_e2e.rs` located the thread session by `starts_with("agui-")`
   prefix** instead of the exact `thread_session_key`. With the override
   active, `list_sessions(ws)` returned every slug's sessions in the shared
   store, and the first `agui-*` hit could be *another test run's leftover*
   → `message_count`/`status` assertions failed non-deterministically
   (flaky: depended on tempdir sort order).
3. **`tests/incremental_writes.rs` + `tests/resume_by_id.rs`** also pinned
   only `RECURSIVE_HOME` → same-store pollution (count drift 2→6, 2→119).

## Files touched
- `src/test_util.rs` — new `PinnedNoSessionsDir` RAII guard (clears
  `RECURSIVE_SESSIONS_DIR`, restores on drop; no self-locking — constructed
  inside `IsolatedWorkspace`, whose pins already hold `env_lock`);
  `IsolatedWorkspace` now holds one.
- `src/paths.rs` — the two `RECURSIVE_SESSIONS_DIR`-mutating tests
  (`sessions_dir_honors_recursive_sessions_dir_override`,
  `user_sessions_dir_creates_dir_when_absent`) now hold `env_lock`
  (second uses `PinnedRecursiveHomeNoLock` to avoid re-lock deadlock).
- `src/http/handlers.rs` — `agui_non_resume_turn_seeds_full_messages_history`
  now saves/restores the var instead of unconditionally removing it
  (it holds `env_lock` but the restore previously erased a pipeline value
  for every later test in the binary).
- `tests/agui_e2e.rs` — `HomeOverride` also clears
  `RECURSIVE_SESSIONS_DIR`; `agui_run_persists_a_listable_native_session`
  matches the session dir by exact `thread_session_key("vis-thread")`
  (the round-trip/fence tests already did).
- `tests/incremental_writes.rs` — `HomePin` clears/restores the var.
- `tests/resume_by_id.rs` — `HomeOverride` clears/restores the var.

## Tests added
None new — the failing tests are the regression tests; this fixes their
isolation. No `#[allow]` silencers.

## Verification
- `cargo test --workspace` → exit 0, 58/58 suites ok, 3809 tests passed,
  0 failed (run twice; one transient `clang` linker segfault on
  `recursive_tui` bin — machine-level, gone on rerun, unrelated).
- `cargo clippy --all-targets --all-features -- -D warnings` → clean.
- `cargo fmt --all -- --check` → clean.
- Stray fixtures cleaned: `tmp-g152-test-ws/`,
  `var-folders-…tmp*/` in worktree root, plus the `agui-legacy-thread/`
  and `agui-old/` leak in the shared pipeline store (those two are pure
  test fixtures, safe to delete).

## Notes
- `PinnedNoSessionsDir` deliberately does **not** acquire `env_lock`
  itself: it is only constructed inside `IsolatedWorkspace`, whose sibling
  pins hold the lock — `std::sync::Mutex` is not re-entrant.
- The `paths.rs` unit tests were the remaining in-binary unserialised
  mutators of the var; they now hold `env_lock` like every other
  env-mutating test (repo policy, `.dev/AGENTS.md` "Env-var tests").
