---
type: Architecture
title: Persistence — Channel × Granularity Coverage Matrix
description: Which channel persists what, at which granularity, where the records land, and what retention covers. Includes the #147 redesign acceptance-criteria → main-landing map.
tags: [persistence, sessions, storage, retention, agui, issue-147]
timestamp: 2026-10-23T00:00:00Z
---

# Persistence — channel × granularity matrix (issue #147)

Source: `src/session/writer.rs` (`SessionPersistenceSink`), `src/session/`
(native `SessionWriter`), `src/http/session_mirror.rs`, `src/storage/`
(flat `StorageBackend` store + retention), `src/http/agui.rs`.

Two stores coexist and never double-write the same rows:

- **Native session dir** — `~/.recursive/workspaces/<hash>/sessions/<sid>/transcript.jsonl`,
  appended row-by-row by the kernel through [`SessionPersistenceSink`]
  (see [sessions.md](sessions.md) for the layout). This is the crash-window
  record: rows land as the kernel appends them, so a SIGKILL mid-run keeps
  every completed step.
- **Flat `StorageBackend` store** — `<workspace>/.recursive/sessions/<key>.jsonl`
  plus the `memory/` key/value space (`session-meta/`, `session-usage/`,
  tombstones). Written per-turn by REST session runs, at run end by trigger
  runs; this is the store the HTTP retention reaper sweeps.

## The matrix

