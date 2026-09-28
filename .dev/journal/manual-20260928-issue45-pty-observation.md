# Issue #45 — tui-pty-harness observation parity

- Date: 2026-09-28
- Goal: expose cell attrs / scrollback / raw-stream tee in the PTY integration layer (issue #45).
- Files touched:
  - crates/tui-pty-harness/src/lib.rs (RunSpec.record_raw + reader tee, SCROLLBACK_ROWS=200, CellAttrs + Color re-export, Screen::{cell_attrs,bg,row_has_bg_color,find_row,history_lines})
  - crates/tui-pty-harness/src/main.rs (--record-raw flag)
  - crates/tui-pty-harness/tests/observation-parity.rs (renamed from wip-issue45-*)
  - crates/recursive-tui/tests/pty_regression.rs (record_raw: None)
  - crates/recursive-tui/tests/pty_bg_semantics.rs (new semantic lock: SGR41 ↔ ratatui Red ↔ vt100 Idx(1))
- Tests added: 3 observation-parity PTY tests, 2 pty_bg_semantics tests, 1 pure-logic unit test (bg Default→None).
- Notes: vt100 0.15.2 `set_scrollback` with large offsets panics in `visible_rows` (usize underflow); history is read via temporary `set_size(rows+len)` windowing instead — see 02-plan.md 实施记录.
