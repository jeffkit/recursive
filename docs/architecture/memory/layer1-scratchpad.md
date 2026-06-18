---
type: Architecture
title: Layer 1 — Scratchpad (Working Memory)
description: Mutable KV working memory. Persists across sessions. Use for plan state, intermediate results, and cross-turn notes.
tags: [memory, layer1, scratchpad, working-memory]
timestamp: 2026-06-18T10:00:00Z
---

# Layer 1 — Scratchpad

A mutable key-value store that persists across sessions. Use it to jot down
plan state, track progress, and share information across turns.

## Storage

Path: `~/.recursive/workspaces/<12-char-hash>/scratchpad.json`

Format:
```json
{
  "entries": [
    { "key": "plan", "value": "Migration plan:\n1. Update imports\n2. ..." }
  ]
}
```

## Tool API

| Tool | Description |
|------|-------------|
| `scratchpad_set` | `key`, `value` — upsert an entry |
| `scratchpad_get` | `key` — retrieve one entry |
| `scratchpad_delete` | `key` — remove an entry |
| `scratchpad_list` | — list all keys with truncated values |
| `working_memory` | convenience wrapper (read/write combined) |

All tools are in `src/tools/memory.rs`.

## Injection into System Prompt

Scratchpad entries are injected at position 4 of the system prompt
(see [Layer 0 Injection](../../architecture/layer0-injection.md)):

```
# Scratchpad
plan: Migration plan:
  1. Update imports
  ...
```

## Best Practices

- Use scratchpad for **plan state** across turns (e.g., `plan`, `current_step`).
- Keep entries small — large values bloat the system prompt.
- Delete entries when done: `scratchpad_delete key=plan`.

## Related Concepts

- [Memory Overview](index.md) — all four layers
- [Memory Tools](../../architecture/tools/memory-tools.md) — full tool API
- [Layer 2 — Facts](layer2-facts.md) — for longer-lived semantic knowledge
