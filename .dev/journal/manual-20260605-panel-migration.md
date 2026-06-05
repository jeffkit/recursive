# Manual edit: panel-migration

**Date**: 2026-06-05
**Goal**: Migrate all modal-triggered /commands to the new bottom-panel system
**Files touched**:
- `src/tui/commands.rs` — replaced `cmd_resume` and `cmd_theme` with `OpenPanel`; added all line-builder functions (`build_help_lines`, `build_cost_lines`, `build_model_lines`, `build_tool_lines`, `build_journal_lines`, `build_resume_lines`, `build_theme_picker_lines`, `serde_journal_context`, `serde_resume_context`); updated tests
- `src/tui/app/commands.rs` — rewrote `handle_command_panel_key` with PgUp/PgDn scroll, selection-aware Up/Down that rebuilds lines; added `rebuild_panel_lines_for_selection`, `confirm_command_panel`; fixed `submit_prompt` to not call `record_submission` when mode is `CommandInteract`
**Tests added**: updated existing tests (`help_opens_panel`, `cost_opens_panel`, `tools_opens_panel_with_catalog`, `cmd_theme_no_args_opens_picker_panel`, `submit_in_command_mode_dispatches_to_registry`)
**Notes**: `/mcp` remains as async `UserAction::ListMcpServers` — the full async flow that sends `Modal::McpServers` via `UiEvent` wasn't changed; that would require hooking the event handler to populate a panel instead.
