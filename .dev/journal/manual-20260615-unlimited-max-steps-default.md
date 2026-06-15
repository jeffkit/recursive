# Manual edit: unlimited max_steps default

**Date**: 2026-06-15
**Goal**: Recursive 默认 `max_steps=0`（不限步数），与 `max_concurrent_runs=0` 语义一致
**Files touched**: `src/config.rs`, `src/run_core.rs`, `src/kernel.rs`, `src/main.rs`, `src/agent/types.rs`, `.dev/flows/self-improve.flow.js`, `.dev/flows/SELF_IMPROVE.md`, `.claude/skills/recursive-loop/SKILL.md`, `.dev/scripts/self-improve.sh`
**Tests added**: `default_max_steps_is_unlimited`, `effective_step_limit_zero_means_unbounded`
**Notes**: `0` → loop cap `usize::MAX`；显式 `--max-steps N` / `RECURSIVE_MAX_STEPS=N` 仍可设有限预算。self-improve flow 不再默认注入 200。
