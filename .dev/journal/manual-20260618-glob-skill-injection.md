# Manual edit: glob-skill-injection (Goal 318)

**Date**: 2026-06-18
**Goal**: Implement `mode: globs` for Recursive skills — auto-inject skill body when a
tool result references a file path matching one of the skill's glob patterns.
**Files touched**:
- `src/skills.rs` — added `SkillMode::Globs`, `Skill.globs: Option<Vec<String>>`,
  extended `parse_skill_meta` to parse `globs:` YAML list, updated `skill_index` to
  show `[globs]` tag, updated `skills_for_injection` to skip Globs mode,
  added `extract_skill_body` (pub wrapper), added 2 new unit tests.
- `src/skills_injector.rs` (NEW) — `SkillInjector` struct, `glob_matches` function,
  `extract_paths` helper, 6 unit tests.
- `src/run_core.rs` — added `globs_skills: Vec<Skill>` field, created `SkillInjector`
  per run, inject matching skills as system messages after each tool-result batch.
- `src/kernel.rs` — added `globs_skills` to `AgentKernel` and `AgentKernelBuilder`,
  `AgentKernelBuilder::skills()` setter, passed to `RunCore`, propagated in
  `with_tools()`.
- `src/runtime.rs` — added `skills: Vec<Skill>` to `AgentRuntimeBuilder`, added
  `skills()` setter, thread to `AgentKernelBuilder::skills()` in `build()`.
- `src/lib.rs` — registered `pub mod skills_injector`.
- `src/tools/find_skills.rs` — added `Globs => "globs"` arm to `mode_label` match.
- All `Skill {}` struct literals across tests: added `globs: None` field.
- `.recursive/skills/arch-sync/SKILL.md` (NEW) — first consumer; fires when the agent
  edits `src/tools/**`, `src/llm/**`, `src/runtime.rs`, `src/config.rs`, etc.

**Tests added**:
- `src/skills.rs::parse_skill_meta_parses_globs_mode`
- `src/skills.rs::skills_for_injection_globs_not_injected_at_session_start`
- `src/skills_injector.rs` — 6 tests: `glob_matches_exact`, `glob_matches_double_star`,
  `glob_matches_single_star`, `glob_matches_star_at_end`, `glob_no_match_different_prefix`,
  `extract_paths_finds_file_paths`, `extract_paths_ignores_urls`

**Notes**:
- Self-improve loop (6 runs, providers: venus-deepseek × 3, minimax, deepseek-pro,
  glm-4.7) all failed due to provider issues; implemented directly.
- `SkillInjector` is created once per agent run; each Globs skill is injected at most
  once per run (tracked by name in `already_injected` HashSet).
- Glob matching supports `**` (multi-segment) and `*` (single-segment) wildcards, with
  no external dependencies — inline implementation in ~50 lines.
- `AgentRuntimeBuilder::skills()` allows callers to pass skills for Globs injection.
  Existing callers (CLI builder at `src/cli/builder.rs`) already pass skills via
  `skills_for_injection` for the system prompt; Globs injection complements this with
  mid-run injection.
