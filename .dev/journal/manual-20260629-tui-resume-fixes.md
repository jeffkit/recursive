# Manual edit: tui-resume-fixes

**Date**: 2026-06-29
**Goal**: Fix two `/resume` (TUI command panel) bugs.

1. **Highlight bar / `▶` marker misalignment.** The interactive command
   panel renderer (`render_command_interact_panel`) highlighted
   `lines[selected]`, but list builders prepend a header + blank spacer,
   so the highlight bar sat two rows above the `▶` marker (which is drawn
   at the item row). Added a `list_offset` field to `CommandPanelState`
   (+ `with_list_offset` builder); the renderer now highlights
   `lines[list_offset + selected]`. Set `list_offset = 2` on the resume,
   journal, and theme panels so the bar and marker share a row.

2. **Resume only appended a note instead of replacing the conversation.**
   `UiEvent::SessionResumed` previously just pushed a "▶ Resumed session …"
   System block while the visible `blocks` kept the current chat. The
   backend now reconstructs the resumed conversation via a new pure helper
   `app::render::blocks_from_messages` (maps `Message`s → `TranscriptBlock`s:
   skips system prompt, rebuilds user/assistant/reasoning text and pairs
   tool calls with their results) and ships it in the event. The event-loop
   handler replaces `self.blocks` with the rebuilt transcript, then appends
   the resume note.

**Files touched**:
- crates/recursive-tui/src/app/mod.rs (CommandPanelState.list_offset)
- crates/recursive-tui/src/ui/command_menu.rs (highlight offset)
- crates/recursive-tui/src/commands.rs (with_list_offset on 3 panels + test)
- crates/recursive-tui/src/app/render.rs (blocks_from_messages + tests)
- crates/recursive-tui/src/events.rs (SessionResumed carries blocks)
- crates/recursive-tui/src/backend.rs (build + send blocks)
- crates/recursive-tui/src/app/event_loop.rs (replace blocks on resume)

**Tests added**:
- `theme_panel_list_offset_aligns_highlight_with_marker`
- `blocks_from_messages_reconstructs_conversation`
- `blocks_from_messages_orphan_tool_result_renders_standalone`

**Notes**: `UiEvent` derives `Eq`, and `recursive::message::Message` does
not (its `ToolCall.arguments` is `serde_json::Value`). So the event carries
`Vec<TranscriptBlock>` (which is `Eq`) rather than raw messages; the
message→block conversion happens in the backend before sending. Persisted
tool results carry no success flag, so they render as succeeded. Run the
crate's tests with `--features recursive/test-utils` (pre-existing
requirement for `state.rs` / `runtime_builder.rs`).
