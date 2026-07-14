# Manual edit: checkpoint-mutant-debt

**Date**: 2026-07-09
**Goal**: Kill high-priority `src/checkpoint.rs` survivors from the partial agent baseline (list delta branch, gc missing-dir guard, read_file_at Ok(None), diff path filter).

**Files touched**:
- `src/checkpoint.rs` — 5 tests + equivalent helpers + `gc` soft-skip
- `.dev/mutant-debt-20260709-agent.md` — mark checkpoint progress

**Tests added**:
- `list_for_session_non_root_counts_delta_not_full_tree` — kills `< → >` on parent-vs-empty-tree branch
- `gc_is_ok_when_shadow_dir_missing` — pins missing-dir early return
- `gc_succeeds_on_populated_shadow_repo` — soft pin for gc Ok path + reachable refs
- `read_file_at_missing_is_none_not_err` — pins `Ok(None)` for missing blob
- `diff_with_path_filter_limits_output` — assert `!contains("b.txt")` (kills `delete !` on paths filter)
- `validate_session_id_rejects_paths` — added mid-string `a..b` pin (kills `|| → &&` on `contains("..")`)

**Equivalent / soft-skip**:
- `log_line_incomplete` — `parts.len() < 3` vs `>` equivalent under fixed `git log` format
- `is_missing_blob_stderr` — OR-chain middle arms equivalent when first phrase matches
- `session_id_has_path_separator` — `/`/`\` OR→AND near-equivalent; slash/backslash tests pin each arm
- `ShadowRepo::gc` whole body — `gc → Ok(())` / success-path `!`/`&&` not unit-observable without flaky size asserts

**Notes**:
- GitNexus impact on `read_file_at` / `validate_session_id` was CRITICAL; production semantics unchanged (tests + equivalent extract/skip only).
- Display fmt for `CheckpointId` already excluded via `.cargo/mutants.toml` `exclude_re`.
- Scoped re-verify: list/read/diff/gc previously-missed mutants caught or skipped; `validate_session_id` file-scoped run **12/12 caught**.
- Remaining hard survivors: `snapshot_for_session` stderr warning `&&`/`!` guards — depend on git stderr shape; leave for a later session unless they reappear as gate blockers.
