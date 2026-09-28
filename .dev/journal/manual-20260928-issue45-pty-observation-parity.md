# Issue #45 manual journal — tui-pty-harness observation parity

**Date:** 2026-09-28
**Goal:** Expose cell attrs + row_has_bg_color, scrollback history, and raw PTY byte recording in tui-pty-harness so the SOP's colour/scrollback assertions are expressible at the integration layer (issue #45).

## Files touched
- `crates/tui-pty-harness/src/lib.rs` — `CellAttrs`, `Color` re-export, `SCROLLBACK_ROWS=200`, `Screen.attrs/history`, `cell_attrs`/`bg`/`row_has_bg_color`/`history_lines`, `RunSpec.record_raw` + reader-thread tee, scrollback extraction via `set_scrollback(usize::MAX)` clamp → iterate offsets → `set_scrollback(0)` restore.
- `crates/tui-pty-harness/src/main.rs` — `--record` CLI flag wired to `RunSpec.record_raw`.
- `crates/tui-pty-harness/tests/observation-parity.rs` — (renamed from wip-issue45-*, `wip_`→ no prefix) three PTY tests: cell attrs/SGR41, scrolled-off content in history, raw tee contains `\x1b[41m`.
- `crates/recursive-tui/tests/pty_bg_semantics.rs` — PTY-layer SGR41↔Idx(1) lock + new in-process ratatui Red counterpart (same file, no new deps).
- `crates/recursive-tui/tests/pty_regression.rs` — `record_raw: None` at the RunSpec construction point.

## Tests added
- `screen_exposes_cell_attrs_and_row_has_bg_color`, `scrolled_off_content_is_assertable`, `record_raw_tees_original_pty_bytes_with_sgr` (tui-pty-harness).
- `inproc_row_has_bg_color_matches_ratatui_red_mapping` (recursive-tui, all platforms).

## Notes
- vt100 0.15 trap: `Screen::scrollback()` is the scroll *offset*, not history depth. Depth = `set_scrollback(usize::MAX)` (clamped) then `scrollback()`. Plan's predicted fix applied; extra clamp step was the missing piece (the prior code read offset 0).
- Verification: `cargo test -p tui-pty-harness` (7+3 green), `cargo test -p recursive-tui --test pty_bg_semantics --test pty_regression` (2+2 green), clippy clean on both crates, `cargo fmt` applied. Full-workspace gates deferred to pipeline per instructions.
