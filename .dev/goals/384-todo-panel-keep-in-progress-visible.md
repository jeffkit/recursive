# Goal 384 — Todo panel keeps the in-progress task visible

**Roadmap**: TUI — usability. The task-list panel (rendered above the
input box) is hardcoded to show only the first 6 items: it takes
`.take(6)` and caps height at `.min(6)`. When a session has more than 6
todos and the in-progress task has moved past item 6, the user cannot see
what the agent is currently doing — the yellow `◉` row is silently
truncated off-screen. This is the single most common "where did the agent
go?" confusion in long sessions.

**Design principle check**:
- Implemented as: replace the fixed `.take(6)` slice in `render_todo_panel`
  with a computed window that guarantees the in-progress item is visible,
  using `Paragraph::scroll((offset, 0))` (the same primitive the input box
  uses to keep the cursor row visible). Lift the height cap so the panel
  can grow with the list up to a fraction of the screen. Pure render-layer
  change + one event-loop hook to (optionally) recompute on update.
- ❌ Does NOT touch `src/run_core.rs::run_inner` — this is TUI-only.
- ❌ Does NOT change the `todo_write` tool, `TodoItem`/`TodoStatus` structs,
  or the `TodoUpdated` event contract. The data model is correct; only the
  rendering window is wrong.

## Why (verified 2026-08-04 by reading the code)

1. **`crates/recursive-tui/src/ui/chat.rs:303-306` — the render takes only
   the first 6 items.** `render_todo_panel` builds its `Vec<Line>` with
   `.iter().take(6)`. Items 7+ (including a possible in-progress task) are
   never rendered. The comment at lines 292-293 admits this: *"Items beyond
   the first 6 are silently truncated (the agent should keep lists short)."*
   In practice agents regularly produce 8-12 item lists.

2. **`crates/recursive-tui/src/ui/chat.rs:28-36` — the panel height is
   capped at 6 items.** `todo_panel_height` returns
   `(app.current_todos.len().min(6) as u16) + 2`. So even if the render
   showed more, the allocated area couldn't fit them. The cap is hardcoded,
   not derived from screen size.

3. **`crates/recursive-tui/src/ui/chat.rs:294` — `render_todo_panel` takes
   `&App` (read-only) and has no scroll state.** It creates a `Paragraph`
   with no `.scroll(...)` call. There is no `todo_scroll_offset` field on
   `App` (confirmed: `app/mod.rs` has only `scroll_offset` for the message
   transcript, nothing for todos). So there is no way for the user to
   scroll, and no auto-follow logic.

4. **`crates/recursive-tui/src/app/event_loop.rs:271-273` — `TodoUpdated`
   replaces the vec but does not reposition any view.** Unlike message
   pushes (which call `scroll_to_bottom`), a todo update just does
   `self.current_todos = todos;` with no scroll/focus adjustment.

5. **The in-progress task is a reliable scroll target.** The `todo_write`
   tool enforces at most one `InProgress` item (`src/tools/todo.rs:147-159`,
   tested at `todo.rs:247-258`). So
   `current_todos.iter().position(|t| t.status == TodoStatus::InProgress)`
   yields a single deterministic index (or `None`) — the exact row that must
   stay visible.

6. **A directly reusable pattern exists: the input box cursor-follow.**
   `crates/recursive-tui/src/ui/input.rs:251-253` computes
   `scroll_y = cursor_row.saturating_sub(visible_rows - 1).min(cursor_row)`
   and passes it to `Paragraph::new(...).scroll((scroll_y, 0))`. This is the
   identical problem (keep a specific row visible in a fixed-height
   `Paragraph`) and the identical solution primitive.

## Scope (do exactly this, no more)

### 1. `crates/recursive-tui/src/ui/chat.rs` — window the todo render around the in-progress item

Replace the `.take(6)` slice in `render_todo_panel` (lines 303-324) with a
windowing slice that guarantees the in-progress item is visible:

