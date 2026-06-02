# Manual edit: Goal-169 TUI skill commands — autocomplete + help modal

**Date**: 2026-06-02
**Goal**: Complete Goal-169 by wiring skill commands into the TUI command autocomplete popup and the `/help` modal. The HTTP endpoint (`GET /slash-commands`) and `dispatch_slash_command` fallback were already in HEAD; what was missing was the visual integration.
**Files touched**:
- `src/tui/app.rs` — load skill commands at TUI startup, pass to `CommandRegistry`
- `src/tui/ui/command_menu.rs` — add `MenuEntry` enum; show built-in + skill commands in autocomplete popup with `[skill]` badge
- `src/tui/ui/modal.rs` — pass `&CommandRegistry` to `render_help_body`; add "Skill Commands:" section in `/help`
**Tests added**:
- `modal.rs::render_help_lists_skill_commands_when_present` — verifies skill section in help output
**Notes**:
- `app.rs` previously called `CommandRegistry::default_set()` without loading skills, so skill commands were never actually available in the TUI despite the machinery existing. Fixed by loading skills from the workspace at `App::new()`.
- Goals 170 (true abort) and 171 (resume picker) were completed by the self-improve loop between sessions.
