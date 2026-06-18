---
type: Architecture
title: Facts Tools — remember_fact, recall_fact, forget_fact, update_fact
description: Layer 2 semantic memory tools. Store tagged facts as JSONL with optional vector search. Global and workspace-scoped.
tags: [tools, facts, memory, layer2, semantic-memory]
timestamp: 2026-06-18T10:00:00Z
---

# Facts Tools

Source: `src/tools/facts.rs`.
Backed by [Layer 2 — Semantic Facts](../memory/layer2-facts.md).

## Tool API

| Tool | Args | Description |
|------|------|-------------|
| `remember_fact` | `text`, `tags` (list) | Store a new semantic fact |
| `recall_fact` | `query`, optional `tags` | Search by keyword or vector similarity |
| `forget_fact` | `id` | Mark fact as superseded |
| `update_fact` | `id`, `text` | Update existing fact text |

## Two Scopes

Facts are stored in two files that are merged on `recall_fact`:

- **Global** (`~/.recursive/memory/facts.jsonl`) — cross-workspace knowledge
- **Workspace** (`<workspace>/.recursive/memory/facts.jsonl`) — project-specific

Pass `global: true` to `remember_fact` to store in the global scope.

## Vector Search (optional)

When the `vector-memory` Cargo feature is enabled, `recall_fact` uses cosine
similarity over embeddings. Default is keyword substring search.

See `src/memory/` for the `VectorStore` trait and implementations.

## System Prompt Injection

`facts_summary()` generates a bullet list injected at Layer 0 position 5.
See [Layer 0 Injection](../layer0-injection.md).

## When to Use Facts vs Scratchpad

| Use case | Tool |
|----------|------|
| Multi-session knowledge ("this project uses X pattern") | `remember_fact` |
| In-run plan state, current step | `scratchpad_set` |
| Cross-project preferences | `remember_fact --global` |

## Related Concepts

- [Layer 2 — Semantic Facts](../memory/layer2-facts.md) — storage format
- [Memory Tools](memory-tools.md) — scratchpad + legacy notes
- [Tools Overview](index.md)
