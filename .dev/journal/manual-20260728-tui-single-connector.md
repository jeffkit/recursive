# Manual edit: tui-single-connector

**Date**: 2026-07-28
**Goal**: Match Claude's tool-call display style — show ⎿ connector only on the first result line; subsequent lines use plain 7-space indentation instead of repeating the connector on every row.
**Files touched**:
- `crates/recursive-tui/src/ui/transcript.rs`

**Tests added**:
- `render_tool_call_only_first_result_line_has_connector` — asserts exactly one result line (across size row + content rows) carries the ⎿ glyph when output has multiple lines.

**Notes**:
- Used a `connector_used: bool` captured by a `next_pfx` closure inside the `Some(ToolResultData)` match arm. The closure flips `connector_used` true on first call and returns `"    ⎿  "` (7 cells), then returns `"       "` (7 spaces) for all subsequent calls.
- The `None` arm (Running…) is a single line and unchanged — connector is appropriate there.
- GitNexus impact: LOW risk, direct callers are `render_block` + 3 tests only.
- Quality gates: cargo test (767/767), clippy (0 warnings), fmt (clean).
