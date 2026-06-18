---
type: Architecture
title: Episodic Tool — episodic_recall
description: Search past session transcripts for relevant content. Powers Layer 3 episodic memory recall.
tags: [tools, episodic, memory, layer3]
timestamp: 2026-06-18T10:00:00Z
---

# Episodic Tool

- **Rust struct**: `EpisodicRecall`
- **Source**: `src/tools/episodic_recall.rs`
- **Registered name**: `episodic_recall`

## Args

| Arg | Type | Description |
|-----|------|-------------|
| `query` | string | Keyword to search for in past sessions |
| `limit` | integer (optional) | Max sessions to return |
| `since` | string (optional) | ISO 8601 date filter |

## What It Returns

Relevant excerpts from past `transcript.jsonl` files, grouped by session.
Useful for "what did I try last time?" before starting a new approach.

## System Prompt Injection

`episodic_recall_summary()` auto-generates a brief recent-session summary
injected at Layer 0 position 6. See [Layer 0 Injection](../layer0-injection.md).

## Related Concepts

- [Layer 3 — Episodic](../memory/layer3-episodic.md) — storage format
- [Sessions](../sessions.md) — how sessions are created and stored
- [Tools Overview](index.md)
