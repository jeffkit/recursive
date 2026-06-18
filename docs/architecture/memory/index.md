---
type: Index
title: Memory System Overview
description: Recursive's four-layer memory architecture — from injected context to episodic recall.
tags: [memory, architecture]
timestamp: 2026-06-18T10:00:00Z
---

# Memory System

Recursive implements a four-layer memory architecture. Each layer has different
persistence, volatility, and access patterns.

```
Layer 0  Injected Context    Static at session start. Read-only during run.
Layer 1  Working Memory      Mutable KV. Persists across sessions.
Layer 2  Semantic Facts      Named facts with tags. Persists across sessions.
Layer 3  Episodic Memory     Session transcripts. Searchable after the fact.
```

## Layers

* [Layer 0 — Injected Context](layer0-injected-context.md) - user.md, project.md, AGENTS.md, skills
* [Layer 1 — Scratchpad](layer1-scratchpad.md) - working memory KV, scratchpad_set/get/delete
* [Layer 2 — Facts](layer2-facts.md) - remember_fact/recall_fact, JSONL + optional vector search
* [Layer 3 — Episodic](layer3-episodic.md) - session transcript search, episodic_recall

## Storage Paths

```
~/.recursive/                              # global (RECURSIVE_HOME overrides)
├── memory/
│   ├── user.md                            # Layer 0: user preferences
│   └── facts.jsonl                        # Layer 2: global facts
└── workspaces/<12-char-hash>/
    ├── path.txt
    ├── scratchpad.json                    # Layer 1: working memory
    └── sessions/<session-id>/
        ├── .meta.json
        ├── transcript.jsonl               # Layer 3: episodic
        └── cost.json

<workspace>/.recursive/
├── memory/
│   ├── project.md                         # Layer 0: project memory
│   └── facts.jsonl                        # Layer 2: workspace facts
└── skills/                               # Bundled skills
```

## Key Design Principles

- **File-first**: all memory is human-readable, git-diffable files.
- **Agent self-managed**: tools allow read/write during a run; next run sees updated state.
- **Freeze-then-inject**: Layer 0 is frozen at session start, Layers 1-3 are live.
- **Scope isolation**: global (`~/.recursive/`) vs workspace-scoped (`.recursive/`).

## Related Concepts

- [Layer 0 Injection](../layer0-injection.md) — assembly order and size limits
- [Facts Tools](../tools/facts-tools.md) — Layer 2 tool API
- [Memory Tools](../tools/memory-tools.md) — Layer 1 tool API
- [Episodic Tool](../tools/episodic-tool.md) — Layer 3 tool API
- [Sessions](../sessions.md) — how Layer 3 data is created
