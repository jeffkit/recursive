# Manual edit: quality-fixes

**Date**: 2026-06-22
**Goal**: Fix code quality issues found in automated audit round 1
**Files touched**: src/main.rs, src/cli/resume.rs, src/run_core.rs, src/cli/init.rs, src/tools/e2b_provider.rs, src/http/mod.rs, src/http/handlers.rs, src/tui/backend.rs
**Tests added**: none
**Notes**: Issues found via automated generalPurpose agent analysis. All fixes verified with cargo test + clippy + fmt.

## Summary of changes

- **HIGH-3** (`src/main.rs`): Fixed `ctrl_c.await.unwrap()` in `tokio::spawn` on non-unix platforms — now uses `if let Err(e)` to log errors instead of panicking silently.
- **HIGH-1** (`src/cli/resume.rs`): Added `#[allow(clippy::unwrap_used, reason = "mutex poison is unrecoverable")]` annotations to four `lock().unwrap()` calls, consistent with the pattern in `src/tools/facts.rs`. The `eprintln!` inline call was extracted to a separate `let` binding first.
- **MEDIUM-1** (`src/run_core.rs`): Removed redundant `args.clone()` inside the `join_set.spawn` async block — `args` was already cloned from `pc.args` and moved into the block.
- **MEDIUM-2** (`src/run_core.rs`): Extracted `attach_reasoning_content(&mut self, reasoning: Option<String>)` helper method to eliminate the duplicate `if reasoning_content.is_some()` blocks on the no-tool-calls and tool-calls paths.
- **MEDIUM-4** (`src/cli/init.rs`): Deleted the unused `default_model_for_preset` function (and its two unit tests) that was marked `#[allow(dead_code)]`.
- **LOW-2** (`src/tools/e2b_provider.rs`): Added `exit_code != 0` check after exec — non-zero exit codes now return `Error::Tool` instead of silently returning stdout/stderr output as if the command succeeded. Removed `#[allow(dead_code)]` from the field.
- **LOW-3** (`src/run_core.rs`): Extracted `const MIN_TRIM_LENGTH: usize = 200` constant to replace the magic number in `maybe_trim_transcript`.
- **LOW-4** (`src/run_core.rs`): Changed `HashMap::new()` to `HashMap::with_capacity(batch.len())` for `batch_map` to avoid rehash when the batch size is known.
- **LOW-5** (`src/http/mod.rs`): Removed the unused `status()` getter method that was marked `#[allow(dead_code)]`.
- **LOW-1** (`src/tui/backend.rs`): `Either::Right` is actually used under `#[cfg(feature = "weixin")]` — the existing `#[allow(dead_code)]` is correct for non-weixin builds. No change needed.
- **MEDIUM-6** (`src/http/handlers.rs`): `map_agent_event` already had a `///` doc comment explaining its `None` semantics. No change needed.
