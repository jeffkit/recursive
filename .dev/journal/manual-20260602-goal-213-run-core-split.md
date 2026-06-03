# Manual edit: Goal 213 — extract RunCore into src/run_core.rs

**Date**: 2026-06-02
**Goal**: Split `src/agent.rs` (~3526 lines) by extracting `RunCore<'a>` and `RunInnerOutcome` into a new `src/run_core.rs` module.
**Files touched**:
- `src/run_core.rs` — new file (766 lines): `TRIM_PLACEHOLDER`, `STUCK_THRESHOLD`, `RunInnerOutcome`, `RunCore<'a>` struct + full `impl<'a> RunCore<'a>` block
- `src/agent.rs` — reduced from 3526 to 2792 lines: moved code replaced with `pub(crate) use crate::run_core::{RunCore, RunInnerOutcome}`, removed now-unused imports (`Ordering`, `debug`, `warn`, `Completion`, `StreamSender`), `TRIM_PLACEHOLDER` constant replaced with import from `run_core`
- `src/lib.rs` — added `pub mod run_core;`

**Tests added**: none (extraction only, no logic change)

**Notes**:
- `RunCore<'a>` imports `StepEvent`, `FinishReason`, `PermissionDecision`, `PermissionHook`, `PlanningMode`, `OnMessageFn` from `crate::agent` (intra-crate circular module references are fine in Rust)
- `TRIM_PLACEHOLDER` moved to `run_core.rs` as `pub(crate)` and imported back into `agent.rs` for use in Agent's legacy methods
- `STUCK_THRESHOLD` moved to `run_core.rs` (private there — only used by RunCore)
- `#[allow(unused_imports)]` added on the re-export line in `agent.rs` since `RunInnerOutcome` is never explicitly named in agent.rs (only accessed via type inference through `run_inner()`)
- All 3 quality gates passed: `cargo test --workspace`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo fmt --all`
