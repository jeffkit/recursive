# Manual edit: modal-scroll-review

**Date**: 2026-06-03
**Goal**: Review all 9 modal windows for usability in the dynamic inline viewport, and add scroll support to all of them.
**Files touched**:
- `src/tui/app/mod.rs` — added `modal_scroll: u16` field
- `src/tui/app/state.rs` — initialized `modal_scroll: 0`; added `push_modal()` helper that resets scroll on each push
- `src/tui/app/commands.rs` — rewrote `handle_modal_key`, `handle_resume_picker_key`, `handle_mcp_servers_key` to use the new `modal_scroll_follow_selection()` auto-scroll; added `modal_scroll_follow_selection()` and `MODAL_LIST_VISIBLE` constant; replaced the one production `modals.push()` call with `push_modal()`
- `src/tui/app/event_loop.rs` — replaced `modals.push()` for McpServersLoaded with `push_modal()`
- `src/tui/ui/modal.rs` — changed centred_rect from 70% to 88×90%; added `.scroll((app.modal_scroll, 0))` to the Paragraph; limited Journal preview to 12 lines with "… N more lines" hint
**Tests added**: none (behaviour covered by existing modal key tests)
**Notes**:
- Generic text modals (Help, ToolList, PlanReview, CostDetail, ModelInfo, Confirm) now support ↑/↓/PageUp/PageDown for vertical scrolling.
- List modals (ResumePicker, McpServers, Journal) auto-scroll to keep the selected item visible as the user navigates.
- scroll is reset to 0 every time a new modal is pushed.
