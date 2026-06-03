# Manual edit: tui-system-prompt-memory-inject

**Date**: 2026-06-03
**Goal**: Fix TUI not loading global memory into system prompt on new sessions
**Files touched**: `src/tui/runtime_builder.rs`
**Tests added**: none (existing test `offline_mode_and_config_file_resolution` covers runtime build path)
**Notes**: 
- `cli/builder.rs` correctly passes `config.system_prompt` (which already includes memory_summary, scratchpad_summary, facts_summary, episodic_recall_summary from Config::from_env()) to AgentRuntimeBuilder
- `tui/runtime_builder.rs` was missing `.system_prompt(&config.system_prompt)` and `.max_steps(config.max_steps)` — two lines
- Result: every TUI session started with no system prompt and no memory context
