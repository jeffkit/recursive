# Manual edit: TUI experience fixes — border, reasoning order, tool colours, scroll hint

**Date**: 2026-06-03
**Goal**: Four TUI polish fixes reported from the user's screenshot:
1. Drop the bordered "Messages" panel and the "Welcome to Recursive TUI" system block
2. Surface thinking / reasoning ABOVE the matching assistant text instead of below it
3. Unify tool-call text colour (args, ⎿ prefix, "Running…", size, "more lines" hint,
   output) — the dim `system_bar` palette made the tool block almost unreadable
4. Remove the stale `ctrl+b/f or wheel scroll` segment from the input footer hint
   (Ctrl+B / Ctrl+F are now emacs readline cursor motion, not transcript scroll)

## Files touched

- `src/tui/ui/chat.rs` — dropped the `Block` wrapper on the messages
  `Paragraph`; `inner_width` / `visible_rows` now use the chunk
  directly (no 2-char / 2-row border subtraction)
- `src/tui/app/state.rs` — `App::new()` starts with an empty
  transcript (welcome `System` block removed); two construction
  tests updated; cleaned up the now-unused `TranscriptBlock`
  import in the test module
- `src/tui/ui/transcript.rs` — `render_tool_call` now uses
  `body_color` for the args, ⎿ prefix, "Running…", byte size,
  "more lines" hint, and the actual output (the bullet ⏺ and
  tool name keep their state colour); `cargo fmt --all` did the
  usual cosmetic cleanup
- `src/run_core.rs` — per-step completion now emits
  `AgentEvent::Reasoning` BEFORE `AgentEvent::AssistantText`, so
  the TUI's `Reasoning { text }` block lands above the matching
  `Assistant { text }` block (model thinks first, then speaks)
- `src/tui/ui/input.rs` — `footer_hint` no longer mentions
  `ctrl+b/f or wheel scroll`; the four affected modes (Prompt,
  Bash, Note, Command) drop the segment; the other two modes
  (AtFile, HistorySearch) were untouched

## Verification

- `cargo test --workspace` — 1099 + smaller crate test runs, all
  green
- `cargo clippy --all-targets --all-features -- -D warnings` —
  clean
- `cargo fmt --all -- --check` — clean
- 14 transcript tests + 243 tui tests still pass after the
  colour unification (no assertion was checking the exact
  `system_bar` colour, so no test churn was needed)

## Trackpad / wheel scroll note

The user reported the message list no longer scrolls with the
trackpad. The mouse-capture path
(`src/tui/mod.rs::handle_mouse`) is wired and matches
`MouseEventKind::ScrollUp` / `ScrollDown` to `app.scroll_offset`
exactly as fixed in commit ad6cf7e. The chat layout's scroll
math (`chat.rs` lines around the `effective_scroll` computation)
is consistent — `max_scroll = total_rows - visible_rows` and
`effective_scroll = max_scroll - scroll_offset` (clamped), with
`visible_rows` now using the full chunk height (no border). On
this build the trackpad should drive scrolling 3 rows per tick
as designed. If the user still sees a regression after pulling
this branch, the next place to look would be the terminal —
some iTerm2 / WezTerm builds intercept trackpad events at a
higher layer; a quick `printf 'test'`-style check is to
launch the TUI under `script(1)` and confirm the terminal is
forwarding wheel events to crossterm at all.
