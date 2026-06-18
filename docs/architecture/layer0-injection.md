---
type: Architecture
title: Layer 0 — System Prompt Assembly
description: How the system prompt is assembled from multiple memory sources before each agent run. Injection order, size limits, and the role of each source.
tags: [layer0, system-prompt, injection, memory, config]
timestamp: 2026-06-18T10:00:00Z
---

# Layer 0 — System Prompt Assembly

`src/config.rs` and `src/cli/builder.rs` assemble the system prompt before
each agent run by concatenating several sources in a stable order. The order
is intentionally **stable → volatile** to maximise prefix-cache hits.

## Injection Order

```
1. # User preferences       ← ~/.recursive/memory/user.md        (≤ 8 KB)
2. # Project memory         ← <workspace>/.recursive/memory/project.md (≤ 8 KB)
3. # Memory                 ← .recursive/memory.json notes (legacy)
4. # Scratchpad             ← working memory KV entries
5. # Facts                  ← facts.jsonl entries (global + workspace merged)
6. # Episodic recall        ← recent session summaries
7. default_system_prompt()  ← hardcoded working principles + TodoWrite guide
8. # Project context        ← AGENTS.md at workspace root         (≤ 16 KB)
9. # Available skills       ← skill_index() string
```

Each section is only included if content is non-empty.

## Size Limits

| Source | Cap | Location |
|--------|-----|----------|
| `user.md` | 8 KB | `MAX_MEMORY_FILE_SIZE` in `src/config.rs` |
| `project.md` | 8 KB | `MAX_MEMORY_FILE_SIZE` |
| `AGENTS.md` | 16 KB | `MAX_PROJECT_CONTEXT_SIZE` |
| Facts summary | token budget | `src/tools/facts.rs::facts_summary()` |

Files that exceed their cap are truncated with a `[…truncated]` marker.

## Key Functions

| Function | File | Purpose |
|----------|------|---------|
| `load_user_memory()` | `src/config.rs` | Read `~/.recursive/memory/user.md` |
| `load_project_memory()` | `src/config.rs` | Read workspace `project.md` |
| `load_project_context()` | `src/config.rs` | Read `AGENTS.md` |
| `default_system_prompt()` | `src/config.rs` | Hardcoded working principles |
| `skill_index()` | `src/skills.rs` | One-line summary per available skill |
| `facts_summary()` | `src/tools/facts.rs` | Recent facts as bullet list |
| `episodic_recall_summary()` | `src/tools/episodic_recall.rs` | Last session summary |

## Adding a New Layer 0 Source

1. Write a `load_*()` function in `src/config.rs` that returns `Option<String>`.
2. Register it in the system prompt builder in `src/cli/builder.rs` (or `src/http/handlers.rs`).
3. Add a section header so the agent knows which section it's reading.

**Do not branch inside `AgentRuntime::run`** to add context — use the system
prompt builder (Invariant #1).

## Architecture Knowledge Bundle as Layer 0

This architecture bundle (`.dev/architecture/`) can itself be injected as
Layer 0 by referencing `index.md` in `project.md`:

```markdown
## Architecture Reference
See `.dev/architecture/index.md` for the full architecture knowledge bundle.
Load individual concept docs with `Read` on demand.
```

## Related Concepts

- [Memory Overview](memory/index.md) — full four-layer memory system
- [Layer 0 — Injected Context](memory/layer0-injected-context.md) — user.md and project.md details
- [Layer 1 — Scratchpad](memory/layer1-scratchpad.md) — working memory injected at position 4
- [Layer 2 — Facts](memory/layer2-facts.md) — facts injected at position 5
- [Layer 3 — Episodic](memory/layer3-episodic.md) — episodic recall at position 6
- [Skills System](skills.md) — skill_index() at position 9
- [Agent Loop](agent-loop.md) — how the assembled system prompt is used
