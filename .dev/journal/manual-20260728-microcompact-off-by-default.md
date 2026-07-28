# Manual edit: microcompact-off-by-default

**Date**: 2026-07-28
**Goal**: Disable Microcompactor by default. The old default (trigger=12) was designed for 128K models but fires way too early on 1M context models (deepseek-v4-flash), causing surprising ctx gauge drops at ~50-80K tokens — only 5-8% of the window. Users reported ctx decreasing without obvious reason.

**Change**: `build_microcompactor_from_env` now returns `None` when `RECURSIVE_MICROCOMPACT_TRIGGER` is unset. Users opt in by setting the env var (e.g. `RECURSIVE_MICROCOMPACT_TRIGGER=40`).

**Files touched**:
- `src/compact/micro.rs` — `build_microcompactor_from_env` default changed to disabled + updated tests

**Tests updated**:
- `build_microcompactor_from_env_defaults_when_unset` → `build_microcompactor_from_env_disabled_by_default` (now asserts None)
- `build_microcompactor_from_env_invalid_trigger_falls_back` → `build_microcompactor_from_env_invalid_trigger_returns_none` (invalid value now returns None, not fallback 12)

**Notes**:
- The Microcompactor is still fully functional; users can enable via env var.
- The underlying logic (prune old tool results by count) is sound — just needs a reasonable threshold for the model size. Proper fix would scale threshold by context window; this is tracked as future work.
- Related fix today: cross-turn compaction accumulated-tokens bug (commit e9aff8b).
