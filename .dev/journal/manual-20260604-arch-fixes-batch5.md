# Manual edit: arch-fixes-batch5

**Date**: 2026-06-04
**Goal**: Complete remaining arch-review issues from batch 5 (#20, #37, #40, #51)
**Files touched**:
- `src/http/handlers.rs` — list_sessions uses atomic counter (#20)
- `src/http/mod.rs` — SessionState.non_system_message_count (#20)
- `src/tui/backend.rs` — wait_for_cancel → tokio::sync::Notify (#37)
- `src/tools/facts.rs` — DuplicateResult enum (#40)
- `src/tools/sub_agent.rs` — fix misleading max_steps doc (#51)
- `tests/http.rs` — update test SessionState initializer

**Tests added**: none (existing tests updated/passing)

**Notes**:
- Issue #37: added `#[allow(clippy::too_many_arguments)]` to `worker_loop` since
  it needs 8 params after adding `cancel_notify`; alternatives (struct grouping)
  would require larger refactor.
- Issue #51: chose doc fix over implementation change since wiring parent budget
  to SubAgent would require significant API surface changes across the call chain.
- All stashed workspace changes on main were preserved via `git stash` before merge.
