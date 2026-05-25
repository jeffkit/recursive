# Goal 59 — Skill refs/ directory support

**Roadmap**: Phase 5.1 — Skill refs/ reference documents accessible via tool

**Design principle check**:
- Implemented as: extension to `src/skills.rs` + `src/tools/load_skill.rs`.
  No agent loop changes.
- Does NOT modify `agent.rs`.

## Why

Current skills are a single SKILL.md file. Real skills often need reference
documents (API specs, schema examples, cheat sheets) that are too large to
stuff into SKILL.md but should be accessible on-demand. Claude Code's skill
system supports a `refs/` subdirectory for this purpose.

## Scope (do exactly this, no more)

### 1. `src/skills.rs` — extend Skill struct + discovery

Add a `refs` field to `Skill`:

```rust
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    /// Reference documents found in <skill_dir>/refs/
    pub refs: Vec<SkillRef>,
}

#[derive(Debug, Clone)]
pub struct SkillRef {
    /// Filename without extension, e.g. "api-spec"
    pub name: String,
    /// Absolute path to the ref file
    pub path: PathBuf,
}
```

In `discover_skills`, after finding SKILL.md, also scan `<skill_dir>/refs/`
for any `.md` or `.txt` files. Populate `Skill::refs` with what's found.

### 2. `src/tools/load_skill.rs` — add `ref` parameter

Extend the `load_skill` tool to accept an optional `ref` parameter:

- If `ref` is absent: current behavior (return SKILL.md body)
- If `ref` is present: find the named ref in `skill.refs` and return its
  content (case-insensitive match on `SkillRef::name`)
- If ref not found: return error listing available refs for that skill

Update the tool's `spec()` to document the new parameter.

### 3. `src/skills.rs` — update `skill_index` to show ref count

When rendering the skill index in the system prompt, if a skill has refs,
append the count:

```
- rust-traits: Explain Rust trait design (3 refs)
```

This tells the agent that more information is available without loading it
all upfront.

### 4. Tests

- Test: `discover_skills` populates `refs` when a `refs/` directory exists
- Test: `discover_skills` handles skill with no `refs/` directory (empty vec)
- Test: `load_skill` with `ref` param returns ref content
- Test: `load_skill` with unknown ref returns error with available refs list
- Test: `skill_index` shows ref count when refs exist

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all -- --check` clean
- No regressions in existing skill tests

## Notes for the agent

- Read `src/skills.rs` for `Skill` struct and `discover_skills` function.
- Read `src/tools/load_skill.rs` for the `LoadSkill` tool implementation.
- The `refs/` directory is a **convention**: `<skill_dir>/refs/*.md` or
  `<skill_dir>/refs/*.txt`. No other formats.
- Keep `SkillRef` simple — just name + path. No frontmatter parsing for refs.
- The `name` field of `SkillRef` should be the filename stem (without
  extension), e.g. `refs/api-spec.md` → name "api-spec".
