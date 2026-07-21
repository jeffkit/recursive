# Manual edit: align-edit-guard-with-fakecc

**Date**: 2026-07-21
**Goal**: Continue the in-progress worktree `feat/align-edit-guard-with-fakecc` aligning the Edit/Write pre-read guard with `fake-cc` (~/Downloads/fake-cc). Picked up the partially-done P0 items, completed the remaining P1/P2 items, fixed compile/test breakage, and added coverage.
**Files touched**:
- `src/tools/fs.rs` — `ReadRecord` enriched with `content` + `timestamp`; `ReadFileState::record` now takes content+mtime and does LRU eviction (cap 100); `get_file_mtime` helper; `WriteFile` gained `read_state`, pre-read guard (existing files only, new files exempt), staleness check with content fallback, call-time re-validation before write, and post-write cache update.
- `src/tools/edit.rs` — Edit staleness check (mtime + content fallback), call-time re-validation right before `tokio::fs::write`, post-edit cache update, `MAX_EDIT_FILE_SIZE` (1 GiB) size guard before reading.
- `src/tools/registry.rs` — `WriteFile` wired with shared `read_state`.
- `crates/recursive-cli/src/cli/builder.rs` — wired `read_state` into the CLI's `WriteFile` so the guard is actually active for the main `recursive` agent (previously only `registry.rs::build_standard_tools_with_roots` wired it; the CLI builder is the production path and was missing it).
- `src/tools/agent.rs` — sub-agent `Write` tool now shares the sub-agent `read_state`.
- `crates/recursive-cli/src/cli/control.rs` — `seed_read_state` updated for the new `record` signature (reads content + mtime).
- `src/config.rs` — fixed pre-existing flaky test `default_max_steps_is_unlimited` (pinned `RECURSIVE_HOME` to an empty temp dir + `env_lock`) that our new sync IO in two edit tests shifted the schedule enough to trip in parallel runs.
**Tests added**:
- `fs::tests`: `write_new_file_allowed_without_prior_read`, `write_existing_file_rejected_without_prior_read`, `write_existing_file_rejected_when_partial_read`, `write_succeeds_after_full_read`, `write_rejected_when_file_modified_since_read`, `write_post_update_cache_allows_consecutive_write`, `write_stale_mtime_but_unchanged_content_allowed`.
- `edit::tests`: `edit_rejected_when_file_modified_since_read`, `edit_allowed_when_mtime_bumped_but_content_unchanged`, `edit_post_update_cache_allows_consecutive_edit`, `edit_rejected_for_oversized_file` (sparse file via `set_len` to avoid 1 GiB disk usage).
**Notes**:
- P2.8 (`isPartialView` for auto-injected context) is **N/A** for recursive: recursive does not auto-inject file contents into `ReadFileState` the way fake-cc injects CLAUDE.md; context files are read straight into the system prompt, never via the `ReadFile` tool, so there is no auto-inject cache entry to mark partial. Skipped rather than adding an unused field.
- P2.10 (path normalization): recursive already canonicalizes cache keys via `resolve_within_any`, so the cache key is normalised. No change needed.
- Staleness tests use `File::set_modified` (stable since 1.75; toolchain is 1.95) to bump mtime 1 h into the future so `disk_mtime > cached_ts` fires deterministically regardless of FS timestamp resolution.
- The oversized-file test uses `File::set_len(MAX_EDIT_FILE_SIZE + 1)` to create a sparse file — logical size > 1 GiB with no real disk allocation.
- All three quality gates clean: `cargo test --workspace` (1988 lib tests, 0 failed), `cargo clippy --all-targets --all-features -- -D warnings` (no warnings), `cargo fmt --all` (clean).
