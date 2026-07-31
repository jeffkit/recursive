# Goal 349 — Make Agent output in the TUI selectable & copyable

**Roadmap**: TUI UX — the transcript panel is currently read-only; add
mouse-drag text selection + keyboard yank so the user can copy the agent's
output without leaving the TUI.

**Design principle check**:
- Implemented as: additions to the `recursive-tui` crate only — new
  selection state on `App`, new mouse arms in `handle_mouse`, a render
  pass that reverse-highlights selected rows, and two key bindings in
  `App::handle_key`. A new `arboard` dev/runtime dependency provides the
  clipboard.
- ❌ Does NOT branch inside `src/run_core.rs::RunCore::run_inner`.
- ❌ Does NOT change the agent runtime, tool registry, providers, or any
  non-TUI crate.
- ❌ Does NOT change `UserAction` semantics for the backend (copy is a
  purely client-side action; no new `UserAction` variant is sent to the
  worker).

## Why

Today the entire transcript is painted as a single read-only ratatui
`Paragraph` (`crates/recursive-tui/src/ui/chat.rs:123-124`), and the mouse
handler (`crates/recursive-tui/src/lib.rs:223-233`) maps **only**
`ScrollUp`/`ScrollDown` onto `scroll_offset`, dropping every
`Down`/`Drag`/`Up` event via `_ => {}`. There is no clipboard crate in
`crates/recursive-tui/Cargo.toml`, no copy/yank key binding in
`App::handle_key` (`crates/recursive-tui/src/app/commands.rs:171-266`),
and no `Copy`/`Yank` variant in `UserAction`
(`crates/recursive-tui/src/events.rs:248`). Worse: because the TUI runs in
raw mode with `EnableMouseCapture` active (`lib.rs:162`), the terminal
emulator's own native drag-selection is partially suppressed — so even the
usual escape hatch (hold Shift/Option to select) is unreliable. The user
has no in-app way to grab the agent's output.

This goal adds two complementary copy paths:

1. **Mouse-drag selection** — press-drag-release over the transcript
   highlights a row range and copies it to the clipboard on release. This
   is the natural "select and copy" the user asked for.
2. **Keyboard yank** — `Ctrl+Y` copies the *last* assistant message whole;
   `Ctrl+Shift+Y` copies the entire visible window. Fast for the common
   "grab the whole answer" case without needing the mouse.

Selection is expressed in **physical-row coordinates of the currently
visible window** (the same coordinate space as `scroll_offset`), reusing
the exact row-window already computed in `chat.rs:108-119`. This keeps the
feature local to rendering and avoids any model/runtime change.

## Scope (do exactly this, no more)

### 1. Add the `arboard` clipboard dependency

In `crates/recursive-tui/Cargo.toml`, add to `[dependencies]`:

```toml
arboard = "3"
```

`arboard` is cross-platform (macOS/Linux/Windows), pure-Rust-ish, and the
standard choice for ratatui-style TUIs. Keep it a normal (non-optional)
dependency — clipboard is a core UX feature, not a feature-flag extra.

### 2. Selection + last-copied state on `App`

In `crates/recursive-tui/src/app/mod.rs`, the `App` struct (defined at
line 47) gains two fields:

```rust
/// Goal-349: active text selection over the visible transcript window,
/// as `(start_row, end_row)` inclusive physical-row indices relative to
/// the *visible* window (0 = top visible row). `None` when nothing is
/// selected. Cleared whenever `scroll_offset` changes (see below) so the
/// highlight never desyncs from the rows it was drawn against.
pub selection: Option<(usize, usize)>,
/// Goal-349: text of the most recent successful copy (mouse-release or
/// yank). Primary purpose: a testable mirror of the clipboard. In
/// headless/CI environments `arboard::Clipboard::new()` can fail (no
/// display server / sandbox), so every copy path writes the same text
/// here as a fallback that unit tests assert on.
pub last_copied: Option<String>,
```

