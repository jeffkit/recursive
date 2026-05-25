# Goal 61 — Skill parameters (frontmatter args)

**Roadmap**: Phase 5.4 — Skill parameters

**Design principle check**:
- Implemented as: extension to `src/skills.rs` + `src/tools/load_skill.rs`.
  No agent loop changes.
- Does NOT modify `agent.rs`.

## Why

Skills currently have only name + description in frontmatter. Real skills
need parameters — configurable values the user or agent can pass when loading
a skill to customize its behavior. For example, a "code-review" skill might
accept a `language` parameter to focus on Rust vs Python idioms.

Parameters in frontmatter let skills declare what they expect, with defaults,
so the agent knows what to pass when invoking `load_skill`.

## Scope (do exactly this, no more)

### 1. `src/skills.rs` — parse `params` from frontmatter

Add a `params` field to `Skill`:

```rust
#[derive(Debug, Clone)]
pub struct SkillParam {
    /// Parameter name, e.g. "language"
    pub name: String,
    /// Brief description
    pub description: String,
    /// Default value (None if required)
    pub default: Option<String>,
}

pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub params: Vec<SkillParam>,
    // refs/scripts from other goals if landed; otherwise omitted
}
```

Parse frontmatter like:

```yaml
---
name: code-review
description: Review code for quality issues
params:
  - name: language
    description: Target language
    default: rust
  - name: strict
    description: Enable strict mode
---
```

The `params:` field is a YAML list of objects. Parse it with the same naive
line-by-line parser already used for name/description (no serde_yaml
dependency). Strategy:

1. Detect `params:` line in frontmatter
2. Each subsequent `  - name: xxx` starts a new param
3. Following `    description: xxx` and `    default: xxx` lines fill in fields
4. Stop parsing params when you hit a line that doesn't continue the list
   (not indented, or next top-level key)

### 2. `src/tools/load_skill.rs` — template substitution

Extend `load_skill` to accept a `params` object:

```json
{
  "name": "code-review",
  "params": {
    "language": "python",
    "strict": "true"
  }
}
```

When returning the skill body:
1. For each declared param, resolve its value: use provided value, else default,
   else return error listing missing required params
2. Replace `{{param_name}}` placeholders in the skill body with resolved values
3. Return the rendered body

If `params` is not provided in the tool call, use all defaults (skip
substitution for params without defaults — leave `{{param_name}}` as-is and
note it in the output).

### 3. `src/skills.rs` — update skill_index to show params

When rendering skill index:

```
- code-review: Review code for quality issues (params: language=rust, strict)
```

Show param name + default if it has one; just name if required.

### 4. Tests

- Test: `discover_skills` parses `params` from frontmatter
- Test: `discover_skills` handles skill without `params` (empty vec)
- Test: `load_skill` with params performs template substitution
- Test: `load_skill` uses defaults when params not provided
- Test: `load_skill` errors on missing required param (no default)
- Test: `skill_index` shows params with defaults

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all -- --check` clean
- No new dependencies added (naive YAML parsing only)
- No regressions

## Notes for the agent

- Read `src/skills.rs` for `parse_skill_meta` — the naive YAML parser.
  Extend it, don't replace it with serde_yaml.
- The `params:` parsing is multi-line. Look for the pattern:
  ```
  params:
    - name: xxx
      description: yyy
      default: zzz
    - name: aaa
      description: bbb
  ```
- Template substitution: simple `str.replace("{{key}}", value)` for each
  resolved param. No need for a full template engine.
- If g59 (refs) or g60 (scripts) have NOT landed, don't add their fields
  to the Skill struct. Check what's currently there and add only `params`.
