---
type: Architecture
title: Layer 2 — Semantic Facts
description: Named facts with tags stored as JSONL. Supports keyword search and optional vector similarity search. Persists globally and per-workspace.
tags: [memory, layer2, facts, semantic-memory]
timestamp: 2026-06-18T10:00:00Z
---

# Layer 2 — Semantic Facts

Facts are named knowledge snippets with tags and optional metadata. They live
across sessions and are surfaced in the system prompt as a bullet-list summary.

## Storage

Two scopes merged at injection time:

| Scope | Path |
|-------|------|
| Global | `~/.recursive/memory/facts.jsonl` |
| Workspace | `<workspace>/.recursive/memory/facts.jsonl` |

Each line is a fact record:
```json
{
  "id": "F1",
  "text": "Goal 133 completed: permissions config...",
  "tags": ["goal-133", "permissions"],
  "source": null,
  "created_at": "2026-05-28T06:53:27Z",
  "last_accessed": "2026-05-28T06:53:27Z",
  "access_count": 0,
  "superseded_by": null
}
```

## Tool API

| Tool | Key args | Description |
|------|----------|-------------|
| `remember_fact` | `text`, `tags` | Store a new fact |
| `recall_fact` | `query` | Search facts by keyword or semantic similarity |
| `forget_fact` | `id` | Mark a fact as superseded / deleted |
| `update_fact` | `id`, `text` | Update an existing fact |

All tools live in `src/tools/facts.rs`. `facts_summary()` in the same file
generates the injected bullet list.

## Search Backends

| Feature flag | Backend |
|---|---|
| (default) | Keyword substring search (`memory/noop.rs`) |
| `vector-memory` | SQLite + sqlite-vec (`memory/sqlite_vec.rs`) |
| `openai-embedding` | OpenAI embedding API (`memory/openai_embedding.rs`) |

The `VectorStore` trait in `src/memory/mod.rs` abstracts the backend.

## System Prompt Injection

Facts are injected at position 5 as:
```
# Facts
• [F1] Goal 133 completed: permissions config...  [tags: goal-133, permissions]
```

See [Layer 0 Injection](../../architecture/layer0-injection.md).

## Related Concepts

- [Memory Overview](index.md) — all four layers
- [Facts Tools](../../architecture/tools/facts-tools.md) — full tool API
- [Layer 1 — Scratchpad](layer1-scratchpad.md) — shorter-lived working memory
- [Layer 3 — Episodic](layer3-episodic.md) — session-level recall
