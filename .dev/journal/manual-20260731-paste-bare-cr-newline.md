# Manual fix: pasted bare \r now becomes a real newline

Date: 2026-07-31
Goal: Fix multi-line paste losing newlines in the TUI input box

## Symptom

Pasting multi-line text into the TUI input box removed every line break —
the content arrived on one line. (Reported by Jeff.)

## Root cause

- Terminals deliver pasted newlines inside a bracketed-paste payload as a
  **bare `\r`** in raw mode (the same byte Enter sends). xterm / VTE / tmux /
  iTerm2 / macOS Terminal.app all do this.
- crossterm 0.28.1 `parse_csi_bracketed_paste` copies the payload verbatim
  (`String::from_utf8_lossy`), so `Event::Paste` carries `"line1\rline2"`.
- `App::handle_paste` (added in 0bc24fa to fix the earlier "first char glued
  to the left border / no real line break" artifact caused by passing raw
  `\r` through) skipped **every** `\r`. It only kept `\n`, so bare `\r` —
  the real delivery format — was eaten and the newline vanished.

## Fix

`handle_paste` now normalizes line endings to real `\n`:
- `\r\n` (Windows/macOS clipboard) → single `\n`
- bare `\r` (terminal paste convention) → `\n`

This resolves both the original left-border artifact (no raw `\r` ever
reaches the terminal) and the newline-loss bug (the buffer gets real `\n`,
which the input renderer already splits on for multi-line display).

## Files touched

- `crates/recursive-tui/src/app/commands.rs` — `handle_paste` normalization;
  replaced `paste_strips_bare_carriage_return` (pinned the buggy drop) with
  `paste_converts_bare_carriage_return_to_newline` and added
  `paste_converts_multiple_bare_carriage_returns`.

## Tests added

- `paste_converts_bare_carriage_return_to_newline` — bare `\r` → `\n`
- `paste_converts_multiple_bare_carriage_returns` — whole multi-line paste
  keeps all lines
- `paste_strips_carriage_returns_from_crlf` unchanged (still passes: `\r\n`
  collapses to a single `\n`)

## Verification

- `cargo test -p recursive-tui` — pass
- `cargo clippy -p recursive-tui --all-targets --all-features -- -D warnings` — clean
- `cargo fmt --all` — clean
- `.dev/scripts/tui-test-presence.sh` — PASS (test-bearing change)

## Note (pre-existing, unrelated — RESOLVED)

The previously-failing `cargo test --workspace` case
`loop_size_orthogonality::runtime_stays_manageable` (`src/runtime.rs` was
4044 lines vs the 3700 invariant limit) is **fixed on main** by the
concurrent self-improve flow's landing (`bedc32b`, Goal 349 land-preserve).
That landing split `AgentRuntimeBuilder` → `src/runtime/builder.rs` and
`CheckpointState` → `src/runtime/checkpoint.rs`, bringing `runtime.rs` to
3677 lines (≤ 3700).

Context: while that landing was in flight it reset the working tree, wiping
an in-progress manual refactor of `runtime.rs` (a more aggressive
builder/compact/goal/tests split) that had not been committed. That WIP is
superseded by the flow's fix and was not restored. The paste-bare-CR fix in
this journal was stashed by the flow ("protected during Goal 349 land") and
automatically restored on landing; verified green afterward:

- `cargo test --workspace` — all green (incl. `runtime_stays_manageable`)
- `cargo test -p recursive-tui --lib paste` — 18 passed
- `cargo clippy --all-targets --all-features -- -D warnings` — clean
- `cargo fmt --all` — clean
