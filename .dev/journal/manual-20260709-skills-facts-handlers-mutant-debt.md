# Manual edit: skills-facts-handlers-mutant-debt

**Date**: 2026-07-09
**Goal**: Start clearing the largest remaining agent mutant-debt files (`skills.rs`, `tools/facts.rs`, `http/handlers.rs`) with high-ROI unit pins + soft-skips for non-observable wrappers.

**Files touched**:
- `src/http/handlers.rs` — 4 new tests + soft-skip on `health` / `generate_session_id` / `openapi_spec` / `list_slash_commands`
- `src/skills.rs` — 8 new tests (discover skip paths, extract_body, trigger case, meta edges, index globs/depends_on, rb/js + exec scripts)
- `src/tools/facts.rs` — strengthened `tokenize` whitespace/punct split assertions
- `.dev/mutant-debt-20260709-agent.md` — queue notes

**Tests added**:
- handlers: `parse_permission_mode_all_variants`, `map_agent_event_tool_result_success_flag`, `map_agent_event_suppresses_non_sse_variants`, `sse_message_from_canonical_filters_system_tool_and_empty`
- skills: `discover_skills_skips_invalid_entries`, `extract_body_strips_frontmatter_and_trims`, `skills_for_injection_trigger_case_insensitive`, `parse_skill_meta_unknown_mode_and_empty_triggers`, `parse_skill_meta_globs_strips_quotes_skips_empty`, `skill_index_shows_depends_on_and_globs_tag`, `discover_scripts_rb_js_extensions`, `discover_scripts_includes_executable_without_ext`
- facts: extended `test_j_tokenize_and_stop_words`

**Notes**:
- GitNexus: `discover_skills` CRITICAL / `tokenize` HIGH — changes are tests (+ soft-skip on non-logic wrappers) only; production semantics unchanged.
- Full-file `agent-mutants.sh` on facts (~200) / skills (~554) is too slow for one session; ROI-scoped re-verify is the gate for this commit.
- Remaining debt: async HTTP handlers, full skills/facts gate-0 sweeps.
