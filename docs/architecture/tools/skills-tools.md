---
type: Architecture
title: Skills Tools — load_skill, find_skills, install_skill, run_skill_script
description: Tools for discovering, loading, installing, and running skill scripts. Skills are SKILL.md files with YAML frontmatter and OKF type: Skill.
tags: [tools, skills]
timestamp: 2026-06-18T10:00:00Z
---

# Skills Tools

| Tool | Source | Description |
|------|--------|-------------|
| `load_skill` | `src/tools/load_skill.rs` | Load a skill's full content into context (supports section loading) |
| `find_skills` | `src/tools/find_skills.rs` | Search available skills by name/keyword |
| `install_skill` | `src/tools/install_skill.rs` | Install a skill from the registry (downloads zip, extracts SKILL.md + refs/) |
| `run_skill_script` | `src/tools/run_skill_script.rs` | Execute a script from a skill's `scripts/` directory |

## Skill Discovery Paths

1. `<workspace>/.recursive/skills/` — project-level (higher priority)
2. `~/.recursive/skills/` — user-level
3. Override with `RECURSIVE_SKILL_PATHS=path1:path2`

## SKILL.md Format (OKF-compliant)

```yaml
---
type: Skill          # OKF required field
name: my-skill
description: "What this skill does"
mode: manual         # manual | always | trigger
triggers: rust, patch
hint: "Short hint for trigger mode"
depends_on: base-skill
params:
  - name: language
    description: Target programming language
    default: rust
---

# Skill body (Markdown)
```

All Recursive project skills are OKF-conformant. See [Skills System](../skills.md).

## Section Loading

`load_skill name=my-skill section="Recovery patterns"` loads only the named
`## Recovery patterns` section — useful for token budget management.

## `${SKILL_DIR}` Placeholder Substitution

When the `Skill` tool returns a skill's **body** or a **named section**,
every occurrence of `${SKILL_DIR}` (and the alias
`${RECURSIVE_SKILL_DIR}`) is replaced with the absolute path of the
directory containing that skill's `SKILL.md`. This lets skill authors
reference bundled scripts and refs with portable, ready-to-run paths
the agent can hand to the `Bash` tool. Ref documents are returned
verbatim and never receive this substitution — they may legitimately
contain literal `${...}` text.

Example skill body:

```markdown
Run the linter with: `bash ${SKILL_DIR}/scripts/lint.sh`
Read the spec from `${SKILL_DIR}/refs/api-spec.md`.
```

After `load_skill` returns, the placeholders are resolved to absolute
paths (no trailing slash is added — write the slash after the
placeholder so `${SKILL_DIR}/scripts/foo.sh` resolves to a well-formed
path). Substitution happens after `{{key}}` parameter substitution.
Dependency bodies inlined by a `Skill` call are not recursed into; each
dependency is substituted when it is loaded by its own `Skill` call.

## Related Concepts

- [Skills System](../skills.md) — discovery, injection modes, full format
- [Tools Overview](index.md)
