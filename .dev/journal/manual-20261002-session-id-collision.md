# 2026-10-02 — session_id same-second collision (incremental_writes red)

## Date
2026-10-02

## Goal
Fix the failing `cargo test --workspace` run: `tests/incremental_writes.rs`
(`messages_persisted_before_finalize` / `streaming_partial_tokens_dont_persist`
/ `resume_after_clean_run_no_double_write` / `resume_after_crash_orphan_visible`)
failed with transcripts containing other tests' messages (left=12/10/8/4 lines,
right=2).

## Root cause
`SessionWriter::create_with_tools` derived
`session_id = filesystem_safe_timestamp() + slug` with **1-second resolution**.
`tests/incremental_writes.rs` shares one stable workspace (`/tmp/g152-test-ws`),
so several tests created sessions in the same second → same session dir →
`create_dir_all` reused it, `transcript.jsonl` opened in append mode, and every
writer appended into a shared file. This is the pre-existing flake documented in
`.dev/journal/manual-20261002-agui-server-layer.md` Caveat 1 and the arch-review
findings (`manual-20260605-arch-review.md` §5.1). It also corrupted real state:
while this workspace's pipeline exports `RECURSIVE_SESSIONS_DIR`, ~100 merged
transcripts accumulated under `sessions/tmp-g152-test-ws/`.

The sentinel `SessionLock` cannot prevent this: it only guards *concurrent*
writers; a second `create()` after the first test ended (same second) acquires
the released lock and appends into the old transcript.

## Fix (product, not test-side)
`src/session/writer.rs::create_with_tools` — allocate a unique directory:
- attempt 0 keeps the legacy `<ts>-<slug>` name when the dir does not exist
  yet and has no `transcript.jsonl`;
- otherwise (dir exists / holds a transcript / lock busy or stale) retry with a
  `<ts>-<slug>-<8-hex>` suffix (uuid v4), up to 8 attempts, then error.

So an existing session's transcript is never appended to by a new session.
`open_existing` (resume) is untouched — resume is *supposed* to append.

`src/test_util.rs::PinnedRecursiveHome` — additionally pins away
`RECURSIVE_SESSIONS_DIR` for the guard's lifetime (it is a HARD override that
beats `RECURSIVE_HOME`, Goal-H J1). A leaked value from the surrounding
process previously defeated the guard's isolation and made
`tests/resume_by_id.rs::most_recent_shortcut_picks_active_or_interrupted`
see 400+ foreign sessions.

## Tests added
- `session::writer::tests::create_in_same_second_gets_distinct_dirs` — two
  `create` calls in the same second on one workspace must get distinct dirs/ids.

## Files touched
- `src/session/writer.rs` (collision-proof dir allocation + regression test)
- `src/test_util.rs` (PinnedRecursiveHome also clears RECURSIVE_SESSIONS_DIR)
- (pre-existing working-tree changes in `src/http/agui.rs` / `README.md` were
  not part of this fix)

## Verification
- `cargo test --workspace` — 3773 passed, 0 failed (twice, and 8× the
  previously-failing `incremental_writes` target).
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — clean.
- `cargo fmt --all` — applied.

## Notes
- One unrelated timing-flaky test surfaced during full runs:
  `tests/issue40-parallel-agent-budget.rs::…wall_clock_exceeded` failed once
  under full-suite load but passes in isolation (5s wall budget; passes 4/4
  reruns). Not touched here.
- Old merged transcripts under the leaked `RECURSIVE_SESSIONS_DIR` tree
  (`sessions/tmp-g152-test-ws/`, `sessions/tmp-g151-test-ws/`) are artifacts of
  the bug and can be deleted manually if desired.
