# Manual edit: tui-session-persistence

**Date**: 2026-06-03
**Goal**: TUI interactive sessions are now persisted to .recursive/sessions/ so that the startup banner can show real recent chats instead of nothing (or self-improve goal runs).
**Files touched**:
- src/tui/backend.rs — worker_loop creates a SessionWriter on the first SendMessage, appends new transcript messages after each turn, and finalises the session on Shutdown.
- src/tui/mod.rs — print_startup_banner now reverses the session list (newest first) and uses `.chars().count() > 60` for correct ellipsis detection on CJK text.
**Tests added**: none (end-to-end TUI behaviour)
**Notes**:
- SessionWriter.append() automatically calls bump_updated_at() for user/assistant messages, so last_prompt is set as soon as the first user message is appended. No separate meta flush needed.
- The session goal is set to the user's first message (capped at 200 chars) since there is no separate "goal" in interactive mode.
- saved_transcript_len skips the initial system-prompt messages that exist before the user's first message; those are not re-saved on each turn.
