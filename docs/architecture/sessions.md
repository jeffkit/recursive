---
type: Architecture
title: Sessions — Persistence and Lifecycle
description: How agent sessions are stored, resumed, and migrated. JSONL transcript format, .meta.json structure, and the session lifecycle.
tags: [sessions, persistence, transcript, episodic]
timestamp: 2026-06-18T10:00:00Z
---

# Sessions

Source: `src/session/` (writer.rs, reader.rs, lifecycle.rs, serialize.rs)

Every agent run is a session. Sessions are persisted to disk so:
- Transcripts survive crashes (Invariant #7).
- Episodic recall can search past runs.
- The self-improve loop can inspect failures.

## Storage Layout

```
~/.recursive/workspaces/<12-char-hash>/
├── path.txt                     ← workspace path back-reference
└── sessions/<session-id>/
    ├── .meta.json               ← session metadata
    ├── transcript.jsonl         ← one message per line
    └── cost.json                ← token usage and cost
```

Session ID format: `<ISO8601-start>-<workspace-slug>` (e.g. `2026-05-28T03:41:37Z-...`)

## .meta.json

```json
{
  "session_id": "2026-05-28T03:41:37Z-...",
  "created_at": "2026-05-28T03:41:37Z",
  "status": "complete",
  "model": "deepseek-chat",
  "message_count": 116,
  "cost_usd": 0.09352
}
```

`status`: `"incomplete"` while running, `"complete"` on clean finish, `"failed"` on crash.

## transcript.jsonl

One JSON object per line — either a `Message` or a tool-call/result pair:

```json
{"id":"msg_001","role":"user","content":"# Goal 131...","timestamp":"..."}
{"id":"msg_002","role":"assistant","content":"...","tool_calls":[{"id":"tc_1","name":"Read","args":{...}}],"timestamp":"..."}
{"id":"msg_003","role":"tool","tool_call_id":"tc_1","content":"...","timestamp":"..."}
```

**Invariant #8**: `Role::Tool` messages MUST immediately follow the
`Role::Assistant` message whose `tool_calls` lists their `id`. Any compaction,
trimming, or resume replay that orphans tool results will be rejected by the
provider with HTTP 400.

## Resume

`AgentRuntime` can reload a previous session's transcript and continue from
where it left off. The self-improve flow's auto-resume step checks that a
saved transcript exists before attempting resume.

## Migration

`src/migrate.rs` handles migration of legacy `.recursive/sessions/` and
`.recursive/scratchpad.json` paths to the new `~/.recursive/workspaces/<hash>/`
structure.

## Related Concepts

- [Agent Loop](agent-loop.md) — how transcripts grow per turn
- [Layer 3 — Episodic](memory/layer3-episodic.md) — searching past sessions
- [Invariants](invariants.md) — Invariant #7 (transcript always saved), Invariant #8 (tool-call pairing)
