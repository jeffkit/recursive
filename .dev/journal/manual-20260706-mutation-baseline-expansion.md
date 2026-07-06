# Manual edit: mutation-baseline-expansion

**Date**: 2026-07-06
**Goal**: Fix failing config test and systematically add mutation-test baseline coverage to previously untested files
**Files touched**:
- `src/config.rs` — fix `web_search_provider_empty_string_becomes_none` (pin RECURSIVE_HOME)
- `src/session/serialize.rs` — add 7 tests (all 5 mutants, 0 → 7 tests)
- `src/multi.rs` — add 11 tests (covers all(), AgentMode::parse, remove_role, coordinator_system_prompt, default_roles content, register_subagent_if_enabled noop path)
- `src/tools/registry.rs` — add 13 tests (register/find/names/specs, retain_tools, is_readonly, fork, build_standard_tools; 0 → 13 tests)
- `src/tools/memory.rs` — add 21 tests (MemoryStore load/save/next_id/add/remove/search, chrono_now_rfc3339, days_to_date; 0 → 21 tests)
- `src/tools/audit.rs` — add 14 tests (is_false, synthetic_unknown_tool, TouchedFiles::is_empty/paths_sorted, unix_millis, truncate_for_audit, blake3_canonical_json; 0 → 14 tests)

**Tests added**: 66 new tests across 6 files
**Notes**:
- Fixed `web_search_provider_empty_string_becomes_none` failure: `Config::from_env()` reads `~/.recursive/config.toml` via `FileConfig::load()`. On a machine with `[search] provider = "brave"`, the empty env var is filtered → None but then falls back to the file config → `Some("brave")`. Fix: use `PinnedRecursiveHomeNoLock` (env_lock already held) to redirect `RECURSIVE_HOME` to a temp dir with no config file.
- Launched full-codebase baseline scan: `agent-mutants.sh --jobs 4 --all` → 4172 mutants across 105 files.
- Build phase takes ~20 minutes with 4 parallel workers (large project). Actual mutation testing will follow.
- Pre-scan zero-test gap analysis: tools/registry.rs (69 mutants), tools/memory.rs (141 mutants), tools/audit.rs (24 mutants), session/serialize.rs (5 mutants) were all completely untested.
- Equivalent mutant strategy: some function-replacement mutants (e.g. `local() → Default::default()`) create infinite recursion → cargo-mutants exit code 3 (timeout = pass). These don't need explicit tests.
