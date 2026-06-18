---
type: Architecture
title: Memory Tools — remember, recall, forget, scratchpad
description: Layer 1 working memory tools (scratchpad KV) and legacy note-store tools (remember/recall/forget). All in src/tools/memory.rs.
tags: [tools, memory, scratchpad, layer1]
timestamp: 2026-06-18T10:00:00Z
---

# Memory Tools

All tools live in `src/tools/memory.rs`.

## Legacy Note Store (remember / recall / forget)

Backed by `.recursive/memory.json`. Predates the facts system.

| Tool | Args | Description |
|------|------|-------------|
| `remember` | `text`, optional `tags` | Store a note |
| `recall` | `query`, optional `tags` | Search notes by keyword + tags |
| `forget` | `id` | Delete a note |

> **Note**: prefer [Facts Tools](facts-tools.md) for new knowledge storage.
> Facts are JSONL-backed, have richer metadata, and support vector search.

## Scratchpad KV (Layer 1)

Backed by `~/.recursive/workspaces/<hash>/scratchpad.json`.

| Tool | Args | Description |
|------|------|-------------|
| `scratchpad_set` | `key`, `value` | Upsert an entry |
| `scratchpad_get` | `key` | Read one entry |
| `scratchpad_delete` | `key` | Remove an entry |
| `scratchpad_list` | — | List all keys + truncated values |
| `working_memory` | `action`, `key`, `value` | Convenience wrapper |

Scratchpad entries are injected into the system prompt at Layer 0 position 4.
See [Layer 1 — Scratchpad](../memory/layer1-scratchpad.md).

## Related Concepts

- [Layer 1 — Scratchpad](../memory/layer1-scratchpad.md) — storage details
- [Facts Tools](facts-tools.md) — richer persistent knowledge
- [Tools Overview](index.md)
