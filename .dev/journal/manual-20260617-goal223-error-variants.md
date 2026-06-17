# Manual edit: goal223-error-variants

**Date**: 2026-06-17
**Goal**: Narrow Error variant granularity — add `call_id` to `Error::Tool`, replace `Error::Other` with `Error::Internal`
**Files touched**:
- `src/error.rs` — add `call_id: Option<String>` to `Error::Tool`, replace `Other(String)` with `Internal { context, message }`
- `src/team.rs` — replace 2× `Error::Other` with `Error::Internal`
- `src/tui/backend.rs` — replace 2× `crate::Error::Other` with `crate::Error::Internal`
- 27 additional files (batch script) — mechanical insertion of `call_id: None,` at all `Error::Tool { name:` struct literal sites
- `.dev/goals/229-01-unwrap-cleanup-runtime.md` — created sub-goal file for first unwrap cleanup batch

**Tests added**: none (existing tests updated for new struct field)

**Notes**:
- Used a Python script to batch-insert `call_id: None,` into 164 `Error::Tool` struct literal constructions across 27 files
- The dispatch path in `tools/mod.rs` (`dispatch_after_permission_check`) currently uses `call_id: None` as a placeholder; threading the real `call_id` from `invoke_with_audit` is deferred (requires adding the param to the function signature, a separate concern)
- All 1245+ tests green; clippy clean
- Commit: `5d6af91`