Initialise both to `None` in the `App::new` constructor
(`crates/recursive-tui/src/app/state.rs` — wherever the other fields get
their defaults; mirror the existing field-init style).

**Clear-on-scroll invariant:** everywhere `scroll_offset` is mutated
(`app/commands.rs:206/210/214/218` for Shift+↑/↓ and PageUp/PageDown, and
`lib.rs:225-230` for the mouse wheel), set `app.selection = None`
immediately after. Reason: the selection indices are relative to the
visible window; scrolling moves the window under the highlight, so a stale
selection would point at the wrong rows. Clearing is the simplest correct
fix. (A future goal could translate indices on scroll; out of scope here.)

### 3. Mouse selection in `handle_mouse`

In `crates/recursive-tui/src/lib.rs`, rewrite `handle_mouse` (lines
223-233) to handle left-button selection in addition to the existing
scroll-wheel mapping. Keep `ScrollUp`/`ScrollDown` exactly as they are.

```rust
fn handle_mouse(app: &mut App, ev: MouseEvent) {
    match ev.kind {
        MouseEventKind::ScrollUp => {
            app.selection = None;                       // Goal-349
            app.scroll_offset = app.scroll_offset.saturating_add(3);
        }
        MouseEventKind::ScrollDown => {
            app.selection = None;                       // Goal-349
            app.scroll_offset = app.scroll_offset.saturating_sub(3);
        }
        MouseEventKind::Down(MouseButton::Left) => {
            // Begin a selection at the clicked visible row.
            app.selection = Some((ev.row as usize, ev.row as usize));
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            // Extend the selection to the dragged-to row. Only extends
            // an existing selection; a drag without a prior Down (e.g.
            // another button held) is ignored.
            if let Some((start, _)) = app.selection {
                let end = ev.row as usize;
                app.selection = Some((start.min(end), start.max(end)));
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            // Release: copy the selected rows' text and clear.
            if let Some((start, end)) = app.selection {
                copy_visible_rows(app, start, end);
                app.selection = None;
            }
        }
        _ => {}
    }
}
```

Note on a crossterm subtlety the agent should be aware of:
`MouseEventKind::Drag(MouseButton::Left)` **does** carry the button in
modern crossterm 0.28, but terminals only emit `Drag` after a `Down` on
the same button — so gating `Drag` on `app.selection` being `Some` (i.e.
a prior `Down` happened) is both correct and robust against stray drags
from other buttons. Do not try to track "which button is held" manually;
rely on the `Down`→`Drag`→`Up` pairing.

Add a helper that extracts the text from the visible window and writes it
to both the clipboard and `last_copied`:

```rust
/// Goal-349: copy the inclusive range `[start, end]` of *visible* rows
/// to the system clipboard and to `app.last_copied` (test mirror).
fn copy_visible_rows(app: &App, start: usize, end: usize) {
    // The chat renderer computes the visible window from `app.blocks`;
    // recompute the same flattened, width-wrapped physical rows and slice
    // the requested range. Factor the windowing out of `ui::chat::render`
    // (currently inline at chat.rs:108-119) into a small reusable
    // `pub fn visible_physical_rows(app, width) -> Vec<Line<'static>>`
    // in `ui::transcript` (or `ui::chat`) and call it from both places so
    // the selection text always matches what is painted.
    let rows = ui::chat::visible_physical_rows(&app, /* width from last render */);
    let lo = start.min(rows.len());
    let hi = (end + 1).min(rows.len());
    let text: String = rows[lo..hi]
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    copy_text(app, text);
}

