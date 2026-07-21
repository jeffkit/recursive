# Manual edit: g325-otel-land

**Date**: 2026-07-14
**Goal**: Salvage Goal 325 OTEL exporter from failed-preserve worktree and land on main (PR #12)
**Files touched**:
- `crates/recursive-cli/src/otel.rs` (new)
- `crates/recursive-cli/src/main.rs` (`LoggingGuard` + Registry layers)
- `crates/recursive-cli/Cargo.toml` (`otel` feature)
- `Cargo.lock`, `.dev/journal/otel-325-feature.md`
**Tests added**: 7 unit tests in `otel.rs` (pure helpers; no live collector)
**Notes**:
- Original Flowcast preserve was **not** a product e2e regression — gate misconfig:
  `onFail=resume-fix` without `resumeFix` callback → `failed-preserved`.
- Cherry-picked onto current main; local build/test/clippy/fmt green; CI green; merged.
- Removed preserve worktree `selfimprove-1783681495143` after land.