| Channel | Granularity | Writer / mount | Retention |
|---|---|---|---|
| CLI run / repl / loop | Event-level (per row) | `SessionWriter::create_with_tools` + `SessionPersistenceSink` (`crates/recursive-cli/src/main.rs`; loop mirrors run_once's wiring minus the stdout sink) | Native dir only — no flat-store rows, nothing for the reaper to sweep |
| CLI resume | Event-level (per row, re-attached) | `SessionWriter::open_existing` + same sink re-mounted onto the resumed runtime | Native dir only |
| TUI | Event-level (per row) | `backend.rs` creates the `SessionWriter` and wires `SessionPersistenceSink` into the runtime's event sink | Native dir only |
| REST session (create / fork / cold load / trigger-shared parts) | **Dual-track**: native mirror event-level **+** flat per-turn (authoritative) | `session_mirror::open_live_writer` → `SessionPersistenceSink` (`http/handlers.rs`), best-effort; flat side via `persist_transcript_per_turn` (runtime builder, #92) — this transcript is what cold load reads, so it is authoritative | Flat side swept by the HTTP retention reaper; tombstone + meta/usage KV records go with it (#214 report semantics) |
| AG-UI thread | Event-level (per row); run row has a single writer, teardown writes only status/usage | `agui_session::open_thread_writer` → `SessionPersistenceSink` (`http/agui.rs`); flat transcript stays teardown-only | Flat teardown rows swept by the reaper; native thread transcript follows the native lifecycle |
| Trigger run | Run-level (one `save_transcript` at run end, best-effort) | key `trigger-run/<uuid>` → `sessions/trigger-run/<uuid>.jsonl` in the flat store (`http/triggers.rs`) | **Under retention** — the key contains `/`, so it lives in a flat-store subdirectory; the sweep descends one level for exactly this shape (issue #147) |
| ACP | **None** | `acp/session.rs::with_persistence_dir` exists but has zero callers — the mechanism is dormant. Wiring it needs per-row semantics *and* session ownership (a #152-class question) first; it is a separate design item, deliberately not absorbed into #147 | n/a |
| `POST /run` | **None** (stateless one-shot by design) | no session id, no writer | n/a |

Compaction consistency is part of the contract, not an afterthought: the sink
handles `AgentEvent::CompactionBoundary` by writing a `compact_boundary`
system entry (`session/writer.rs`, g157), AG-UI's `load_thread_seed` reloads
through `compact::rebuild_declared_seed` (the canonical rebuild, Goal 413 —
truncation after the boundary, legacy-marker backfill, killed-run orphan
tool-call skip-answer), and the REST flat side resyncs via
`is_stored_prefix` / full rewrite with the watermark marked
(`runtime.rs`). The AG-UI `messages` contract (threadId is the session;
request `messages` is this turn's prompt, not the history) lives in
[agui.md](agui.md) — including its documented cost: a client-side history
rewrite does not take effect.

## 10-08 redesign acceptance criteria → where they landed

From the #147 redesign hand-off ("验收判据 ①–⑥", 2026-10-08):

| # | Criterion | Landing |
|---|---|---|
| ① | The four arrival points wire the sink once, at the shared assembly point | CLI run/repl/loop, CLI resume, REST sessions, AG-UI threads (TUI shares the CLI assembly) all mount the same `SessionPersistenceSink` type over a session writer |
| ② | After a compaction rewrite, storage and kernel views agree (resync tested) | `compact_boundary` sink entry (writer.rs, g157) + `rebuild_declared_seed` on reload (Goal 413) + flat-side `is_stored_prefix` resync — each with in-repo tests |
| ③ | A run is incrementally readable while in flight | Sink appends per kernel append (`run_core` pushes + announces in the same breath); in-process proof: the `MidRunProbeProvider` park assertion in `tests/agui_e2e.rs` |
| ④ | Per-turn persistence removed, SIGKILL guarantees intact on every channel | **Variant, ratified:** dual-track instead — REST keeps flat per-turn as the authoritative cold-load record alongside the event-level mirror; CLI/TUI/AG-UI never had kernel-level per-turn writes. Rationale: removing the flat per-turn record would change REST's crash-window and compaction guarantees, which is the redesign body itself, not a cleanup |
| ⑤ | Non-local backends do no per-row full rewrite | **Variant, resolved by routing:** event-level persistence runs through the local native writer and never touches `StorageBackend`; the proposed `TranscriptLog` port is therefore **closed as unnecessary** (not "deferred") — there is no backend that would need it. `append_transcript`'s load-extend-save remains only on the per-turn path |
| ⑥ | `agui-*` read paths check ownership | `thread_session_key` derived from tenant/subject, `open_thread_writer` carries owner/tenant, cancel registry namespaced by session key, `tests/agui_auth.rs` (#152 P0, commit 36487c5b) |

The `messages` semantics (threadId is the session) were evaluated
independently of the persistence mechanism, as the hand-off required, and
landed in 14bac15a.

## Honest boundaries

- **SIGKILL live evidence is an acceptance-engineering task, not a CI one.**
  The criterion "a killed host keeps completed steps (with non-zero token
  cost)" requires killing a real process; the in-process deterministic form
  (the `MidRunProbeProvider` park assertion) is its CI shape. Same split as
  #134 W3a (CI) / W3b (live).
- **Data written is not data recoverable** (the 2026-10-08 revert lesson):
  every persistence claim above names its read path — flat per-turn rows are
  what cold load reads, native transcript rows are what thread reload reads.
  The 10-08 `StoragePersistenceSink` experiment was reverted precisely
  because it wrote the flat store with no reader attached.
- **Retention covers the flat store's transcript shape only.** `purge_sessions_older_than`
  walks `sessions/` and (since the #147 fix) one level of namespace
  subdirectories (`trigger-run/<uuid>`); native session dirs are lifecycle-managed
  (see [sessions.md](sessions.md)) and are not in its scope. Sweep failures are
  observable: `ReclaimReport{removed, failed}` plus a WARN per undeletable
  record (#214).
- **ACP stays dark on purpose.** An API that looks like persistence but has
  no caller is not a feature; this matrix is the reference that says so.
- **Event-stream persistence is #132's domain** (durable sequence numbers,
  cursors, snapshot-at-cursor + gap-free follow, bounded slow consumers).
  The native `transcript.jsonl` rows are a natural data source for #132's
  snapshot semantics, but nothing here pre-builds that interface;
  `http/event_log.rs`'s in-memory ring + explicit `gap` frame is unchanged.
  **W1 (goal 422) now closes the cross-process half of that contract**: the
  per-session event watermark (`SessionMeta.event_watermark.next_seq`, one
  write per turn via `cold_load::persist_event_watermark`) seeds the
  cold-loaded ring, so a pre-restart cursor resolves to one of three explicit
  states — `snapshot` (state rebuilt by the same `compact::rebuild_seed` a
  cold load uses, current up to `up_to_seq`), `gap` (when no snapshot can be
  derived), or the issue-#97 in-ring replay — never a silent resume from now;
  garbage cursors keep their non-gap semantics. The replay window is
  configurable (`RECURSIVE_SSE_REPLAY_CAPACITY`, default = the
  `SESSION_EVENT_LOG_CAPACITY` constant) and the loss side is observable:
  `recursive_event_stream_gaps` / `recursive_event_stream_snapshots` /
  `recursive_event_log_evictions` on `/metrics`, WARN on the seeding and
  fallback paths. This section only *reads* transcripts and metadata — the
  write paths stay the ones mapped above (#147 boundary intact); W2 (bounded
  upstream contract + read-only file API) is a separate goal.
