# 20261003 — manual-20261003-issue81-tasks-domain-split (1/3)

## Goal

Issue #81 (拆单 from #60): 会话与任务域拆层 — move the session/task-domain
tools out of the flat `src/tools/` directory into a new `src/tasks/` module,
with `src/tools/mod.rs` keeping only re-exports (compat paths preserved).

## Files touched

Moved (`git mv`, 12 tool files from `src/tools/` → `src/tasks/`):

- `task_create.rs` / `task_get.rs` / `task_list.rs` / `task_output.rs` /
  `task_stop.rs` / `task_update.rs` (coordinator-mode `task_*` tools)
- `team_create.rs` / `team_delete.rs` (`team_*` tools)
- `stop_loop.rs` / `schedule_wakeup.rs` / `watch_file.rs` / `run_background.rs`
  (loop / wakeup / background-job tools)

`src/tasks.rs` → `src/tasks/mod.rs` (TaskRegistry / TaskState / TaskId domain
core stays put, now hosting its tools as submodules).

Edited:

- `src/tasks/mod.rs` — added `pub mod` declarations for the 12 submodules
  (coordinator-mode ones feature-gated, matching the old gating) + doc note.
- `src/tools/mod.rs` — replaced the 12 `pub mod` declarations with
  `pub use crate::tasks::<name>;` re-exports, so `crate::tools::run_background`,
  `recursive::tools::task_create`, … keep resolving for every existing caller
  (registry.rs, http, acp, mcp, tests). All `pub use` item re-exports unchanged.
- `src/tasks/{run_background,schedule_wakeup,stop_loop,watch_file}.rs` —
  `use super::{resolve_within, SessionToolState, Tool}` → `use crate::tools::…`
  (they no longer sit inside `tools/`); `super::transport::ToolTransport` →
  `crate::tools::transport::ToolTransport`. The `task_get/output/stop/update →
  super::task_create::lookup_task_id` imports still work unchanged (same parent
  module). No `task_*`/`team_*` file needed any import edit.
- `tests/invariants/test_coverage.rs` — MUST_HAVE_TESTS entries:
  `src/tools/run_background.rs` → `src/tasks/run_background.rs`,
  `src/tasks.rs` → `src/tasks/mod.rs`.
- docs (guarded by `tests/docs_living_paths.rs`):
  `docs/architecture/tools/{task-tools,multi-agent,index}.md` source-path
  columns updated to `src/tasks/…`.

## Tests added

None needed — pure code motion, zero behavior change; all existing unit tests
moved with their files (`use super::*` keeps working because submodule paths
are preserved inside `tasks/`).

## Notes

- Invariant #9 intent preserved: one tool per file, each registered via
  `pub mod` in `src/tasks/mod.rs`; the `tool_files_are_registered_in_mod_rs`
  invariant test still passes (it only scans `src/tools/`, which now contains
  no moved files — the moved files' declarations live in `src/tasks/mod.rs`).
- `src/tools/mod.rs` diff is purely: 12 `pub mod` lines → 12 `pub use` lines
  (+ comment); the item-level `pub use` re-exports and the inline test module
  are untouched, so there is no behavior diff for any consumer.
- `run_background::DEFAULT_KILL_GRACE_PERIOD` re-exported by `src/mcp.rs` via
  `crate::tools::run_background::…` — still resolves through the module
  re-export, no edit needed there.
- Gates: `cargo test --workspace` green (58 suites ok, incl. default-features
  run without failures), `cargo clippy --workspace --all-targets
  --all-features -- -D warnings` clean, `cargo fmt --all -- --check` clean.
- Parts 2/3 of #60 will follow the same pattern for other domains.
