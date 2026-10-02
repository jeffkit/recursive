# Manual fix — test isolation for `RECURSIVE_SESSIONS_DIR`

- **Date**: 2026-10-02
- **Goal**: fix `cargo test --workspace` failures that only reproduce when the
  host environment sets `RECURSIVE_SESSIONS_DIR` (issue-keeper pipeline layout:
  `/Users/.../.issue-keeper/pipeline/recursive-68/sessions`).
- **Files touched**:
  - `src/http/handlers.rs` — removed the `std::env::remove_var("RECURSIVE_SESSIONS_DIR" /
    "RECURSIVE_WORKSPACE")` calls from the issue-#68 test bodies. These tests run under the
    shared `env_lock()` in the same process as every other test binary; unsetting the
    var at test end left a window where parallel tests resolved sessions into a
    global dir. Tests now leave env mutation to the paired pin guards.
  - `src/test_util.rs` — `IsolatedWorkspace::new()` also pins
    `RECURSIVE_SESSIONS_DIR` to `<home>/sessions` inside `SessionEnvPins`
    (one `env_lock` acquisition covers both variables). The hard override beats
    `RECURSIVE_HOME`, so a host-level value previously leaked other tests'
    session files into `SessionReader::list_sessions` (episodic_recall +
    session::tests asserted on shared real-pipeline transcripts).
  - `tests/incremental_writes.rs`, `tests/orphan_resume.rs`,
    `tests/checkpoint_e2e.rs` — local `HomePin`/`HomeOverride` structs gained a
    `PinnedSessionsDir` pin (borrows the `PinnedRecursiveHome` to prove the env
    lock is held, restores the previous value on Drop).
  - `tests/resume_by_id.rs` — already carried a combined `SessionEnvGuard`
    (home + sessions dir) on main; unchanged there.
- **Tests added**: none — this is an isolation fix; existing assertions are
  unchanged (they were correct; the environment leaked under them).
- **Notes**:
  - Root cause of the flaky matrix: `paths::user_sessions_dir` honours
    `RECURSIVE_SESSIONS_DIR` as a hard override (Goal-H J1, commit d84d90e).
    The orchestrator env for this worktree sets it to the pipeline sessions
    dir, so any test that only pinned `RECURSIVE_HOME` still resolved
    `list_sessions` / `SessionWriter::create` into the shared pipeline
    directory. Symptoms: `messages_persisted_before_finalize` saw
    `transcript[0].role == System` (another test's compact-boundary summary),
    counts like 9/11/13 instead of 2, `episodic_recall` matched real pipeline
    session content.
  - Rebase note: this fix was first authored against the pre-#56 monolithic
    `handlers.rs`; after the branch was rebased onto main (layered
    `src/http/agui.rs`), the handlers-side hunks shrank to dropping the
    `remove_var` calls from the issue-#68 tests only — the resume/seeding
    paths they used to touch now live in `src/http/agui.rs::prepare_run` and
    never read these variables at handler level.
  - Verification: `cargo test --workspace` green with `RECURSIVE_SESSIONS_DIR`
    both set (host env) and unset; `cargo clippy --workspace --all-targets
    --all-features -- -D warnings` clean; `cargo check --no-default-features
    --lib` green (main's #54 minimal-feature gate); `cargo fmt --all` applied.
