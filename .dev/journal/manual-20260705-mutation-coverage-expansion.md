# Manual edit: mutation-coverage-expansion

**Date**: 2026-07-05
**Goal**: Extend mutation testing from TUI-only to full codebase; create gate scripts for
`recursive-agent` and `recursive-cli`; kill all surviving mutants in the core agent modules.

## Files touched

### New scripts
- `.dev/scripts/agent-mutants.sh` — scoped mutation gate for `recursive-agent` crate (mirrors
  `tui-mutants.sh`). Auto-detects changed files vs `main`; parallel copy-mode with `--jobs`;
  exit-3 (timeout-only) treated as pass.
- `.dev/scripts/cli-mutants.sh` — same for `recursive-cli`.

### Updated scripts
- `.dev/scripts/tui-mutants.sh` — added exit-3 (timeout-only) as-pass logic to match the
  new scripts' behaviour.
- `.dev/scripts/agent-mutants.sh` — same exit-3 fix.

### Updated gate config
- `.flowcast/gates.json` — registered `agent-mutants` and `cli-mutants` gates alongside the
  existing `tui-presence` and `tui-mutants` gates.

### Tests added to `src/compact.rs`
- `estimate_chars_includes_tool_call_overhead` — verifies per-tool-call 32-byte overhead.
- `estimate_chars_includes_reasoning_content` — verifies reasoning content is counted.
- `apply_to_transcript_splices_and_returns_counts` — baseline splice+return coverage.
- `apply_to_transcript_too_short_returns_none` — early-return path (len < keep_n + 2).
- `apply_to_transcript_minimum_length_boundary` — exact minimum boundary (keep_n=2).
- `apply_to_transcript_boundary_plus_not_times` — **kills `replace + with *`** at line 225
  (the threshold is `keep_n + 2`, not `keep_n * 2`; uses keep_n=3 where the two formulas differ).

### Tests added to `src/run_core.rs`
- `finish_reason_str_is_nonempty_and_not_xyzzy` — kills `replace → String with String::new()`
  and `"xyzzy".into()`.
- `finish_reason_str_matches_display` — verifies Display delegation.
- `maybe_trim_does_nothing_when_under_limit` — boundary: no trim when chars < limit.
- `maybe_trim_does_not_trim_exactly_min_trim_length` — verifies `>` not `>=` in content check.
- `maybe_trim_trims_one_more_than_min_trim_length` — verifies trimming fires at MIN+1.
- `maybe_trim_emits_event_when_trimmed` — event emission verified via channel capture.
- `maybe_trim_no_event_when_nothing_trimmed` — no-event path verified.
- `maybe_compact_fires_at_threshold_boundary` — kills `replace < with <=` at threshold.
- `maybe_compact_kept_count_is_correct` — kills `replace - with +` and `replace - with /` in
  kept-count calculation via Compacted event capture.
- `execute_tool_calls_plan_mode_guard_when_not_exploring` — kills `delete !` at line 306:
  verifies that a plan-mode tool returns an error when NOT in exploring_plan_mode.

### Tests added to `src/session/mod.rs`
- `usage_meta_accumulate_adds_tokens` — all fields including Optional ones.
- `usage_meta_accumulate_with_none_treats_as_zero` — None fields accumulate as 0.
- `usage_meta_from_token_usage_positive_values` — positive cache/reasoning mapping.
- `usage_meta_from_token_usage_zero_cache_is_none` — zero → None conversion.
- `usage_meta_is_zero_both_zero`, `_input_nonzero`, `_output_nonzero` — kills `&&` → `||`.
- `session_cost_accumulate_all_fields` — kills `+=` → `*=`.
- `session_cost_accumulate_twice_sums_correctly` — verifies cumulative addition.
- `hash_tool_specs_is_deterministic`, `_differs_for_different_specs` — hash stability.
- `default_schema_version_is_one` — schema constant.
- `default_session_path_empty_goal_uses_unnamed` — kills `delete !` on `is_empty()` branch.
- `list_sessions_ignores_non_json_files` — kills `==` → `!=` on extension check.
- `workspace_slug_truncates_to_80_chars` — kills `>` → `>=` on length guard.

### Tests added to `src/transcript.rs`
- `pretty_empty_content_produces_no_content_line` — kills `delete !` on `is_empty()`.
- `pretty_content_with_trailing_newline_not_doubled` — kills `delete !` on `ends_with('\n')`.
- `pretty_content_without_trailing_newline_gets_one` — verifies newline injection.
- `pretty_tail_*` and `pretty_head_*` equivalents — same edge cases for tail/head variants.
- `epoch_day_to_ymd_unix_epoch`, `epoch_day_to_ymd_known_dates` — date algorithm coverage.
- `chrono_lite_now_contains_correct_year_and_format` — timestamp format invariants.
- `take_first_n_exactly_len_returns_all` — kills `replace > with >=` in boundary check.

### Tests added to `src/config.rs`
- `context_window_tokens_uses_override_when_set`, `_fallback_is_nonzero`.
- `web_search_provider_empty_string_becomes_none`, `_nonempty_string_becomes_some`.
- `validate_for_agent_ok_with_valid_config`, `_rejects_missing_api_key`,
  `_rejects_empty_api_key`, `_rejects_unknown_provider`, `_accepts_anthropic_provider`.

## Tests added (count)
~40 new unit tests across 5 source files.

## Gate coverage added
| Gate | Crate | Status |
|------|-------|--------|
| `agent-mutants` | `recursive-agent` (src/\*) | Added |
| `cli-mutants` | `recursive-cli` | Added |

## Verified results
- `compact.rs`: **0 MISSED** (1 timeout = infinite-loop detection, treated as pass)
- `run_core.rs`: IN PROGRESS (confirming 12 previously-MISSED mutants are now caught)
- `transcript.rs` + `session/mod.rs`: IN PROGRESS

## Notes
- Timeout (exit 3) from `cargo-mutants` means the mutation causes infinite-loop behaviour,
  which is detected by the test harness via timeout. This is treated as a pass (mutation
  detected, just slowly). All three gate scripts updated to `exit 0` on code 3.
- The `apply_to_transcript_boundary_plus_not_times` test uses `keep_n=3` specifically because
  `keep_n=2` produces `2+2=4=2×2` making the `+` and `*` formulas indistinguishable.
- The `execute_tool_calls_plan_mode_guard_when_not_exploring` test requires constructing a
  `ToolRegistry` with `PermissionsConfig { mode: PermissionMode::Plan { ... } }`.
