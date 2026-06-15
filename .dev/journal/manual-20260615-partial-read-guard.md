# Manual edit: partial-read-guard

**Date**: 2026-06-15
**Goal**: Goal 261 — Add ReadFileState shared between ReadFile and StrReplaceTool to reject edits on files that were never read or only partially read (line range). Follows the same Arc<Mutex<...>> pattern as TouchedFiles (Goal 164).
**Files touched**:
- src/tools/fs.rs (ReadFileState, ReadRecord, ReadFile.read_state field)
- src/tools/str_replace.rs (StrReplaceTool.read_state field + guard)
- src/tools/mod.rs (ToolRegistry::with_read_file_state)
- src/run_core.rs (wire ReadFileState into main registry)
- src/tools/spawn_worker.rs (wire fresh ReadFileState into sub-agent registry)
**Tests added**:
- src/tools/fs.rs: read_state_records_full_read, read_state_records_partial_read, read_state_full_range_not_partial
- src/tools/str_replace.rs: edit_rejected_when_file_never_read, edit_rejected_when_partial_read, edit_allowed_after_full_read, edit_allowed_when_no_read_state
**Notes**: Guard is opt-in — StrReplaceTool::new() sets read_state: None and behaves as before. Sub-agents get a fresh empty ReadFileState. ReadFileState is not reset between turns.
