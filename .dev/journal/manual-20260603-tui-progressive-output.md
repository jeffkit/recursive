# Manual edit: TUI progressive output + compact startup banner

**Date**: 2026-06-03
**Goal**: Replace the full-screen TUI with true progressive output (messages scroll into terminal native scrollback), and add a compact fake-cc-style startup banner with logo, version, model, and recent sessions.
**Files touched**:
- `src/tui/mod.rs` — INLINE_HEIGHT 10, new print_startup_banner(), insert_before() drain loop
- `src/tui/app/mod.rs` — added `last_printed_idx` and `print_queue` fields
- `src/tui/app/state.rs` — initialized new fields in App::new()
- `src/tui/app/event_loop.rs` — added flush_ready_blocks() method
- `src/tui/ui/chat.rs` — render only in-flight blocks (index >= last_printed_idx)
**Tests added**: none (UI-only change)
**Notes**:
- Reasoning blocks are deferred until the following Assistant block is finalized so they appear together in scrollback
- Sessions shown are filtered to last_prompt-only entries (user-initiated), max 3
- Startup banner styled after fake-cc: 3-line Unicode box-drawing logo + version·model + dotted separator + recent sessions
- Clippy fix: match → matches! macro in flush_ready_blocks
