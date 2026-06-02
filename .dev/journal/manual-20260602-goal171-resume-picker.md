# Manual edit: goal171-resume-picker

**Date**: 2026-06-02
**Goal**: Implement /resume command and ResumePicker modal (Goal-171)
**Files touched**:
- src/tui/events.rs — added UiEvent::SessionResumed, UserAction::ResumeSession
- src/tui/ui/modal.rs — added ResumeEntry, Modal::ResumePicker, render_resume_picker_body, load_recent_sessions
- src/tui/commands.rs — added /resume command (alias r), cmd_resume(), removed Eq derive from CommandOutcome (f64 cascade)
- src/tui/app.rs — added workspace_path field, SessionResumed handler, handle_resume_picker_key()
- src/tui/backend.rs — added UserAction::ResumeSession handler (load_messages + set_transcript)

**Tests added**: updated registry_includes_all_thirteen_commands (count 12→13)
**Notes**:
- SessionCost has no total_usd field — cost_usd hardcoded to 0.0 for now
- f64 in ResumeEntry forced removal of Eq derive from ResumeEntry, Modal, CommandOutcome
- truncate_transcript(len) semantics: keeps first N messages (pre-turn restore), NOT last N
