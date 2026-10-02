# 2026-10-03 — pipeline-68 NEEDS_FIX remediation: rebase onto main + drop leaked test artifacts

## Date
2026-10-03 03:0x (+0800)

## Goal
Address reviewer blockers on `v2-pipeline-68-1003001814-cont` (issue #68,
per-request system prompt for `/agui`):
1. Branch forked at `a95ee999` and never rebased — `git diff main` reverted
   21 commits of landed work (dae17f80 real-time transcript persistence +
   SIGTERM watchdog, b11b0987 feature-matrix, 22196934/3b7ad2f8 #61/#55
   test isolation, 74110390/f2260c3c/46cb6e2c v3-host).
2. 73 session-artifact files (`agui-*`, `tmp/agui-*`, `var-folders-*`
   transcript dirs) committed in the WIP snapshot `858f12b8` — leaked from
   `agui_prompt_fixture`, which returns (releasing `env_lock`) *before*
   `agui_post` runs the HTTP request and never pinned
   `RECURSIVE_SESSIONS_DIR`.

## Files touched
- History: rebased branch onto `main` (`46cb6e2c`), dropping the WIP
  artifact commit via `git rebase -i` with `drop` for `858f12b8`.
  Branch is now 4 commits on top of main; backup ref:
  `backup/pipeline-68-pre-rebase-10030181` (= old `0e6ccd52`).
- `src/http/handlers.rs` — conflict resolution keeps main's
  `saved_sessions_dir` save/restore in `agui_non_resume_turn_seeds_full_messages_history`;
  `agui_prompt_fixture` now pins `RECURSIVE_SESSIONS_DIR` to an isolated
  root under the temp home for the whole test process lifetime (the guard
  drops with the helper's return, so the pin must be an explicit set_var,
  matching the sibling handler-test pattern).
- `src/paths.rs` — `user_sessions_dir` treats an **empty** override as
  unset (it carried no destination; `PathBuf::from("")` resolves
  CWD-relative — the exact leak vector). Regression assert added to
  `sessions_dir_honors_recursive_sessions_dir_override`. Conflict in
  `user_sessions_dir_creates_dir_when_absent` resolved in favor of the
  branch's deadlock fix (main's `PinnedRecursiveHomeNoLock` variant
  reintroduced the same self-deadlock the branch fixed — both target the
  non-reentrant lock, branch form keeps the tightened assert).
- `.dev/journal/manual-20261003-agui-per-request-system-prompt.md` —
  conflict-resolved (kept, content union).

## Tests added
- `sessions_dir_honors_recursive_sessions_dir_override` extended: empty
  override must fall through to the default `<home>/…/sessions` layout.
- No new test files; #68 acceptance suite (12 handler tests + protocol
  camelCase test) unchanged and green.

## Gates
- `cargo test --workspace`: **3859 passed / 0 failed** (was 3853 on the
  stale branch; +6 from main's new tests).
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: clean
  (after dropping the now-unused `PinnedRecursiveHomeNoLock` import the
  rebase left in `src/paths.rs`).
- `cargo clippy --lib --no-default-features`: clean.
- `cargo fmt --all`: applied. Note: `cargo fmt --all -- --check` flags
  pre-existing main-side files (`resume.rs:422`, `incremental_writes.rs`)
  — reproducible on stock main with `rustfmt --check --edition 2021` in
  isolation; main evidently gates with a different rustfmt style. Left
  byte-identical to main on purpose (reformatting them here would churn
  the diff); all **branch-authored** hunks are fmt-clean.

## Notes
- Reviewer's content-level concern "restores `emit_turn_messages` batch"
  etc. all disappear with the rebase — verified `git diff main..HEAD`
  has zero deletions outside the 4 legitimately-overlapping files
  (handlers.rs / paths.rs / agui_e2e.rs / incremental_writes.rs) and
  those 18 removed lines are all test-isolation shuffles inside the #68
  feature itself.
- Artifact dirs still present **untracked** on disk from the old runs;
  they are not in the index. `clean`/`gc` of the worktree is left to the
  flow harness.
- `git log HEAD..main` = 0: branch strictly contains main.
