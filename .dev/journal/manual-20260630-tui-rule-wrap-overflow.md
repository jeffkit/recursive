# Manual edit: tui-rule-wrap-overflow

**Date**: 2026-06-30
**Goal**: Fix the TUI markdown horizontal-rule (`---`) wrapping a stray `─`
onto the next row after every separator.
**Files touched**:
- `crates/recursive-tui/src/ui/transcript.rs` — `render_assistant` now passes
  `width.saturating_sub(2)` to `render_markdown`, reserving the 2-column
  indent it prepends to every emitted line.
- `crates/recursive-tui/src/ui/markdown.rs` — `TagEnd::Table` no longer
  double-subtracts the indent (`table_max = width` instead of
  `width.saturating_sub(2)`); the indent reservation now lives with the
  caller. Added regression test
  `horizontal_rule_fits_panel_with_two_space_indent`.
**Tests added**:
- `horizontal_rule_fits_panel_with_two_space_indent` — simulates
  `render_assistant`'s 2-space indent + `wrap_lines_to_width` and asserts the
  rule occupies exactly one physical row (no stray `─` wrap).
**Notes**:
- Root cause: `Event::Rule` built the rule to the full `wrap_width`, but
  `render_assistant` prepends a 2-space indent to every markdown line, then
  `chat.rs` hard-wraps at `messages_area.width`. So the rule line was
  `width + 2` columns and overflowed by 2, wrapping trailing `─` chars.
  Tables already reserved the indent via their own `saturating_sub(2)`; the
  rule did not.
- The user's hypothesis ("a `-` at paragraph start挤占空间") was close: it
  IS a per-line prefix stealing columns, but it's the 2-space `indent`
  (`Span::raw("  ")`), not a `-` bullet (bullets are `•` U+2022).
- Pre-existing clippy lints in `skill_commands.rs:672` (`unwrap_used`,
  `needless_borrow`) are unrelated to this change and fail on `main` too.