```rust
// Content rows available inside the bordered panel (area.height - 2 for border).
let content_rows = area.height.saturating_sub(2) as usize;
let total = app.current_todos.len();

// Find the in-progress item — the row that MUST stay visible.
let anchor = app
    .current_todos
    .iter()
    .position(|t| t.status == TodoStatus::InProgress);

// Compute the window [start, start+content_rows) that contains the anchor.
// If no in-progress item (all pending or all done), show the tail (most
// recent activity), mirroring how the transcript pins to the bottom.
let start = match anchor {
    Some(idx) if total > content_rows => {
        // Center the anchor in the window, clamped to valid bounds so we
        // don't scroll past the start or end of the list.
        let ideal_start = idx.saturating_sub(content_rows / 2);
        ideal_start.min(total.saturating_sub(content_rows))
    }
    Some(_) => 0, // list fits entirely, no scroll needed
    None => total.saturating_sub(content_rows), // no anchor → pin to tail
};

let end = (start + content_rows).min(total);
let items: Vec<Line> = app.current_todos[start..end]
    .iter()
    .map(|item| { /* existing icon/style/label match — unchanged */ ... })
    .collect();

let widget = Paragraph::new(items)
    .block(Block::default().borders(Borders::ALL).title(title))
    .wrap(Wrap { trim: true });
// NOTE: no .scroll() needed — the slice [start..end] IS the visible window.
frame.render_widget(widget, area);
```

Key points:
- **Slice, not `.scroll()`**: since we render only the windowed items into
  the `Paragraph`, the `Paragraph` itself starts at item `start`. This is
  simpler than rendering all items and scrolling, and avoids ratatui
  computing wrap-heights for off-screen lines. (The input-box pattern uses
  `.scroll()` because it must also track a visible cursor; we don't.)
- **Centering**: `idx.saturating_sub(content_rows / 2)` puts the in-progress
  task roughly in the middle of the panel, so the user sees context above
  and below it. Clamp with `.min(total - content_rows)` so the window
  doesn't overshoot the list end.
- **No anchor → tail**: when there's no in-progress item (all done, or all
  pending before the agent starts), show the end of the list (most recent
  activity). This matches user expectation and the transcript's
  scroll-to-bottom behaviour.

### 2. `crates/recursive-tui/src/ui/chat.rs` — lift the height cap to be screen-relative

Change `todo_panel_height` (lines 28-36) so the panel can grow beyond 6 when
the list is long and the screen has room, but never dominates the screen:

```rust
fn todo_panel_height(app: &App, screen_height: u16) -> u16 {
    if app.current_todos.is_empty() {
        return 0;
    }
    // Cap at ~1/3 of the screen so the transcript keeps the majority of
    // vertical space. Keep at least the old 6-item default for short lists.
    let by_items = app.current_todos.len() as u16;
    let max_by_screen = screen_height.saturating_sub(8).max(3) / 3; // leave room for transcript+input+status
    let cap = max_by_screen.max(6); // never smaller than the old default
    by_items.min(cap) + 2 // +2 for the border
}
```

This requires passing `screen_height` (=`frame.area().height`) into
`todo_panel_height`. The caller is `render` at the top of `chat.rs` where
`frame` is available — pass `frame.area().height`. Update the call site.

Rationale for `max(6)`: the old behaviour showed 6; we don't want to shrink
the panel for lists that already fit. For long lists the panel grows up to
1/3 of the screen, giving the windowing logic room to show context around
the in-progress task.

### 3. Optional (only if cheap): truncation indicator

If the window excludes items (start > 0 or end < total), add a subtle
indicator in the panel title, e.g. ` Tasks (5/9 done) ↑2 ↓3 ` where `↑2`
means 2 items scrolled off the top and `↓3` means 3 off the bottom. This
tells the user the list is longer than the window. Skip if it adds more
than a few lines of fiddly formatting — the core fix is the windowing.

### 4. Tests

Update the existing test and add new ones in `chat.rs`'s `#[cfg(test)]`
module:

- **Update** `todo_panel_height_grows_with_items_and_caps_at_six` (line 486):
  the cap is now screen-relative, so pass a screen height and assert the new
  behaviour (e.g. with a tall screen, 9 items → height 11; with a short
  screen, the cap kicks in). Rename to reflect the new semantics if needed.
