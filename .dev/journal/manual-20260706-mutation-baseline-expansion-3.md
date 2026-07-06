# Manual edit: mutation-baseline-expansion-3

**Date**: 2026-07-06
**Goal**: Continue advancing the full mutation test baseline by targeting high mutant-to-test ratio files and adding targeted tests to kill uncovered mutants.

## Files touched & tests added

| File | Tests before | Tests after | Key mutants killed |
|------|-------------|-------------|-------------------|
| `src/tools/web_fetch.rs` | 17 | 20 | `|| → &&` in `is_private_ip` IPv4 arm; `& → |` and `& → ^` in IPv6 segment masks (fc00::/7 ULA, fe80::/10 link-local); `is_private_ip → bool with true/false` |
| `src/tools/registry.rs` | 13 | 37 | `register_with_aliases`, `find_by_name` via alias, `register_mut_with_aliases`, `specs_partitioned` (eager vs deferred), `split_eager_deferred` (hint extraction), `is_deferred_spec`, `retain_tools` alias cleanup, `with_touched_files`/`clear_touched_files`, `with_read_file_state`, `permission_mode`, `with_permissions`, `permissions_config`, `is_plan_mode`, `with_permission_hook`/`clear_permission_hook`, `with_policy`/`policy`, `with_headless`, fork isolation, names vs aliases |
| `src/tools/glob.rs` | 9 | 21 | `match_seg_inner → bool` replacements; `delete match arm (None,None)`, `(Some(&'*'), _)`, `(Some(&'?'), Some(_))`; `replace match guard pc==tc with true/false`; `delete ! in match_seg_inner` line 40; `delete ! in match_path` line 64; `glob_matches → bool` replacements; `relativise → String::new()/xyzzy` |
| `src/tools/dispatch.rs` | 7 | 12 | `args_preview_for_permission → String::new()/xyzzy`; `> with ==`, `> with <`, `> with >=` (truncation boundary); `rw_root_allows_both_read_and_write` (tier gate) |
| `src/paths.rs` | 8 | 12 | `delete ! in user_sessions_dir` line 67; `user_shadow_git_dir/user_scratchpad_path → Ok(Default)` function replacements; `workspace_hash_from_canonical → String::new()/xyzzy` |
| `src/tools/estimate_tokens.rs` | 6 | 7 | `is_deferred → bool with false` |

## Total new tests added this session: 36

## Commits
- `f4b87ff` test: expand web_fetch is_private_ip coverage (17 → 20 tests)
- `994dd5c` test: massively expand ToolRegistry coverage (13 → 37 tests)
- `4577904` test: expand Glob tool coverage (9 → 21 tests)
- `80965ee` test: expand dispatch.rs coverage (7 → 12 tests)
- `19aa459` test: expand paths.rs coverage (8 → 12 tests)
- `7282cd4` test: add is_deferred=true assertion for EstimateTokens

## Quality gates
- `cargo test --workspace`: all pass
- `cargo clippy --all-targets --all-features -- -D warnings`: clean

## Notes
- Files with 0 reported mutants (`bash.rs`, `fact_store.rs`, `compaction.rs`, `background.rs`, `retry.rs`, `cost.rs`, etc.) appear to be excluded from the mutants index or have no testable logic. Skipped.
- The `ToolRegistry` expansion (13 → 37 tests) is the largest single gain: covers 24 new mutants across builder pattern, deferred tool partitioning, alias management, and permission/policy hooks.
- The `glob.rs` expansion targets the internally implemented glob matcher's recursive descent functions, covering all four match arm deletion mutants and both `delete !` mutations.
- Equivalent mutants noted: none new this session.
