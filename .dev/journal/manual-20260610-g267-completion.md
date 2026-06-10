# Manual edit: g267 completion

**Date**: 2026-06-10
**Goal**: Complete Goal 267 (unified atomic_write) after self-improve.sh
e2e smoke gate failed on an unrelated argusai MCP session issue. The
agent had landed the helper module + 7 unit tests + session.rs
partial migration + a `meta_lock` race fix, but had not finished
migrating the remaining 5 production sites nor deleted the 3
duplicate `atomic_write` implementations. This journal records the
lead's manual completion.

**Files touched**:
- `src/atomic.rs` (new) — unchanged from agent; 8 unit tests pass
- `src/lib.rs` — added `pub mod atomic;` (agent missed this)
- `src/session.rs` — migrated 5 production sites to
  `crate::atomic::atomic_write`; deleted local `fn atomic_write`
  (lines 471-493 in the old layout)
- `src/cost.rs` — migrated 3 production sites to
  `crate::atomic::atomic_write`; `write_cost_json`, `update_meta_with_cost`,
  and the unit test fixture at line 357
- `src/transcript.rs` — migrated `TranscriptFile::write_to` to
  `crate::atomic::atomic_write`
- `src/checkpoint.rs` — migrated `restore_paths` per-file write to
  `crate::atomic::atomic_write` (most user-visible fix: torn restores
  on crash were a real concern)
- `src/team.rs` — deleted local `pub(crate) fn atomic_write`; migrated
  `save_team` and the `atomic_write_replaces_existing` unit test to
  `crate::atomic::atomic_write`; cleaned up an unused `Path` import
- `src/storage/local.rs` — deleted local `async fn atomic_write`;
  migrated `save_transcript` and `save_memory` to
  `crate::atomic::atomic_write_async`; cleaned up an unused `Path`
  import; `save_memory` now takes `value.as_bytes().to_vec()` (was
  `tokio::fs::write(&path, value)` — non-atomic)
- `scripts/_g267_*.py` — deleted (debug scripts left by the agent,
  not needed)

**Tests added**: none (the agent's 8 unit tests in `src/atomic.rs` pass
unchanged)

**Notes**:
- E2E smoke gate failure was a `SESSION_NOT_FOUND` from the
  `mcp2cli → argusai MCP` path, not a product bug. The worktree's
  argusai MCP session was not started; this is a known workflow
  limitation (see user memory: "argusai + recursive e2e pitfalls").
  Self-improve.sh auto-resumed the agent to fix, but the resumed
  agent hit minimax `context window exceeds limit (2013)` and
  crashed, so the run aborted as a "SMOKE-FIX still failing"
  rollback. Product code in the worktree was left intact; the
  `SMOKE-FIX` rollback does not actually reset the working tree
  in this case (also known: "self-improve auto-rollback can fail
  silently").
- Lead code review per OPERATIONS §3.4.1 found that the agent
  reported "0 matches for `std::fs::write`" but actually 7
  production sites remained. The agent's report was a green gate
  pass (1179 tests, clippy clean) but **not** a complete migration.
  This is the classic "agent does the minimum to pass tests but
  skips entire subsections" failure mode that the SOP §3.4.1
  warns about. Lead override to manually complete the migration
  is the correct branch on this failure mode.
- The race fix in `cost.rs::update_meta_with_cost` from the
  agent (acquiring `meta_lock` before the RMW) is preserved; the
  helper now does the atomic write under that lock.
- `restore_paths` change is the highest-value fix: a crash mid-restore
  previously truncated the user's working file to half its content;
  now the file stays at the old content until the atomic rename
  completes the restore.
