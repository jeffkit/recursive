# Manual edit: tui-loop-slash-dispatch

**Date**: 2026-07-29
**Goal**: Fix `/loop` command not triggering the TUI loop when input is pasted or submitted while in `InputMode::Prompt`.

## Root Cause

Goal-343 intentionally skips mode auto-detection on bracketed paste, so pasting `/loop <goal>` leaves the buffer in `InputMode::Prompt` rather than flipping to `InputMode::Command`. When the user pressed Enter, `submit_prompt` processed it as a plain `SendMessage`, bypassing `dispatch_slash_command` entirely. The backend received the text literally, and `loop_state` was never set — so `loop_arbiter` never started.

The agent still interpreted the text and called `schedule_wakeup`, but without an active loop the wakeup had no effect.

## Fix

Modified the `InputMode::Prompt` arm of `submit_prompt` in `crates/recursive-tui/src/app/commands.rs` to detect slash commands at submission time:

- If the buffer starts with `/` and the first token matches a known built-in or skill command, delegate to `dispatch_slash_command` (same path as `InputMode::Command`).
- Otherwise, fall through to `SendMessage` as before.
- Deliberately **not** an early return, so `record_submission` (history) still runs for both paths.

Also resolved two pre-existing merge conflicts between HEAD and commit `324aa2c`:
- `src/run_core.rs`: kept HEAD's new compaction/circuit-breaker tests and merged in the incoming `// Kills:` comment for the run_inner section header.
- `src/runtime.rs`: removed the duplicate `use crate::Compactor;` import (incoming fix) and cleaned up unused `CompactionRunner` / `Microcompactor` imports that were left dangling.

**Files touched**:
- `crates/recursive-tui/src/app/commands.rs` — logic fix + 4 new tests
- `src/run_core.rs` — merge conflict resolution (no logic change)
- `src/runtime.rs` — merge conflict resolution + unused import cleanup

**Tests added**:
- `pasted_known_slash_command_in_prompt_mode_dispatches_as_command`
- `pasted_loop_command_in_prompt_mode_dispatches_start_loop`
- `pasted_unknown_slash_text_in_prompt_mode_sends_as_message`
- `typed_slash_command_still_dispatches_via_command_mode`

---

## Fix 2: ESC spurious interrupt from fragmented SGR mouse events (manual-20260729-esc-double-press)

**Root cause identified**: The terminal sends SGR mouse events as `\x1b[<btn;col;rowM`. When this sequence is fragmented at the byte level, crossterm receives `\x1b` first, times out waiting for the rest, and emits `KeyCode::Esc`. The remaining `[<btn;col;rowM` bytes arrive later as individual character inputs. Since `handle_esc` Step 2 fires immediately on a single ESC during a running turn, this caused spurious interrupts whenever the user's mouse hovered over the TUI window while a turn was running.

**Evidence**: User reported seeing `[<35;92;39` characters appear in the input box after the interrupt — the exact remnant of a fragmented SGR mouse sequence `\x1b[<35;92;39M`.

**Files touched**:
- `crates/recursive-tui/src/app/commands.rs` — ESC interrupt now requires double-press within the window (same as Ctrl+C). First press shows "Press ESC again to interrupt". Added 3 new tests.
- `crates/recursive-tui/src/lib.rs` — Added ESC+`[` filter in the main event loop: if ESC arrives and the immediately-next pending event is `KeyCode::Char('[')`, the pair is discarded as a fragmented mouse sequence. Added `KeyCode` to crossterm import.

**Tests added**:
- `esc_during_running_turn_requires_double_press_to_interrupt`
- `esc_during_idle_turn_is_noop`
- `esc_outside_window_during_turn_does_not_interrupt`

**Notes**: All 775 TUI tests pass. `cargo clippy --all-targets --all-features -- -D warnings` clean. `cargo fmt --all` clean.
