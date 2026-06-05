# Manual edit: banner-style-refresh

**Date**: 2026-06-05
**Goal**: Refresh TUI startup banner style to match Claude Code aesthetic — orange accent color, muted gray hierarchy, per-span colored session arrows
**Files touched**: src/tui/mod.rs
**Tests added**: none (visual-only change, no logic changed)
**Notes**: Replaced cyan double-stroke box logo with single-stroke box in orange (#cd6432). Session list now uses orange `›` arrows with dimmer gray text. Version/workspace lines use graduated gray tones instead of DIM modifier. All quality gates pass.
