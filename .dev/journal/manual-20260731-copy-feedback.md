# Manual fix: copy feedback + upward-drag selection (Goal 349 follow-up)

Date: 2026-07-31
Goal: Make the TUI select-and-copy feature discoverable and fix upward drag

## User report

1. Selecting text with the trackpad works (top→bottom), but there was no way
   to know how to copy — expected: release after selecting = copy, no extra key.
2. Selecting bottom→top failed: the selection collapsed to a single row that
   followed the pointer.

## Root causes

- **No feedback on copy.** Goal-349 already copies on `MouseEventKind::Up(Left)`
  (release-copy — no extra key needed), but nothing told the user it happened.
  `copy_text` wrote to `last_copied` (test mirror) only.
- **Upward-drag anchor bug.** `handle_mouse`'s `Drag(Left)` arm re-derived the
  anchor from the stored (already normalised) range:
  `let (start, _) = app.selection; selection = (start.min(end), start.max(end))`.
  Downward drags worked because the anchor stayed the first element. After the
  first upward drag the stored pair became `(cursor, anchor)`, so the next
  drag read the *previous cursor row* as the anchor and dropped every row
  between the click point and that position — the highlight collapsed to a
  single row following the pointer.

## Fix

- **Selection semantics: `(anchor, cursor)`.** `Down(Left)` sets
  `(row, row)`; `Drag(Left)` keeps the first element fixed and only moves the
  cursor (`selection = (anchor, ev.row)`). Consumers normalise with min/max:
  the renderer (`ui/chat.rs`) and the copy path (`copy_visible_rows`) both
  compute `(min, max)` before use, so upward and downward drags behave
  identically.
- **Copy notice.** `App::copy_notice: Option<(usize, Instant)>` records
  `(char_count, copied_at)` in `copy_text` (chars, not bytes — CJK-safe).
  `ui/status.rs` renders `copied N chars` as a bold green segment for ~3
  seconds, then it ages out. All copy paths (mouse release, Ctrl+Y,
  Ctrl+Shift+Y) route through `copy_text`, so all of them get the notice.

## Files touched

- `crates/recursive-tui/src/lib.rs` — `handle_mouse` Down/Drag arms (anchor
  semantics), `copy_visible_rows` normalises the range; doc comment updated.
- `crates/recursive-tui/src/ui/chat.rs` — selection highlight normalises
  (anchor, cursor) → inclusive range.
- `crates/recursive-tui/src/app/mod.rs` — new `copy_notice` field.
- `crates/recursive-tui/src/app/state.rs` — init `copy_notice`, stamp it in
  `copy_text`.
- `crates/recursive-tui/src/ui/status.rs` — transient `copied N chars`
  segment (3s, bold green).

## Tests added

- `lib.rs::mouse_upward_drag_keeps_anchor_and_selects_full_range` — Down(4) →
  Drag(2) → Drag(1) keeps anchor 4; rows 1..=4 highlighted; release copies
  rows 1..=4 (fails on the old collapse bug: only rows 1..=2 would be
  selected).
- `status.rs::status_bar_shows_copy_notice_with_char_count` —
  `copy_text("hello world")` → "copied 11 chars" with a separator.
- `status.rs::status_bar_hides_copy_notice_after_expiry` — aged past 3s → gone.
- `status.rs::status_bar_no_copy_notice_before_any_copy` — no copy → no segment.
- `state.rs::copy_text_records_char_count_for_notice` — "héllo 你好" = 9 chars
  (11 bytes) proves the count is chars, not bytes.

## Verification

- `cargo test -p recursive-tui --lib` — pass
- `cargo clippy -p recursive-tui --all-targets --all-features -- -D warnings` — clean
- `cargo fmt --all` — clean
- `.dev/scripts/tui-test-presence.sh` — PASS (test-bearing change)
- `.dev/scripts/tui-mutants.sh` — PASS (no survivors in the diff)
