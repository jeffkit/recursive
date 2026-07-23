# Goal 343 — TUI bracketed paste: stop pasted newlines from auto-submitting

**Roadmap**: TUI usability (input box — paste handling)

**Design principle check**:
- Implemented as: terminal-mode toggle (`EnableBracketedPaste` /
  `DisableBracketedPaste`) in the TUI entry point + a new `Event::Paste`
  arm in the event loop + a `handle_paste` method on `App` that inserts
  the pasted text into the prompt buffer without submitting.
- ❌ Does NOT branch inside `src/run_core.rs::RunCore::run_inner`.
- ❌ Does NOT touch the kernel / run loop / providers / tools.
- ❌ Does NOT introduce a new `Error` variant (invariant #7).

## Why

Pasting multi-line text into the Recursive TUI today submits the message
immediately instead of landing it in the input box for editing. Observed
(screenshot 2026-07-23): pasting
`用 deepseek 跑 self-improve 的 goal 329 (.dev/goals/329-compact-mod-split.md) …`
produced two `Loop started` system blocks — the pasted text (with a trailing
newline) was treated as a submitted prompt.

Root cause: `crates/recursive-tui/src/lib.rs` never enables bracketed paste
mode, and the event loop only matches `Event::Key` / `Event::Mouse`
(`_ => {}` drops everything else). Without bracketed paste, the terminal
delivers pasted bytes as ordinary keystrokes; the embedded `\n` arrives as a
bare `KeyCode::Enter` with no modifiers, which `app/commands.rs:197`
(`KeyCode::Enter => self.submit_prompt()`) treats as "send message". So the
first line of a paste is submitted, and subsequent lines either submit empty
or append — the user never sees the text in the input box.

This is the standard TUI paste bug. The fix is the standard two-piece remedy:
enable bracketed paste so capable terminals wrap pastes in
`\x1b[200~ … \x1b[201~` (crossterm surfaces this as `Event::Paste(String)`),
and handle `Event::Paste` by inserting the whole string into the buffer
without submitting. Terminals without bracketed paste degrade to current
behaviour (unavoidable — without the protocol there is no way to tell a
pasted newline from a typed Enter).

crossterm 0.28 (already in `crates/recursive-tui/Cargo.toml`) provides
`EnableBracketedPaste`, `DisableBracketedPaste`, and `Event::Paste(String)`.
No new dependency.

## Scope (do exactly this, no more)

### 1. `crates/recursive-tui/src/lib.rs` — enable + handle bracketed paste

- **Startup**: after `EnableMouseCapture` (line ~160), also execute
  `EnableBracketedPaste`. Import it from `crossterm::event`.
- **Teardown**: in `RawModeGuard::drop`, execute `DisableBracketedPaste`
  before the existing `DisableMouseCapture` / `LeaveAlternateScreen` /
  `disable_raw_mode`. Keep the `let _ = …` ignore-error pattern (matches
  existing style; the guard's drop body is `#[cfg_attr(test, mutants::skip)]`).
- **Event loop**: in the `match event::read()?` block (lines ~179-187), add
  an arm *before* the catch-all `_ => {}`:
  ```rust
  Event::Paste(text) => app.handle_paste(&text),
  ```
  `handle_paste` returns no `UserAction` (paste never submits), so it does
  not participate in the `if let Some(action) = keymap::dispatch(...)` send
  path — call it directly like the `Event::Mouse` arm calls `handle_mouse`.

### 2. `crates/recursive-tui/src/app/commands.rs` — `handle_paste`

Add a public method on `App`:
```rust
/// Insert pasted text into the prompt buffer at the cursor, preserving
/// newlines. Never submits, never triggers first-char mode auto-detect
/// (`!`/`#`/`/`/`@`) — a paste is raw content, not typed input.
pub fn handle_paste(&mut self, text: &str) {
    for c in text.chars() {
        self.prompt.insert_char(c);
    }
}
```
Rationale for NOT going through `handle_char_input`: that method auto-switches
mode on a leading `!`/`#`/`/` when the buffer is empty, and enters AtFile
mode on `@`. A pasted block that starts with one of those (e.g. a shell
snippet starting with `!`, or markdown with `#` headings) would silently
flip modes — surprising and wrong. Paste must insert literally. `insert_char`
already advances the cursor char-boundary-correctly and resets
`history_idx`, which is the right behaviour for a paste.

Edge cases to handle correctly (the tests below pin them):
- Empty paste → no-op (loop over zero chars).
- Paste containing `\n` → newlines inserted, buffer becomes multi-line,
  cursor at end. No submit.
- Paste when a modal / permission / plan-approval dialog is pending:
  `handle_key` short-circuits those before reaching the chat `match`, but
  `handle_paste` is called directly from the event loop, bypassing that
  routing. Guard: if `pending_permission.is_some()`, or
  `plan_awaiting_approval`, or `plan_mode_request_pending`, or
  `!self.modals.is_empty()`, or `prompt.mode == CommandInteract`, drop the
  paste (return early) so it doesn't corrupt a dialog state. Reuse the same
  predicates `handle_key` checks at its top.

### 3. Tests (TUI — mandatory in same commit per CLAUDE.md)

In-process harness tests in `app/commands.rs` `#[cfg(test)] mod tests`
(use `crate::harness::Harness` where rendering is needed; otherwise direct
`App::new()` + `handle_paste` assertions are fine since `handle_paste` is
pure buffer mutation):

- `paste_inserts_text_into_buffer_without_submitting` — fresh `App`, call
  `handle_paste("hello")`, assert `app.prompt.buffer == "hello"`, cursor at
  end, and that no `UserAction` was produced (the event-loop arm calls
  `handle_paste` directly; assert the method signature returns `()`).
- `paste_with_newlines_does_not_submit` — `handle_paste("line1\nline2")`,
  assert `app.prompt.buffer == "line1\nline2"` and no transcript `User`
  block was pushed (blocks stay empty).
- `empty_paste_is_noop` — `handle_paste("")`, buffer stays empty.
- `paste_does_not_trigger_mode_autodetect` — fresh `App`,
  `handle_paste("!ls -la")`, assert `app.prompt.mode == InputMode::Prompt`
  (NOT `Bash`) and `buffer == "!ls -la"` (the `!` is literal content).
- `paste_does_not_enter_atfile_on_at_sign` — `handle_paste("@file.txt")`,
  assert mode stays `Prompt`, buffer == `"@file.txt"`, `atfile` state not
  entered.
- `paste_dropped_while_permission_modal_pending` — set
  `app.pending_permission = Some(...)`, `handle_paste("x")`, assert buffer
  stays empty. (Use the existing `set_pending_permission` helper or
  construct the `PermissionRequest` with a dummy `oneshot::channel()`.)
- `paste_dropped_while_plan_awaiting_approval` — set
  `app.plan_awaiting_approval = true`, `handle_paste("x")`, assert buffer
  empty.
- `paste_dropped_while_modal_open` — push any modal onto `app.modals`,
  `handle_paste("x")`, assert buffer empty.

Add a `lib.rs` test that the event loop routes `Event::Paste` to
`handle_paste` is NOT feasible (the loop is inside `run_with_backend`,
which owns the real terminal). Instead cover the routing contract by
keeping `handle_paste` `pub` and asserting behaviour through the method
directly; the `lib.rs` change is a one-line arm reviewed by the TUI
mutation/presence gates.

## Acceptance

- `cargo test --workspace` green; `cargo clippy --all-targets --all-features
  -- -D warnings` clean; `cargo fmt --all` clean.
- `.dev/scripts/tui-test-presence.sh` PASS (TUI src changed with tests
  added in the same commit).
- `.dev/scripts/tui-mutants.sh` (advisory for manual edits; mandatory for
  the flow) — run it; fix survivors inside the diff hunks (the
  `handle_paste` body and the `lib.rs` paste arm are the changed regions).
- Manual check (document in journal): on a bracketed-paste-capable terminal
  (iTerm2 / WezTerm / Kitty / Alacritty), pasting multi-line text lands it
  in the input box unsubmitted; pressing Enter then submits. On a terminal
  without bracketed paste, behaviour is unchanged (degraded, documented).
- No new `Cargo.toml` dependency.

## Notes for the agent

- **Read first:** `crates/recursive-tui/src/lib.rs` (entry point + event
  loop + `RawModeGuard`), `crates/recursive-tui/src/app/commands.rs`
  (`handle_key`, `handle_char_input`, `submit_prompt`, and the top-of-
  `handle_key` guard predicates), `crates/recursive-tui/src/input_state.rs`
  (`PromptInputState::insert_char`), and `crates/recursive-tui/src/harness.rs`
  for the in-process test harness.
- **crossterm 0.28 API:** `crossterm::event::{EnableBracketedPaste,
  DisableBracketedPaste}` are `ExecuteCommand`-style commands used as
  `io::stdout().execute(EnableBracketedPaste)?`. `Event::Paste(String)` is
  a variant of `crossterm::event::Event`. Confirm by grepping the
  `crossterm` source in `~/.cargo` if unsure, but do NOT upgrade crossterm.
- **Guard ordering in `handle_paste`:** mirror the early-return predicates
  from the top of `handle_key` (permission, CommandInteract, plan approval,
  plan-mode request, non-empty modal stack). Paste must be a no-op while
  any of those is active — do not let a paste land in the buffer behind a
  dialog and then get submitted when the dialog closes.
- **Do NOT route paste through `handle_char_input`** — that method's
  first-char mode auto-detect and `@`-AtFile entry are typed-input
  behaviours; paste is raw insertion. Use `PromptInputState::insert_char`
  directly in a loop.
- **Do NOT add a `UserAction::Paste`** variant — paste is a pure UI-input
  event with no backend side effect; adding a `UserAction` would needlessly
  widen the backend surface.
- **Terminal without bracketed paste** degrades to today's behaviour. That
  is expected and not a bug — note it in the journal. Do not try to
  heuristically detect "pasted newline vs typed Enter" from timing; it is
  unreliable and out of scope.
- **DO NOT modify** `src/` (kernel/runtime/run_core), `src/llm/`,
  `src/tools/`, providers, or the backend worker (`backend.rs` /
  `runtime_builder.rs`) — paste is entirely a TUI-input concern.
- Journal entry: `.dev/journal/manual-<YYYYMMDD>-tui-bracketed-paste.md`.