- **Add** `render_todo_panel_shows_in_progress_when_beyond_viewport`: build
  an `App` with 9 todos where item index 7 is `InProgress`. Render to a test
  buffer (use the existing test-render helper — search `TestBackend`/`Buffer`
  usage in this file) with a panel area tall enough for ~5 rows. Assert the
  in-progress item's text (or its `◉` icon) appears in the rendered output,
  i.e. it was NOT truncated. (If direct buffer assertion is hard, factor the
  window computation into a pure `fn todo_window(total, anchor, content_rows)
  -> (usize, usize)` and unit-test THAT — it's the logic that matters.)
- **Add** `todo_window_centers_anchor`: test the pure window helper —
  `todo_window(total=9, anchor=7, content_rows=4)` should return a window
  containing index 7 (e.g. `(5, 9)` or similar). Test edge cases: anchor at
  0, anchor at last index, total ≤ content_rows (no scroll).

Extract the window math into a small pure function `todo_window(...)` so it's
unit-testable independently of ratatui rendering — this is the goal's
headline logic and MUST have a named test.

## Files NOT to touch

- `src/tools/todo.rs` — the tool and data model are correct.
- `src/run_core.rs`, `src/kernel.rs`, `src/runtime.rs` — kernel invariants.
- `crates/recursive-tui/src/backend.rs` — the `TodoUpdated` event bridge is
  fine; the fix is in rendering, not in the data path.
- `.dev/flows/`, `.dev/scripts/`, `.flowcast/` — supervisor infrastructure.
- `tests/invariants/**` — must stay green.

## Acceptance

- `cargo build --workspace` green.
- `cargo test --workspace` green.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` clean.
- TUI gates: `sh .dev/scripts/tui-test-presence.sh` green; `cargo test -p recursive-tui` green.
- Grep: `rg '\.take\(6\)' crates/recursive-tui/src/ui/chat.rs` — **empty**
  (the hardcoded truncation is gone).
- Grep: `rg '\.min\(6\)' crates/recursive-tui/src/ui/chat.rs` — **empty**
  (the hardcoded height cap is gone, replaced by screen-relative logic).
- Grep: `rg 'fn todo_window' crates/recursive-tui/src/ui/chat.rs` — present
  (the pure window helper exists and is unit-tested).
- Headline tests by name:
  `cargo test -p recursive-tui todo_window` — the window-helper tests green.
  `cargo test -p recursive-tui todo_panel_height` — the updated height test green.

## Notes for the agent (traps)

- **The in-progress item is at most one.** The `todo_write` tool rejects a
  list with two `InProgress` items (`todo.rs:147-159`). So
  `.position(|t| t.status == InProgress)` is deterministic — don't write
  logic for "multiple in-progress" items; it can't happen. If it's `None`
  (no in-progress), pin to the tail.
- **Use slicing, not `Paragraph::scroll`.** Rendering `items[start..end]`
  into the `Paragraph` is simpler and correct. `.scroll()` is for when you
  must also render an off-screen cursor (like the input box does); the todo
  panel has no cursor. Rendering only the visible slice also avoids ratatui
  computing wrap heights for lines the user never sees.
- **`area.height` includes the border.** The `Block::default().borders(ALL)`
  consumes 2 rows (top + bottom). Content rows = `area.height - 2`. Get this
  wrong and the last item will be clipped by the bottom border. The existing
  code's `+ 2` in `todo_panel_height` is the matching inverse.
- **`wrap(Wrap { trim: true })` means a long item can occupy 2+ rows.** The
  window math counts items, not wrapped rows. For the first cut, assume one
  row per item (the common case — todos are short). If a long todo wraps and
  pushes the in-progress item out, that's an acceptable known limitation;
  note it in the journal rather than over-engineering wrapped-row math.
- **Don't add user-scroll controls (arrow keys / mouse) in this goal.** The
  auto-follow (keep in-progress visible) is the requested fix and is
  stateless (computed each render). Manual scroll would require a new
  `App` field + key/mouse handlers + state management — that's a separate
  enhancement. Keep this goal focused on auto-follow.
- **cargo-fmt + clippy are enforced gates** — run both before finishing.
- **Journal**: write `.dev/journal/manual-20260804-goal384-todo-panel-auto-follow.md`
  with Date / Goal / Files touched / Tests added / Notes (especially the
  centering math and the no-manual-scroll decision).
