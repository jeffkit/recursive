---
type: Architecture
title: Layer 3 — Episodic Memory
description: Session transcript store. Enables cross-session recall of what happened in past runs. Backed by JSONL session files.
tags: [memory, layer3, episodic, sessions]
timestamp: 2026-06-18T10:00:00Z
---

# Layer 3 — Episodic Memory

Episodic memory lets the agent recall what happened in previous sessions —
what was attempted, what succeeded, what failed.

## Storage

Path: `~/.recursive/workspaces/<hash>/sessions/<session-id>/transcript.jsonl`

Each line is a message:
```json
{"id":"msg_001","role":"user","content":"# Goal 131 ...","timestamp":"2026-05-28T03:41:37Z"}
{"id":"msg_002","role":"assistant","content":"...","tool_calls":[...],"timestamp":"..."}
```

Session metadata lives in `.meta.json`:
```json
{
  "session_id": "2026-05-28T03:41:37Z-...",
  "status": "complete",
  "model": "deepseek-chat",
  "message_count": 116,
  "cost_usd": 0.09352
}
```

## Tool API

| Tool | Description |
|------|-------------|
| `episodic_recall` | Search past sessions by keyword/date range; returns relevant excerpts |

`episodic_recall_summary()` in `src/tools/episodic_recall.rs` generates a
brief summary of the most recent session for system prompt injection.

## System Prompt Injection

Most-recent session summary injected at position 6:
```
# Episodic recall
Session 2026-05-28: Implemented Goal 131 (permissions). Completed.
Key actions: edited src/permissions.rs, added 12 tests.
```

## Relationship to Compaction

When a transcript grows too large, `Compactor` (in `src/compact.rs`) summarises
it. The compacted summary captures `kept_facts` and `next_steps` — essentially
a manual episodic record written into the running session.

See [Agent Loop](../../architecture/agent-loop.md) for compaction details.

## Related Concepts

- [Memory Overview](index.md) — all four layers
- [Episodic Tool](../../architecture/tools/episodic-tool.md) — recall tool API
- [Sessions](../../architecture/sessions.md) — session lifecycle and JSONL format
- [Layer 2 — Facts](layer2-facts.md) — for structured semantic facts
