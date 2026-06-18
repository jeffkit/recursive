---
type: Architecture
title: Skills System
description: How skills are discovered, parsed, injected, and invoked. SKILL.md format (OKF-conformant), injection modes, and the skill_index().
tags: [skills, architecture, okf]
timestamp: 2026-06-18T10:00:00Z
---

# Skills System

Source: `src/skills.rs`

A skill is a reusable knowledge unit — a `SKILL.md` file with YAML frontmatter
and a Markdown body. Skills are loaded into context on demand and can carry
executable scripts.

## SKILL.md Format (OKF-conformant)

```yaml
---
type: Skill          # OKF required field — all project skills have this
name: rust-patch-discipline
description: "Surgical-edit guide for Rust files using V4A apply_patch"
mode: manual         # manual (default) | always | trigger
triggers: apply_patch, V4A, patch rejected
hint: "Use when patching Rust files"
depends_on: base-skill, other-skill
params:
  - name: language
    description: Target language
    default: rust
---

# Body (Markdown)
```

The `type: Skill` field makes all Recursive skills OKF v0.1 conformant.

## Injection Modes

| Mode | Behaviour |
|------|-----------|
| `always` | Injected into every system prompt automatically |
| `trigger` | Injected when the current message matches a `triggers` keyword |
| `manual` | Only loaded when the agent calls `load_skill name=X` |

## Discovery Paths (in priority order)

1. `<workspace>/.recursive/skills/` — project-level
2. `~/.recursive/skills/` — user-level
3. `RECURSIVE_SKILL_PATHS=path1:path2` — explicit override

## `skill_index()`

Returns a compact one-line-per-skill list injected at Layer 0 position 9:

```
rust-patch-discipline: Surgical-edit guide for Rust files using V4A apply_patch
recursive-loop: Loop orchestrator for the Recursive self-improvement workflow
```

This gives the agent a menu of available skills without loading their full content.

## `skills_for_injection()`

Returns skills with `mode: always` or triggered skills that match the current
message. These are prepended to the system prompt automatically.

## Section Loading

`load_skill name=rust-patch-discipline section="Recovery patterns"` loads only
the named `## ...` section — essential for token budget management when skills
are large.

## Project Skills

| Skill | Mode | Description |
|-------|------|-------------|
| `rust-patch-discipline` | manual | V4A apply_patch guide |
| `recursive-loop` | manual | Self-improve loop orchestrator |
| `gitnexus-guide` | manual | GitNexus tools reference |
| `gitnexus-impact-analysis` | manual | Blast-radius analysis |
| `gitnexus-debugging` | manual | Bug tracing with GitNexus |
| `gitnexus-exploring` | manual | Codebase exploration |
| `gitnexus-refactoring` | manual | Safe refactoring with GitNexus |
| `gitnexus-cli` | manual | GitNexus CLI commands |

## Related Concepts

- [Layer 0 Injection](layer0-injection.md) — where skill_index() is injected
- [Skills Tools](tools/skills-tools.md) — load_skill, find_skills, install_skill
- [Memory Overview](memory/index.md)
