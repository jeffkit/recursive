# Goal 62 — Skill injection modes

**Roadmap**: Phase 5.6 — Injection modes: always / trigger / manual

**Design principle check**:
- Implemented as: extension to `src/skills.rs` + `src/agent.rs` (system
  prompt assembly only). Minimal agent loop touch.
- Loop logic unchanged — only the initial system prompt composition differs.

## Why

Currently all skills are "manual" — the agent must explicitly call
`load_skill` to access them. But some skills should be:
- **always**: injected into the system prompt at session start (e.g.,
  project conventions, coding standards)
- **trigger**: loaded automatically when certain keywords appear in the
  user's goal (progressive disclosure without manual invocation)
- **manual**: current behavior — agent decides when to load

This gives skill authors control over when their content reaches the agent.

## Scope (do exactly this, no more)

### 1. `src/skills.rs` — add `mode` field + enum

```rust
#[derive(Debug, Clone, PartialEq, Default)]
pub enum SkillMode {
    /// Inject into system prompt at session start
    Always,
    /// Auto-load when trigger words appear in the user goal
    Trigger,
    /// Agent must explicitly call load_skill (current behavior)
    #[default]
    Manual,
}

pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub mode: SkillMode,
    /// Trigger words (only relevant when mode == Trigger)
    pub triggers: Vec<String>,
    // params/refs/scripts from other goals if landed
}
```

Parse from frontmatter:

```yaml
---
name: rust-conventions
description: Rust coding standards for this project
mode: always
---
```

```yaml
---
name: mcp-guide
description: MCP protocol reference
mode: trigger
triggers: mcp, protocol, server
---
```

If `mode` is absent, default to `Manual`.

### 2. `src/skills.rs` — new function `skills_for_injection`

```rust
/// Returns skill bodies that should be injected into the system prompt.
///
/// - Always-mode skills: always included
/// - Trigger-mode skills: included if any trigger word appears in `goal`
/// - Manual-mode skills: never included (they use load_skill tool)
pub fn skills_for_injection(skills: &[Skill], goal: &str) -> Vec<(String, String)> {
    // Returns Vec<(skill_name, skill_body)>
}
```

For trigger matching: case-insensitive substring match of each trigger word
against the goal text. Simple `goal.to_lowercase().contains(&trigger)`.

### 3. `src/agent.rs` or `src/main.rs` — wire injection

Find where the system prompt is assembled (search for `skill_index` usage
or where `discover_skills` result is used). After the skill index is
appended, also append the bodies of all `skills_for_injection` results.

Format:

```
=== Skill: rust-conventions (auto-loaded) ===
[skill body here]
```

Keep total injection bounded: if total injected skill content exceeds
8192 chars, truncate with a marker. This prevents "always" skills from
blowing up the context.

### 4. `src/skills.rs` — update `skill_index` to show mode

```
Available skills (use `load_skill` to activate):
- rust-conventions: Rust coding standards [always]
- mcp-guide: MCP protocol reference [trigger: mcp, protocol, server]
- python-api: Python API patterns
```

Manual-mode skills show no tag (current behavior). Always/trigger show
their mode.

### 5. Tests

- Test: `discover_skills` parses `mode: always` from frontmatter
- Test: `discover_skills` parses `mode: trigger` + `triggers` list
- Test: `discover_skills` defaults to Manual when mode absent
- Test: `skills_for_injection` returns always-mode skills regardless of goal
- Test: `skills_for_injection` returns trigger-mode skill when goal matches
- Test: `skills_for_injection` does NOT return trigger-mode when no match
- Test: `skills_for_injection` returns empty for all-manual skills
- Test: `skill_index` shows mode tags

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all -- --check` clean
- Skills without `mode` frontmatter still work as before (manual)
- No regressions in existing skill tests

## Notes for the agent

- Read `src/skills.rs` for `parse_skill_meta` and `discover_skills`.
- Read `src/main.rs` for where skills are wired into the system prompt.
  Look for `skill_index` or `discover_skills` calls.
- The `triggers` field in frontmatter is comma-separated on one line:
  `triggers: mcp, protocol, server`. Split on `,` and trim whitespace.
- For the injection in main.rs/agent.rs: you need access to the user's goal
  text. Check where `run` is called with the goal — that's where you inject.
- Keep the 8192-char cap simple: sum up all injected bodies, if over limit,
  truncate the last one that pushes it over. Add `[truncated]` marker.
- If other goals (g59/g60/g61) have added fields to Skill, preserve them.
  If not, only add `mode` and `triggers`.
