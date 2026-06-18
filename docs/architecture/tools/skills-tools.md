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

## Related Concepts

- [Skills System](../skills.md) — discovery, injection modes, full format
- [Tools Overview](index.md)
