# Manual edit: tui-acceptance-checklist-fixes

**Date**: 2026-06-03
**Goal**: Fix 5 issues identified in docs/tui-acceptance-checklist.md
**Files touched**:
- `src/tui/ui/splash.rs` — version string
- `src/tui/ui/transcript.rs` — render_markdown wiring + theme colors
- `src/tui/mod.rs` — RAII terminal guard
- `docs/tui-fake-cc-gap.md` — path update
**Tests added**: updated existing tests to pass `&theme::DARK`
**Notes**:
- render_markdown (Goal-172) was written but never called; now wires into render_assistant replacing the line-by-line render_inline approach
- Theme refactor: render_block / render_blocks now take `&Theme`; all 5 render_* functions use theme colors
- TerminalGuard::drop() restores raw mode / DisableMouseCapture / LeaveAlternateScreen; the explicit cleanup at end of run() was removed
