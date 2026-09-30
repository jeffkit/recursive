# Goal 64: Skill content backing (`body` / `content` fields)

Date: 2026-07-14
Goal: #64 — Skills must be constructible from pure content (no local file),
so business docs can be delivered remotely / per-tenant. Minimal-change
approach (content-first, path fallback) chosen over the full `SkillSource`
trait, per goal's "minimal" option.

## Files touched
- `src/skills.rs`
  - `Skill.body: Option<String>` — in-memory SKILL.md content; `Some` takes
    precedence over reading `path`.
  - `SkillRef.content: Option<String>` — same for ref docs.
  - `discover_skills` sets both to `None` (back-compat: byte-identical
    behavior for directory-backed skills).
  - `skills_for_injection` (Always mode) uses body-first fallback.
  - New `skill_from_content(name, content, refs)` constructor: parses
    frontmatter/sections like discover, synthetic path
    `/virtual/skills/<name>/SKILL.md`, no scripts, keeps body in memory.
- `src/tools/load_skill.rs` — three disk reads (`:96` dep body, `:204` ref,
  `:268` main body) are now content-first, path-fallback. `LoadSkill` still
  holds `Vec<Skill>` (trait-object `SkillSource` deferred).
- Struct literals in `src/compact/reinject.rs`, `src/system_prompt.rs`,
  `src/runtime/tests.rs`, `src/http/handlers.rs`, `tests/integration.rs`
  updated with the new fields.

## Tests added
- `load_skill_body_only_returns_body_without_file` (path never touched)
- `load_skill_body_only_returns_section`
- `load_skill_body_only_ref_content` (inline ref)
- `load_skill_body_only_dependency_resolution` (dep body from memory)
- `skill_index_renders_body_only_skill`
- `skills_for_injection_always_mode_body_only` (skills.rs path exercised
  via load_skill.rs test)
- `load_skill_body_only_no_skill_dir_substitution` — documents that
  `${SKILL_DIR}` resolves to the synthetic virtual dir (remote skills must
  not use `${SKILL_DIR}` / `scripts/`).

## Notes
- Refs for remote skills: pass `SkillRef { content: Some(..), path: <synthetic> }`.
- Deferred to a follow-up goal: `SkillSource` trait / `HttpSkillSource` /
  `StaticSkillSource` and routing skill reads through `ToolTransport`.
- Gates: `cargo clippy --all-targets --all-features -- -D warnings` clean;
  `cargo fmt --all` applied; `cargo test --workspace` green.