/// Write `text` to the clipboard (best-effort) and always to
/// `app.last_copied`. The clipboard call may fail in a headless
/// environment; that must NOT abort the copy or surface an error to the
/// user — `last_copied` is the source of truth for tests.
fn copy_text(app: &mut App, text: String) {
    if let Ok(mut cb) = arboard::Clipboard::new() {
        let _ = cb.set_text(text.clone());
    }
    app.last_copied = Some(text);
}
```

**Refactor requirement:** the row-windowing math (`transcript::render_blocks`
→ `transcript::wrap_lines_to_width` → slice by `scroll_offset`) is
currently duplicated logic that only lives inside `ui::chat::render`
(chat.rs:76-119). Extract a `pub fn visible_physical_rows(app: &App,
width: u16) -> Vec<Line<'static>>` (returning the same `window` slice
chat.rs:119 produces) so `handle_mouse` and the render path share one
source of truth. The render path must call the same fn — do not leave two
copies of the windowing math. Keep `handle_mouse`'s need for the *last
rendered* width: store the last render width on `App` (e.g.
`last_render_width: u16`, default 80) updated at the top of
`ui::chat::render`, so `handle_mouse` can call `visible_physical_rows`
without re-deriving the width.

### 4. Render the selection highlight

In `crates/recursive-tui/src/ui/chat.rs`, after building `window`
(chat.rs:119), if `app.selection` is `Some((s, e))`, restyle the rows in
that inclusive range to reversed video before wrapping them in the
`Paragraph`. Concretely: map each `Line` in the window through

```rust
if let Some((s, e)) = app.selection {
    let lo = s.min(window.len());
    let hi = (e + 1).min(window.len());
    for line in &mut window[lo..hi] {
        for span in &mut line.spans {
            span.style = span.style.add_modifier(Modifier::REVERSED);
        }
    }
}
```

(`Modifier::REVERSED` is the idiomatic ratatui way to invert fg/bg
regardless of the row's existing colour — important because assistant
markdown rows carry per-span syntax colours.) The selection is already in
*visible-window* coordinates, which exactly matches `window`'s indexing,
so no coordinate translation is needed at render time.

### 5. Keyboard yank in `App::handle_key`

In `crates/recursive-tui/src/app/commands.rs`, inside the chat-screen
`match key.code` block (lines 171-266), add two arms (place them near the
scroll keys, before the generic `KeyCode::Char(c)` catch-all at line 229
so the modifiers check wins over plain-char input):

```rust
// Goal-349: Ctrl+Y — yank the last assistant message to the clipboard.
KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL)
    && !key.modifiers.contains(KeyModifiers::SHIFT) =>
{
    if let Some(TranscriptBlock::Assistant { text, .. }) =
        app.blocks.iter().rev().find(|b| matches!(b, TranscriptBlock::Assistant { .. }))
    {
        crate::app::commands::copy_text(app, text.clone()); // or the lib.rs helper, factored to a shared location
    }
    None
}
// Goal-349: Ctrl+Shift+Y — yank the entire visible window.
KeyCode::Char('Y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
    // Capital 'Y' = Shift+y; crossterm delivers Shift as the char case
    // for letter keys when no other modifier interferes. Assert in a
    // test that Ctrl+Shift+Y (KeyEvent Char('Y') + CONTROL) lands here
    // and NOT in the plain Ctrl+Y arm above.
    let rows = crate::ui::chat::visible_physical_rows(&app, app.last_render_width);
    let text = rows.iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    crate::app::commands::copy_text(app, text); // same shared helper
    None
}
```

Note: factor the `copy_text` helper from step 3 into a location importable
from both `lib.rs` (mouse path) and `app/commands.rs` (key path) — e.g. a
free `pub fn copy_text(app: &mut App, text: String)` on `App` in
`app/state.rs` or a small `app/clipboard.rs`. Do not duplicate the
clipboard logic in two places.

The yank arms return `None` (no `UserAction` sent to the backend) — copy
is purely client-side, so the worker is never involved.

### 6. Tests (follow `.dev/skills/tui-acceptance.md` — all three layers)

