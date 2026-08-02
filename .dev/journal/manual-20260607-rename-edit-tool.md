# Manual edit: rename-edit-tool

**Date**: 2026-06-07
**Goal**: Align internal naming with the public tool name "Edit"
**Files touched**:
- `src/tools/str_replace.rs` → deleted (replaced by `src/tools/edit.rs`)
- `src/tools/edit.rs` → new (renamed copy, `StrReplaceTool` → `EditTool`)
- `src/tools/mod.rs` → `pub mod str_replace` → `pub mod edit`, `StrReplaceTool` → `EditTool`
- `src/tools/fs.rs` → comment-only: `StrReplaceTool` → `EditTool`

**Tests added**: none (pure rename, no behavior change)
**Notes**: The tool has exposed itself as `"Edit"` to the LLM since goal 258.
This commit makes the internal Rust naming (`EditTool`, `edit.rs`) match
the external name. No API or behavior change.
