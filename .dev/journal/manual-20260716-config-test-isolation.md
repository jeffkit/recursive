# Manual edit: config-test-isolation

**Date**: 2026-07-16
**Goal**: Fix config tests failing due to loading the developer's global `~/.recursive/config.toml` (which contained a custom preset `"nvidia"` not defined in checking `providers.toml`).
**Files touched**:
- [src/config.rs](file:///Users/kongjie/projects/recursive/src/config.rs)
**Tests added**: none (isolated existing tests)
**Notes**: Added `tempfile::tempdir()` and `PinnedRecursiveHomeNoLock` to 6 config tests (`default_max_steps_is_unlimited`, `retry_env_overrides_apply`, `shell_timeout_default_and_env_override`, `headless_env_var_sets_config`, `stuck_window_and_error_rate_env_override`, `goal_eval_transcript_tail_default_and_env_override`) that invoke `Config::from_env` but were not isolated from the user's local `RECURSIVE_HOME`/home directory configuration on macOS.
