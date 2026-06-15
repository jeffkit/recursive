# Manual edit: goal261-task4-read-file-state-wiring

**Date**: 2026-06-15
**Goal**: Wire ReadFileState into CLI tool registry and sub-agent registry (Goal 261, Task 4)
**Files touched**:
- `src/cli/builder.rs`
- `src/tools/agent.rs`

**Tests added**: none (existing tests cover the wired paths)

**Notes**:
- `src/cli/builder.rs::build_tools`: added `Arc<Mutex<ReadFileState>>`, attached via
  `with_read_file_state`, passed to `ReadFile::with_read_state` and newly-registered
  `EditTool::with_read_state`. `EditTool` was previously absent from the CLI registry.
- `src/tools/agent.rs::build_sub_registry`: sub-agents now receive a **fresh**
  `ReadFileState` (not the parent's). `Read` and `Edit` tool names are special-cased
  to construct new instances bound to the sub-agent's state; all other tools are
  forwarded from the parent registry as before.
- `src/tools/mod.rs::build_standard_tools` was already wired (Task 3); no changes needed there.