Write tests at the **rendered layer first**, using the in-process
`Harness` (`crates/recursive-tui/src/harness.rs`):
`Harness::new()/with_size()`, `pump(UiEvent)`, `type_key/ctrl/enter`,
`render() -> Screen`, and `Screen::row_has_bg_color /
row_has_bg_other_than / find_row / text / numbered`.

Required unit tests (in the module that owns each behaviour, under
`#[cfg(test)] mod tests`):

- `mouse_down_drag_selects_row_range` — pump an `AssistantMessage`, drive
  a `Down`→`Drag` sequence (construct `MouseEvent`s directly or via a
  small test helper), then `render()` and assert the dragged rows are
  reversed: `screen.row_has_bg_color(y, <reversed-indicator>)` or, since
  REVERSED flips existing bg, assert via the style modifier on the cells
  (`screen.style(x,y).add_modifier` contains `REVERSED`). The
  tui-acceptance doc warns against "any bg" checks — assert on the
  specific `REVERSED` modifier per cell, which is unambiguous.
- `mouse_up_copies_selection_and_clears` — after the Down/Drag from the
  previous test, emit `Up(MouseButton::Left)`, then assert
  `app.last_copied` is `Some(...)` containing the dragged rows' text and
  `app.selection` is `None`.
- `scroll_clears_selection` — set `app.selection = Some((0,0))`, call the
  Shift+Up key handler (`h.type_key(<shift+up>)`), assert
  `app.selection == None` and `scroll_offset` increased (pins the
  clear-on-scroll invariant).
- `ctrl_y_copies_last_assistant_message` — pump a User then Assistant
  block, `h.ctrl('y')`, assert `app.last_copied` equals the assistant
  text exactly.
- `ctrl_shift_y_copies_visible_window` — pump enough blocks to fill the
  window, `h.type_key(KeyEvent::new(Char('Y'), CONTROL|SHIFT))` (or the
  exact crossterm shape — verify with a `type_key`-level test), assert
  `app.last_copied` contains the visible rows and is NOT the whole
  transcript (window is smaller than the full transcript).
- `ctrl_y_without_assistant_block_is_noop` — no assistant block yet,
  `h.ctrl('y')`, assert `app.last_copied` stays `None` (no panic).
- `clipboard_failure_falls_back_to_last_copied` — this documents the
  headless behaviour: `arboard::Clipboard::new()` may return Err in CI;
  the test asserts that `copy_text` still sets `last_copied` even when
  the clipboard call fails. (You can simulate by asserting
  `last_copied.is_some()` after any copy path — the clipboard itself is
  not assertable in a sandbox.)

Also add a `visible_physical_rows` unit test asserting it returns exactly
the `window` slice that `render()` paints (same length, same first/last
row text) — this pins the step-3 refactor so selection and paint can't
diverge.

Then run the gates:

```bash
.dev/scripts/tui-test-presence.sh   # hard gate: confirms tests were added
.dev/scripts/tui-mutants.sh         # hard gate: no survivors in touched files
```

If `tui-mutants.sh` reports survivors in `lib.rs` terminal-IO code
(raw-mode / mouse / alternate-screen setup), that is **expected** per
tui-acceptance.md §3 (that layer is covered by the PTY tour, not the
in-process harness) — document each such survivor in the journal rather
than chasing it.

### 7. PTY tour (step 4 of tui-acceptance)

```bash
cargo build -p recursive-tui
cargo run -q -p tui-pty-harness -- run \
  --bin "$PWD/target/debug/recursive-tui" \
  --keys "hello\r" --wait-ms 2500 --snap numbered
```

Confirm the assistant reply renders, then (manually, since drag is hard to
script over PTY) verify in a real terminal that mouse-drag highlights
rows and release copies. The PTY tour's automated assertion is just
"transcript still renders correctly after the refactor" (regression
guard).

### 8. Journal

`.dev/journal/manual-<YYYYMMDD>-tui-select-copy.md` — note:
- the prior state (read-only `Paragraph`, mouse capture suppressed native
  selection),
