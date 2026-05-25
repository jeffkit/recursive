# Goal 64 — Skill composition (depends_on)

**Roadmap**: Phase 5.5 — Skill composition

**Design principle check**:
- Implemented as: extension to `src/skills.rs` + `src/tools/load_skill.rs`.
  No agent loop changes.
- Does NOT modify `agent.rs`.

## Why

Some skills build on others. A "rust-testing" skill might depend on
"rust-conventions" being loaded first. Without composition, the agent
must figure out dependencies manually. The `depends_on` mechanism
auto-loads prerequisite skills when a skill is loaded.

## Scope (do exactly this, no more)

### 1. `src/skills.rs` — add `depends_on` field

```yaml
---
name: rust-testing
description: Testing patterns for Rust
depends_on: rust-conventions
---
```

Or multiple:
```yaml
---
name: advanced-mcp
description: Advanced MCP patterns
depends_on: mcp-guide, rust-conventions
---
```

Parse `depends_on` as a comma-separated list of skill names. Add to Skill:

```rust
pub struct Skill {
    // ... existing fields ...
    /// Skills that should be auto-loaded before this one
    pub depends_on: Vec<String>,
}
```

### 2. `src/tools/load_skill.rs` — resolve dependencies

When `load_skill` is called:
1. Find the requested skill
2. Check its `depends_on` list
3. For each dependency, recursively load it (if not already loaded)
4. Concatenate: dependency bodies first, then the requested skill body
5. Mark loaded skills to prevent circular dependencies

Output format:
```
=== Dependency: rust-conventions ===
[body of rust-conventions]

=== Skill: rust-testing ===
[body of rust-testing]
```

**Circular dependency protection**: maintain a visited set. If a cycle is
detected, skip the circular dependency and add a warning:
`[WARNING: circular dependency detected, skipping {name}]`

**Depth limit**: max 3 levels of dependency resolution. Beyond that,
return error suggesting the skill tree is too deep.

### 3. `src/skills.rs` — show depends_on in skill_index

```
- rust-testing: Testing patterns for Rust (depends_on: rust-conventions)
```

### 4. Tests

- Test: `discover_skills` parses single `depends_on` value
- Test: `discover_skills` parses comma-separated `depends_on` list
- Test: `discover_skills` handles missing `depends_on` (empty vec)
- Test: `load_skill` resolves single dependency
- Test: `load_skill` resolves multi-level dependencies (A → B → C)
- Test: `load_skill` detects circular dependency (A → B → A)
- Test: `load_skill` respects depth limit
- Test: `skill_index` shows depends_on

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all -- --check` clean
- Dependencies are resolved automatically
- Circular dependencies are detected and handled gracefully

## Notes for the agent

- Read `src/skills.rs` for the Skill struct and `parse_skill_meta`.
- Read `src/tools/load_skill.rs` for the LoadSkill tool.
- The `depends_on` parsing: look for `depends_on:` line in frontmatter,
  split value on `,`, trim each entry.
- For dependency resolution in LoadSkill: you already have access to
  `self.skills` (Arc<Vec<Skill>>). Find each dependency by name.
- The "already loaded" tracking doesn't persist between tool calls
  (no session state in tools). Instead, track within a single load_skill
  invocation using a local HashSet<String> passed through recursive calls.
- Keep the output readable: clear markers between dependency content and
  the main skill content.
