# Manual edit: fix scrollback completeness + queued-turn spinner

**Date**: 2026-06-21
**Goal**: Fix TUI issue #2 (transcript scrollback was incomplete / couldn't
reliably reach old messages) and the cosmetic follow-up to issue #1 (a turn
drained from the type-ahead queue ran with the spinner stuck off).

**Files touched**:
- `src/tui/ui/transcript.rs` — new `wrap_lines_to_width()` helper (+ 5 unit tests).
- `src/tui/ui/chat.rs` — replace `Paragraph::scroll` + estimated row count with
  exact pre-wrap + `usize` windowing.
- `src/tui/events.rs` — new `UiEvent::TurnStarted` variant.
- `src/tui/app/event_loop.rs` — handle `TurnStarted` by re-arming the spinner
  (+ regression test).
- `src/tui/backend.rs` — emit `TurnStarted` before running each turn (fresh and
  queued).

**Tests added**:
- `tui::ui::transcript::tests::wrap_splits_long_line_into_width_bounded_rows`
- `tui::ui::transcript::tests::wrap_preserves_blank_lines_one_for_one`
- `tui::ui::transcript::tests::wrap_width_zero_is_noop`
- `tui::ui::transcript::tests::wrap_preserves_span_styles_across_break`
- `tui::ui::transcript::tests::wrap_handles_wide_chars_without_exceeding_width`
- `tui::app::event_loop::tests::turn_started_rearms_spinner_after_finish`

**Notes**:

Issue #2 root cause: the chat panel rendered the *entire* transcript into one
`Paragraph`, scrolled via `Paragraph::scroll((effective_scroll as u16, 0))`,
and computed `effective_scroll` from a row count it *estimated* with
`line_width.div_ceil(panel_width)`. That estimate assumes naive
character-width wrapping, but ratatui's `Wrap { trim: false }` wraps on word
boundaries and produces a different number of physical rows. The drift made
scroll positions inexact and could leave rows that were never reachable. The
`as u16` cast was an additional latent overflow on very long transcripts.

Fix: pre-wrap every logical line into physical rows at the exact panel width
with `wrap_lines_to_width()` (per-span style preserved, Unicode-width aware),
then window the rows ourselves in `usize` and render the slice with no further
wrapping or scroll offset. Windowing is exact, so both the top and bottom are
always reachable, and there is no `u16` truncation. `scroll_offset` keeps its
existing meaning (rows from the bottom; 0 = stuck to newest).

Queued-spinner fix: the backend now emits `UiEvent::TurnStarted` immediately
before spawning each turn task. Because queued type-ahead messages are fed back
through the same `SendMessage` handler, this covers them too. The UI handles
`TurnStarted` by calling `TurnState::start()` — idempotent for a freshly
submitted turn (already armed on submit), and essential for a queued turn whose
predecessor's `TurnFinished` had already cleared the running state.

**Quality gates**: `cargo fmt --all`, `cargo clippy --all-targets
--all-features -- -D warnings` (clean), `cargo test --features tui --lib tui::`
(256 passed), `cargo test --features tui --test tui_backend_smoke` (5 passed).
