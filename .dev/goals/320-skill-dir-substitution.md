# Goal 320 — `${SKILL_DIR}` substitution in the `Skill` tool

## Why

Recursive's skill system exposes four tools to the LLM (`Skill`,
`find_skills`, `run_skill_script`, `install_skill`). Reviewing the
Claude Code reference (`~/Downloads/fake-cc/src/tools/SkillTool` and
`src/skills/loadSkillsDir.ts`) shows that Claude Code collapses all of
this into a **single `Skill` tool**: skill scripts are executed by the
existing Bash tool, with the skill body referencing bundled scripts via
a `${CLAUDE_SKILL_DIR}` placeholder that is substituted with the skill's
own directory at load time.

To converge on that model, `run_skill_script` will be removed (Goal 321)
and `find_skills` will be removed (Goal 319). Before those can land
safely, the `Skill` tool must support the same path-substitution
mechanism so skill authors can write:

```
Run the linter with: `bash ${SKILL_DIR}/scripts/lint.sh`
```

and have the agent invoke the existing `Bash` tool with the resolved
absolute path. This goal adds that substitution. It is intentionally
landed **before** Goals 319/321 so the replacement mechanism exists
before the old tools are removed.

## Goal

When the `Skill` tool (`src/tools/load_skill.rs`) returns a skill body
or a named section, substitute every occurrence of `${SKILL_DIR}` (and
the alias `${RECURSIVE_SKILL_DIR}`) with the absolute path of the
directory containing the skill's `SKILL.md`. Substitution happens after
parameter (`{{key}}`) substitution and only on the body / section
content paths (not on `ref` document content — refs are arbitrary
documents and may legitimately contain literal `${...}` text).

## Design

### 1. Substitution helper

In `src/tools/load_skill.rs`, add a small private helper:

```rust
/// Substitute `${SKILL_DIR}` and `${RECURSIVE_SKILL_DIR}` with the
/// absolute path of the directory containing the skill's SKILL.md.
/// Trailing slashes are stripped so the placeholder reads naturally
/// both as `${SKILL_DIR}` (bare) and `${SKILL_DIR}/scripts/foo.sh`.
fn substitute_skill_dir(content: &str, skill: &Skill) -> String {
    let Some(skill_dir) = skill.path.parent() else {
        return content.to_string();
    };
    let dir = skill_dir.to_string_lossy().to_string();
    content
        .replace("${SKILL_DIR}", &dir)
        .replace("${RECURSIVE_SKILL_DIR}", &dir)
}
```

`skill.path` is already absolute (set by `discover_skills` from the
search paths, which are absolute). Do **not** canonicalize — the search
paths may legitimately contain symlinks the user wants preserved.

### 2. Apply in `execute`

In the body-return path (the final `let body = …` block near the end of
`execute`), apply `substitute_skill_dir` to `rendered` before dependency
concatenation. Do the same in the section-return path (`rendered` for
sections). Do **not** apply it in the `ref`-return path.

### 3. Ordering

Substitution order inside `execute`:
1. Resolve `params` and do `{{key}}` replacement (existing).
2. `substitute_skill_dir` (NEW).
3. Dependency concatenation (existing, unchanged — deps are separate
   skill bodies and get their own substitution when they are loaded by
   a future `Skill` call; do not recurse here).

## Scope (do exactly this, no more)

Files to touch:
- `src/tools/load_skill.rs` — add `substitute_skill_dir`, apply on body
  and section paths.
- `docs/architecture/tools/skills-tools.md` — document the
  `${SKILL_DIR}` placeholder in the `Skill` tool section (one short
  paragraph + example). If the file does not exist in the baseline,
  skip the doc change (do not create new docs files).

Do NOT touch:
- `src/tools/run_skill_script.rs`, `src/tools/find_skills.rs` (other
  goals own those).
- `src/skills.rs` (Skill struct / parser unchanged).
- `src/tools/registry.rs`, `src/tools/mod.rs` (no registration change).
- Any file under `.dev/` other than this goal file.

## Acceptance

1. `cargo test --workspace` green, including new unit tests:
   - `load_skill_substitutes_skill_dir_in_body` — a skill whose body
     contains `bash ${SKILL_DIR}/scripts/lint.sh` returns the literal
     with `${SKILL_DIR}` replaced by the skill directory's absolute
     path.
   - `load_skill_substitutes_recursive_skill_dir_alias` — the
     `${RECURSIVE_SKILL_DIR}` alias is also replaced.
   - `load_skill_substitutes_skill_dir_in_section` — a named section
     containing `${SKILL_DIR}` is substituted when loaded via
     `section`.
   - `load_skill_does_not_substitute_skill_dir_in_ref` — loading a
     `ref` document whose content contains the literal text
     `${SKILL_DIR}` returns it unchanged.
   - `load_skill_no_skill_dir_when_path_has_no_parent` — degenerate
     case returns content unchanged (no panic).
2. `cargo clippy --all-targets --all-features -- -D warnings` clean.
3. `cargo fmt --all` no diff.
4. Existing `load_skill` tests still pass (param substitution, deps,
   case-insensitive name lookup, etc.).

## Notes for the agent

- `skill.path.parent()` returns `Option<&Path>`; handle `None`
  defensively (return content unchanged) rather than `unwrap`-ing —
  Invariant #5 forbids `unwrap` in non-test code.
- Use `.to_string_lossy()` for the path → string conversion; skill
  directories are not expected to contain non-UTF8, but `to_string_lossy`
  is the clippy-acceptable way to convert `Path` → `String` without
  `unwrap`.
- Do not invent a `canonicalize` step — it adds syscalls and can break
  on missing paths in tests. The search-path machinery already yields
  absolute paths.
- Keep the helper private (`fn`, not `pub`) — it is an internal
  implementation detail of the tool.
- **DO NOT modify files outside the scope list.** In particular do not
  touch `run_skill_script.rs` or `find_skills.rs`; they are owned by
  Goals 319 and 321.
