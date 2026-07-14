# Manual edit: small-file mutation gates

**Date**: 2026-07-10
**Goal**: Re-gate hooks/coordinator/kernel/config_file/config to 0 missed; land pins for survivors.
**Files touched**: `src/hooks/mod.rs`, `src/coordinator.rs`, `src/kernel.rs`, `src/config_file.rs`, `.dev/scripts/agent-mutants.sh`, `.dev/mutant-debt-20260709-agent.md`
**Tests added**: ToolTimingHook start-map pins; coordinator filter_registry + env_lock; kernel compaction prepend + max_transcript_chars; config_file set_secret newline edges
**Notes**: `config.rs` already gate-0 on re-run. `agent-mutants` FEATURES now include `coordinator-mode`.
