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
    ├── cost.json                ← token usage and cost
    └── wakeup.jsonl             ← pending loop wakeup (issue #99), if any
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

`status`: `"active"` while running, `"completed"` on clean finish,
`"interrupted"` on SIGINT/SIGTERM or `recursive pause`, `"paused"`, and
`"crashed"` for every abnormal stop.

Because `"crashed"` alone cannot tell a step-budget stop from a provider
error, a failed session also records *why* (issue #111):

```json
{
  "status": "crashed",
  "finish_reason": "budget_exceeded"
}
```

`finish_reason` is the canonical `FinishReason` string
(`budget_exceeded`, `stuck:<tool>:<n>`, `transcript_limit:<chars>/<limit>`,
`provider_stop:<reason>`, `permission_denial_limit`,
`wall_clock_exceeded:<secs>`) and is absent for a session that ended
normally. `error` carries the failure text when the run aborted with an
`Err` (provider/transport/IO) instead of a finish reason. Both fields are
optional — a `.meta.json` written before they existed loads unchanged —
and both are also carried by `ExportedTranscript` (`sessions export`).

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

A crash during tool execution leaves an unanswered `tool_call` at the tail of
the transcript, which the provider rejects (invariant #8), so `recursive
resume` requires a policy for those orphan calls: `--orphans=skip` answers
each one with a synthetic `[interrupted: no result recorded]` result,
`--orphans=redo` re-executes it against the current registry, `--orphans=ask`
prompts, and `--orphans=abort` (the default when stdin is not a TTY) refuses
to resume. The answer is both seeded into the run and appended to
`transcript.jsonl`, so the next resume sees a paired transcript instead of
re-detecting the same orphans.

## Loop wakeups (issue #99)

`recursive loop` arms its next turn with `schedule_wakeup`. The pending request
is persisted to `wakeup.jsonl` in the active session directory — one JSON line
with `reason`, `prompt`, `scheduled_at_ms`, `due_at_ms` — before the loop
sleeps, and removed as soon as the wakeup fires (the request is the following
turn's goal then, not a pending record) and again when the loop ends. A process
that starts later scans the workspace's session directories for a record whose
due time has passed and restores it as the loop's first goal; a record that is
not yet due is left alone (and stays on disk) rather than blocking the operator
until the original wakeup time. The scan skips session directories whose
`.lock` is held by a live process, so a running loop's record is never stolen
by another loop started in the same workspace. Restoration consumes the record,
so a second restart cannot replay it. Loops started with `--no-session` have no
directory to write to, keep the previous in-memory-only behaviour, and do not
consume another session's record.

## Derived search index and the retrieval tools (issue #131)

`episodic_recall` scans the JSONL logs on every call. The session retrieval
family (`src/session/index.rs`, `src/session/relations.rs`,
`src/session/export.rs`, `src/knowledge/session_query.rs`, feature
`session-index`) adds a **discardable** read model instead:

- **`SessionIndex`** — SQLite FTS5 over every session of the workspace, at
  `~/.recursive/workspaces/<hash>/session-index.sqlite3` (0600, in the 0700
  workspace dir). The database carries `application_id` (`RCS1`) +
  `user_version`; a database with a foreign stamp is **rebuilt in place**, never
  migrated. A session is re-indexed only when its directory path plus the
  `transcript.jsonl` / `.meta.json` stat revision changed, and rows for vanished
  sessions are swept. The index also exposes an active-**lease** seam
  (`SessionIndex::lease` / `release`, a `TEMP` table that dies with the
  connection): a leased session is re-read on every refresh and is never
  evicted by the parsed-transcript cold-read cache's LRU. **No producer calls it
  yet** — today every refresh is driven by the stat revision, which is what
  catches a session being appended to.
- **Relation tracing** — a cross-turn compaction drains the *oldest* messages
  of the model context, summarises that prefix, and writes the summary behind a
  `compact_boundary` marker; `replacements` rebuilds that chain from the log
  (each marker folding the oldest messages no earlier marker already folded) and
  `trace_session` / `trace_event` answer "what replaced this / what did it
  replace". `session_trace` also reports sessions that start from the same
  origin transcript (recursive persists no explicit fork lineage, so transcript
  identity is the derived signal).
- **Tools** — `session_search`, `session_event_search` (eager, read-only),
  plus the deferred `session_trace`, `session_event_trace`,
  `session_event_read`. All five share one `SessionQuery`, cap results at 100,
  and enforce the **workspace boundary**: an explicit `cwd` outside the calling
  session's workspace is refused (`Error::ToolRejected`) instead of silently
  narrowed. A `cwd` inside the workspace is accepted but does not narrow
  anything — the index only ever covers the one workspace.
- **Export** — `export_session_tree` streams a ZIP (`ZipWriter::new_stream`)
  containing the root session, the sessions that declare it as their
  `derived_from`, and the session directory's attachments, plus a
  `manifest.json`. One export per session may be in flight (process-wide
  `ExportGuard`); a second is refused with `Error::ExportInProgress`. The tree
  is a stated seam as much as a feature: `derived_from` is read from
  `.meta.json`, and **no producer declares it yet** (`SessionWriter::set_derived_from`
  has no caller in-tree), so today an export is one session plus its
  attachments. There is no CLI/HTTP entry point either — `export_session_tree`
  is the library entry point a channel can call.

## Read-only workspace files over HTTP (Goal 423 W2b, issue #132)

`GET /sessions/{id}/files` serves the session's workspace read-only: `mode=lines`
(1-indexed inclusive line page, the `Read` tool's convention incl. CRLF→LF
normalization), `mode=bytes` (base64 byte window) and `mode=list` (single-level
listing that never follows symlinks). Contract: paths resolve with containment
on the *resolved form* via `resolve_within_any` (#170 precedent) and a terminal
symlink is always refused (inside the root or not); every response carries a
`version` token (`"{mtime_ms}-{len}"`, `412` on a `?version=` mismatch); a file
above `RECURSIVE_WORKSPACE_FILE_MAX_BYTES` (default 256 KiB — the same constant
as the `Read` tool) is an explicit `413` in every mode — oversized reads fail,
they never truncate; pagination out of range is an explicit `422`, never a
clamp. Owner/admin gate as on every `/sessions/:id*` route (cold-loading
included — a restart does not 404 the first read), GET-only, zero write
surface, refusals counted as `recursive_workspace_file_rejections{reason}` on
`/metrics`. Implementation: `src/http/workspace_files.rs`.

## Bounded uplink contract (Goal 423 W2a, issue #132)

The one Client→Host uplink today is `POST /sessions/{id}/messages`; its body
is read within an explicit byte budget (`RECURSIVE_UPLINK_MAX_BYTES`, default
1 MiB — byte-for-byte the router's global `DefaultBodyLimit`, which stays in
place for every other POST). An overflow is a **typed 413 `ApiError` envelope
naming `limit_bytes`**, counted as `recursive_uplink_overflows` on `/metrics`
and WARNed with the session id, limit and observed size — never axum's
plain-text rejection, never a silent clamp. The refusal fails that one
request only: session state, runtime and live SSE subscribers are untouched.
The budget is per *logical stream*; when a WS mux arrives, each logical
stream reuses the same primitive (`src/http/uplink.rs`) and the same error
shape.

## Migration

`src/migrate.rs` handles migration of legacy `.recursive/sessions/` and
`.recursive/scratchpad.json` paths to the new `~/.recursive/workspaces/<hash>/`
structure.

## Related Concepts

- [Agent Loop](agent-loop.md) — how transcripts grow per turn
- [Layer 3 — Episodic](memory/layer3-episodic.md) — searching past sessions
- [Context Compaction](compaction.md) — the four compaction contracts over this transcript (ownership, canonical rebuild, token accounting, observability)
- [Cancellation & Process Ownership](cancellation.md) — session teardown (DELETE / idle eviction / graceful shutdown) is liquidation: background jobs are cancelled and reaped before the environment is destroyed
- [Invariants](invariants.md) — Invariant #7 (transcript always saved), Invariant #8 (tool-call pairing)
