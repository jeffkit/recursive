---
type: Architecture
title: Layer 0 — Injected Context
description: Static context sources injected into the system prompt before each run — user.md, project.md, AGENTS.md, and skill index. Read-only during run.
tags: [memory, layer0, system-prompt]
timestamp: 2026-06-18T10:00:00Z
---

# Layer 0 — Injected Context

Layer 0 is frozen at session start. The agent can read it but cannot update it
mid-run. Changes take effect on the next session start.

## Sources (in injection order)

| Source | Path | Cap | Purpose |
|--------|------|-----|---------|
| User preferences | `~/.recursive/memory/user.md` | 8 KB | Personal working style, model prefs |
| Project memory | `<workspace>/.recursive/memory/project.md` | 8 KB | Agent-writable project notes |
| AGENTS.md | `<workspace>/AGENTS.md` | 16 KB | Project contract, invariants, conventions |
| Skill index | auto-generated | — | One-line per available skill |

## Updating Layer 0 Sources

- **user.md**: edit manually or use an agent to write notes about cross-project preferences.
- **project.md**: the agent can update this with `Write` to leave notes for future sessions.
- **AGENTS.md**: maintained by humans; governs all self-improve runs.
- **Skills**: automatically discovered from `.recursive/skills/` and `~/.recursive/skills/`.

## Architecture Knowledge Bundle as Layer 0

This bundle (`.dev/architecture/`) can be referenced from `project.md`:

```markdown
## Architecture reference
Architecture knowledge bundle: `.dev/architecture/index.md`
Load individual concept docs with `Read` as needed.
```

This gives the self-improve agent on-demand access to the full system map
without pre-loading everything into context.

## Related Concepts

- [Layer 0 Injection](../../architecture/layer0-injection.md) — full assembly order
- [Layer 1 — Scratchpad](layer1-scratchpad.md) — writable working memory
- [Skills System](../../architecture/skills.md) — how skill_index() is generated
- [Memory Overview](index.md) — all four layers
