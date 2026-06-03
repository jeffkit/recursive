# Manual edit: tui-emacs-cursor-motion-and-newline

**Date**: 2026-06-03
**Goal**: macOS Terminal users (the project's primary audience) get
the readline / emacs keybindings they expect from the prompt input
box. Three things were off:

1. Ctrl+B / Ctrl+F were page-scrolling the transcript, not moving
   the cursor. Modern terminals (iTerm2, Terminal.app, WezTerm,
   Alacritty, kitty) all deliver `PageUp` / `PageDown` reliably now,
   so the original "terminal-independent scroll fallback" is no
   longer worth the cost of breaking emacs muscle memory.
2. Ctrl+N / Ctrl+P had no binding at all — the keypress fell
   through to the character handler and inserted literal `n` / `p`.
3. Shift+Enter / Alt+Enter for newline-inside-buffer depends on the
   macOS Terminal forwarding the modifier, which Terminal.app
   often strips. Provide Ctrl+Enter (terminal-independent) and
   Ctrl+J (emacs "line feed") as well.

**Files touched**:
- `src/tui/input_state.rs` — new `move_prev_line` /
  `move_next_line` methods on `PromptInputState`. Both split the
  buffer on `\n` and walk to the same visual column on the
  previous / next line, clamping the column to the target line's
  length (emacs `previous-line` / `next-line` semantics). 10 new
  unit tests covering same-column move, clamping to a shorter
  line, no-op on the first / last line, three-line step-by-step
  walks in both directions, and the empty-intermediate-line
  case.
- `src/tui/app/commands.rs` — dispatcher changes (note: this
  file lives in `app/commands.rs` after the Goal 220 split):
  - `Ctrl+B` → `prompt.move_left()` (was: scroll_offset += 10).
  - `Ctrl+F` → `prompt.move_right()` (was: scroll_offset -= 10).
  - `Ctrl+P` → `prompt.move_prev_line()` (new).
  - `Ctrl+N` → `prompt.move_next_line()` (new).
  - `KeyCode::Enter` with `CONTROL` modifier now joins the
    existing `SHIFT` / `ALT` modifier guard for newline
    insertion.
  - `KeyCode::Char('j')` with `CONTROL` modifier (emacs line
    feed) inserts `\n`.
  - Removed the two tests that asserted the old Ctrl+B/F
    page-scroll behaviour; left a comment block pointing at
    the new dispatcher tests.
- `src/tui/keymap.rs` — 8 new dispatcher tests:
  `dispatch_ctrl_b_moves_cursor_left`,
  `dispatch_ctrl_f_moves_cursor_right`,
  `dispatch_ctrl_b_does_not_scroll_transcript`,
  `dispatch_ctrl_p_moves_cursor_to_previous_line`,
  `dispatch_ctrl_n_moves_cursor_to_next_line`,
  `dispatch_ctrl_n_in_single_line_is_noop`,
  `dispatch_ctrl_j_inserts_newline_without_submitting`,
  `dispatch_ctrl_enter_inserts_newline_without_submitting`, plus
  `dispatch_plain_enter_still_submits` regression guard.

**Quality gates**:
- `cargo test --workspace` — clean.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — clean.

**Notes**:
- Transcript scroll is now reachable via `PageUp` / `PageDown`,
  `Shift+↑` / `Shift+↓` (single line), and the mouse wheel —
  all of which already worked. The old `Ctrl+B` / `Ctrl+F`
  scroll fallback was a "Goal 150 follow-up" written when some
  terminals stripped `SHIFT` from arrow keys; that no longer
  matches the primary user's environment.
- Ctrl+B/F still wins over transcript scrolling **even when the
  buffer is empty**: empty-buffer behaviour is just "cursor is at
  position 0, moving left is a no-op". This is intentional — the
  user should not get surprise scrolling from pressing B/F.
- `Ctrl+J` (LF) and `Ctrl+Enter` are both wired to the same
  newline branch, so either one works; the user can pick the one
  their terminal forwards. This addresses the Terminal.app
  `Option+Enter` interception issue without forcing the user to
  reconfigure their terminal.
- The empty intermediate line tests are important because
  Shift+Enter creates `foo\n\nbar` layouts; Ctrl+N from the
  middle row must not blow up when that row has length 0.
