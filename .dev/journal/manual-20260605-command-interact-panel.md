# Manual edit: command-interact-panel

**Date**: 2026-06-05
**Goal**: Add an interactive command panel that opens below the input box (same slot as command dropdown), pushing messages area upward. Commands can now return `CommandOutcome::OpenPanel(CommandPanelState)` to open a persistent panel instead of an ephemeral modal.
**Files touched**:
- src/tui/input_state.rs — added `InputMode::CommandInteract` variant; updated all match arms
- src/tui/app/mod.rs — added `CommandPanelState` struct + builder methods; added `App::active_command_panel` field
- src/tui/app/state.rs — init `active_command_panel: None`; added `open_command_panel` / `close_command_panel` helpers
- src/tui/commands.rs — added `CommandOutcome::OpenPanel(CommandPanelState)`; removed `PartialEq` derive (not needed)
- src/tui/app/commands.rs — route `CommandInteract` mode keys to `handle_command_panel_key`; handle `OpenPanel` in dispatch; add `CommandInteract` arm in `submit_prompt`; implement `handle_command_panel_key`
- src/tui/ui/command_menu.rs — `panel_height` handles `CommandInteract`; `render_panel` dispatches to new `render_command_interact_panel`
- src/tui/ui/input.rs — indicator colour, box title, footer hint for `CommandInteract`
**Tests added**: none (visual/structural change; existing tests all pass)
**Notes**:
- Panel height is driven by `CommandPanelState::height` (set at construction), capped at `MAX_VISIBLE + 2`.
- Esc always closes the panel. Up/Down move selection. Enter confirms (default: close).
- Commands that need richer interaction after Enter should check `active_command_panel.context` in a custom handler before calling `close_command_panel`.
- The orange (#cd6432) border matches the banner accent colour for visual consistency.
