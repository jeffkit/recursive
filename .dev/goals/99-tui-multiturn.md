# Goal 99 — TUI: Multi-turn Conversation View

**Roadmap**: Phase 11.3 — TUI (part 3/5)

**Design principle check**:
- Implemented as: UI enhancements in `crates/recursive-tui/src/main.rs`
- ❌ Does NOT modify core library
- Visual/UX only — no new API endpoints

## Why

The TUI shows messages linearly. For multi-turn conversations, users
need: scrolling (when messages exceed screen), visual separation between
turns, and a status bar showing session info (connected, session ID,
message count).

## Scope (do exactly this, no more)

### 1. Scrollable message history

- Track scroll offset in App state
- Up/Down arrow keys scroll the message view
- Auto-scroll to bottom when new messages arrive
- PageUp/PageDown for fast scrolling

### 2. Status bar

Add a third panel (1 line) between messages and input:

```
┌─ Messages ────────────────────────────────────────┐
│ ...                                                │
└────────────────────────────────────────────────────┘
 Connected | Session: abc123 | Messages: 5 | Scroll: 3/12
┌─ Input ───────────────────────────────────────────┐
│ █                                                  │
└────────────────────────────────────────────────────┘
```

### 3. Turn separators

Insert a visual separator line between user→agent turns:
```
You: hello
  🔧 read_file
  ✓ read_file
Agent: Here's what I found...
────────────────────────────
You: thanks, now write it
  🔧 write_file
  ✓ write_file
Agent: Done!
```

Use `StyledMessage::Separator` variant for this.

### 4. Input cursor indicator

Show a blinking cursor position or at minimum a `▌` character
at the end of the input text.

### 5. Tests

- Test: scroll_up/scroll_down adjusts offset correctly
- Test: scroll doesn't go below 0 or above max
- Test: auto-scroll on new message
- Test: status bar text contains session info when connected

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean
- TUI scrolls, shows status bar, has turn separators

## Notes for the agent

- Read `crates/recursive-tui/src/main.rs` for current App struct and ui().
- For scrolling, use `Paragraph::scroll((offset, 0))` in ratatui.
- Status bar: use a simple `Paragraph` with `Constraint::Length(1)` in the layout.
- Turn separator: add after each AssistantMessage in handle_ui_event.
- **DO NOT modify any file in `src/`.**
- **Keep changes to the TUI crate only.**
