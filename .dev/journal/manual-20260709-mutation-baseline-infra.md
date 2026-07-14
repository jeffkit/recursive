# Manual edit: mutation-baseline-infra

**Date**: 2026-07-09
**Goal**: Institutionalize mutation-testing cadence — exclude fixture noise, add agent/cli presence gates, kill high-priority survivors from the partial agent baseline, start an agent mutant-debt tracker.

**Files touched**:
- `.cargo/mutants.toml` — exclude `tests/bin/**`, Display/Debug fmt, `serve_with_graceful_shutdown`
- `.flowcast/gates.json` — register `agent-presence` + `cli-presence` before their mutants gates
- `.dev/scripts/agent-test-presence.sh` — new presence gate for `src/`
- `.dev/scripts/cli-test-presence.sh` — new presence gate for `crates/recursive-cli/`
- `.dev/mutant-debt-20260709-agent.md` — living agent debt queue + cadence
- `Cargo.toml` — `mutants = "0.0.3"` dev-dep on recursive-agent
- `src/lib.rs` — extract `rewind_to_char_boundary` with `#[mutants::skip]` (equivalent `>→>=`)
- `src/config.rs` — tests for TEAM/SUBAGENT OR-gate, 16KB project-context cap, memory/scratchpad layer injection + separator
- `src/config_file.rs` — test for trailing-newline append in `set_secret`
- `src/kernel.rs` — test that non-summary first message is not prepended
- `src/coordinator.rs` — test that `RECURSIVE_COORDINATOR_MODE=0` stays off
- `src/hooks/mod.rs` — test that Continue does not short-circuit
- `src/checkpoint.rs` — document near-equivalent `||→&&` between `/` and `\` (existing slash/backslash tests pin each arm)

**Tests added**:
- `config::subagent_enabled_via_team_env_alone`
- `config::subagent_enabled_via_subagent_env_alone`
- `config::load_project_context_exactly_16kb_is_not_truncated`
- `config::from_env_injects_memory_and_scratchpad_layers`
- `config_file::set_secret_appends_when_file_lacks_trailing_newline`
- `kernel::kernel_run_does_not_prepend_non_summary_first_message`
- `coordinator::is_coordinator_mode_rejects_non_one_env_value`
- `hooks::continue_action_does_not_short_circuit`

**Notes**:
- GitNexus impact on `truncate_str` / `validate_session_id` was CRITICAL; this change only adds tests + config / equivalent-skip extraction — no production behaviour change beyond the helper extract in `truncate_str` (same semantics).
- `checkpoint.rs` git-dependent survivors (`gc`, `snapshot_for_session`, …) deferred to a dedicated session — tracked in the debt file.
- Partial baseline (2026-07-07) was ~10% of 4182 mutants; `mutants.toml` drops fixture/fmt noise so future `--all` runs are cleaner.
- Presence gates mirror TUI: cheap fail-fast before expensive mutants resume-fix cycles.
- `.gitignore` updated to track `.cargo/mutants.toml` (was covered by `.cargo/*`).
- Scoped config recheck: memory/scratchpad/facts/episodic `delete !` + layer-separator `>→>=` covered by `from_env_injects_memory_and_scratchpad_layers`.