- the two copy paths added (drag-select + Ctrl(+Shift)+Y),
- the `visible_physical_rows` refactor and why selection lives in
  visible-window coordinates,
- the `last_copied` test mirror and why `arboard` can't be asserted
  directly in CI,
- any `tui-mutants` survivors in `lib.rs` terminal-IO code (expected).

## Acceptance

- `cargo test -p recursive-tui` green (including the new tests above).
- `cargo clippy -p recursive-tui --all-targets -- -D warnings` clean.
- `cargo fmt --all --check` clean.
- `.dev/scripts/tui-test-presence.sh` exits 0.
- `.dev/scripts/tui-mutants.sh` exits 0 (survivors only in `lib.rs`
  terminal-IO code, documented in the journal, are acceptable).
- The headline tests `mouse_down_drag_selects_row_range`,
  `mouse_up_copies_selection_and_clears`, and `ctrl_y_copies_last_assistant_message`
  all pass.
- No change outside `crates/recursive-tui/` (no runtime/kernel/tool/provider
  edits; `UserAction` unchanged).
- Existing scroll behaviour (`Shift+↑/↓`, `PageUp/PageDown`, mouse wheel)
  unchanged in effect — only gains the clear-on-selection side effect.

## Notes for the agent

- **This is a TUI-only feature.** Everything happens in
  `crates/recursive-tui/`. Do not touch `src/run_core.rs`, the runtime,
  tools, providers, or `UserAction`. Copy is client-side; the worker is
  not involved (yank/mouse-copy handlers return `None`).
- **Follow `.dev/skills/tui-acceptance.md` to the letter** — it is loaded
  for any goal touching `crates/recursive-tui/`. Rendered-layer tests
  first (via `Harness`), then `tui-test-presence.sh`, then
  `tui-mutants.sh` as a hard gate, then a PTY tour.
- **`arboard` in CI:** `arboard::Clipboard::new()` can fail without a
  display server. That is exactly why `last_copied` exists — assert on
  it, never on the live clipboard. The clipboard call must be
  best-effort (swallow the `Err`); a failed clipboard must not change
  control flow or surface an error.
- **Selection coordinate space:** selection indices are relative to the
  *visible window* (the same space as the `window: Vec<Line>` slice at
  chat.rs:119), NOT relative to the full transcript. This is what makes
  the render highlight a trivial index into `window`. The cost is the
  clear-on-scroll invariant (step 2) — accept that cost; translating
  indices across scrolls is a separate, larger change.
- **Modifier precedence in the key match:** put the `Ctrl+Shift+Y` arm
  (capital `'Y'`) and the `Ctrl+Y` arm (lowercase `'y'`, with an explicit
  `!SHIFT` guard) BEFORE the generic `KeyCode::Char(c)` arm at
  commands.rs:229, or they'll never fire. crossterm encodes Shift on a
  letter as the capital letter, so `Ctrl+Shift+y` arrives as
  `Char('Y') + CONTROL`. A test must pin this (the
  `ctrl_shift_y_copies_visible_window` test).
- **Modifier::REVERSED, not a hard-coded colour.** Assistant rows carry
  per-span syntax-highlight colours from the markdown renderer; painting
  a fixed bg would fight those. REVERSED inverts whatever is there.
- **Known expected `tui-mutants` survivors:** the raw-mode / mouse-capture
  / alternate-screen setup in `lib.rs` (lines ~160-165) is terminal-IO
  code that the in-process harness can't reach. Per tui-acceptance.md §3,
  survivors there are expected and covered by the PTY tour — document
  them, don't chase them in-process.
- **`git` discipline:** this goal lands in the `recursive` sub-repo
  (`git -C recursive ...`), not the infra4agent monorepo root. Commit in
  the sub-repo. macOS `launch-flow.sh` runs the flow under tmux to avoid
  App Nap reclaiming the long-running Node process.
