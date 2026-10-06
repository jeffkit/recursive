---
type: Architecture
title: Session Query Tools — session_search family
description: Derived FTS5 index over past sessions, relation tracing, and the workspace-boundary rules for cross-session search.
tags: [tools, sessions, retrieval, fts5, index]
timestamp: 2026-10-05T00:00:00Z
---

# Session Query Tools

- **Rust structs**: `SessionSearch`, `SessionEventSearch`, `SessionTraceTool`,
  `SessionEventTraceTool`, `SessionEventRead`
- **Source**: `src/knowledge/session_query.rs`
- **Registered names**: `session_search`, `session_event_search`,
  `session_trace`, `session_event_trace`, `session_event_read`
- **Feature**: `session-index` (default)

## Args

| Tool | Args |
|------|------|
| `session_search` | `query` (string), `limit` (int, ≤100), `cwd` (optional) |
| `session_event_search` | `query`, `session_id` (optional), `limit`, `cwd` |
| `session_trace` | `session_id` |
| `session_event_trace` | `session_id`, `index` (int) |
| `session_event_read` | `session_id`, `index`, `context_lines` (default 2) |

## What It Returns

JSON. Searches return session headers / event hits with a snippet window and
the entry `index`; the trace tools return compaction replacements
(`first_replaced` / `summary_index` / `replaced` / `turn`) and origin copies;
`session_event_read` returns the entries around an index, each truncated at
4000 characters.

A replacement describes one compaction: it folded the `replaced` **oldest**
messages the model context still held — a message is folded when its index falls
in `[first_replaced, first_replaced + replaced)` — into the summary at
`summary_index`. The folded block is the oldest part of the log, not the
messages adjacent to the marker: the producer drains a prefix of the context and
appends the summary behind the `compact_boundary` marker, so the marker lands at
the end of the history it summarises.

## Where the data comes from

All five tools share one `SessionQuery` → one `SessionIndex`
(`src/session/index.rs`): a **discardable** SQLite FTS5 read model over
`~/.recursive/workspaces/<hash>/sessions/`. It is rebuilt in place when its
`application_id` / `user_version` stamp does not match, and re-indexes a session
only when its directory path or its stat revision changed. The index also
carries an active-lease seam (a `TEMP` table that dies with the connection) for
a live writer; no producer calls it yet, so the stat revision is what keeps the
index current. See [Sessions](../sessions.md) for the layout.

## Workspace boundary

Cross-session search only looks inside the calling session's workspace. An
explicit `cwd` outside it is **refused** (`Error::ToolRejected`) rather than
narrowed; `..` is resolved lexically before containment is checked. The
parameter is a claim to check, not a filter: the index covers exactly one
workspace, so a `cwd` at or below it is accepted but does not narrow the
results. Results are capped at 100.

## Related Concepts

- [Episodic Tool](episodic-tool.md) — `episodic_recall`, the scan-based
  predecessor (kept: it also reads single sessions without an index)
- [Sessions](../sessions.md) — storage layout, derived index, export
- [Tools Overview](index.md)
