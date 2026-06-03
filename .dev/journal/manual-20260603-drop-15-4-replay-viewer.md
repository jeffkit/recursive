# Manual edit: drop Phase 15.4 (replay viewer)

**Date**: 2026-06-03
**Goal**: Remove Phase 15.4 "Replay viewer (web-based transcript replay)" from `.dev/ROADMAP-v4.md`. Concluded during review that the value is too low to justify the cost.

**Files touched**: `.dev/ROADMAP-v4.md` (removed 15.4 row + rewrote the Phase 15 total line)

**Tests added**: none

**Notes**:

The "view transcript" use case is already covered end-to-end:
- CLI: `recursive replay <file>` with `--head`, `--tail`, `--resume-from` (g09/g17/g20)
- TUI: 25 files in `src/tui/`, block-aware transcript rendering (markdown, diff, plan mode, tool call/result)
- Observability: tracing (g115), Prometheus metrics (g122), cost tracker (g116)
- Recovery: safe-replay policy for orphan tool calls (g154)
- Export: portable JSON format (g117) — still available for any third-party viewer

What's actually missing is small:
- TUI scrub/jump-to-step for long transcripts
- Per-message metadata overlay (token cost, timing) in scrub mode
- Side-by-side diff of two transcripts

If that gap ever matters, the right shape is a TUI extension (no new dependencies, fits the dev-tool audience), not a separate web surface. Recursive has consistently deferred web/UI work (Phase 16 plugins, SaaS-shaped features) for the same reason.

For now: 15.4 dropped. If a TUI scrub mode ever becomes a real need, file a fresh goal — don't reopen the web framing.
