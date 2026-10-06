# Manual landing of issue #131 — session retrieval family (DSH `session-query` borrow)

- goal source: issue #131 (scene gap report, P2, jeffkit follow-up on DSH
  `packages/session-query/` + `session-log-export/`)
- mode:        orchestrator-direct
- base HEAD:   `4e1aee98`
- verdict:     completed

## Problem

The session surface was JSONL transcripts plus `sessions list/rewind`:
the model could not search across sessions, there was no way to explain how a
history came to look the way it does, and a session could not be exported as a
whole tree. `episodic_recall` scans every log per call — fine for a handful of
sessions, wrong for a workspace with hundreds.

## What landed

### 1. Derived FTS5 index — `src/session/index.rs`

`SessionIndex` is a **discardable read model**: SQLite FTS5 over the workspace's
sessions, at `~/.recursive/workspaces/<hash>/session-index.sqlite3` (dir 0700,
db 0600). It holds nothing that cannot be re-derived from the session
directories.

- **self-identifying header** — `application_id = 0x52435331` ("RCS1") +
  `user_version = 1`. A database with a foreign stamp is **rebuilt in place**
  (tables dropped, schema recreated), never migrated.
- **revision-keyed cold reads** — `stat_revision()` (transcript + meta mtime in
  ns and size) decides whether a session needs re-reading; unchanged logs are
  skipped and the pass counts them.
- **active leases + cold-read cache** — `lease()` writes to a `TEMP` table that
  dies with the connection; leased sessions are always re-read on refresh and
  are never evicted by the parsed-transcript LRU (`ColdReadCache`).
- search: FTS5 `trigram` (case-insensitive substring, CJK-safe) for terms of
  ≥3 chars, `LIKE` fallback below that; results are capped at
  `MAX_SEARCH_RESULTS = 100`.

### 2. Replacement / origin tracing — `src/session/relations.rs`

`replacements()` rebuilds the compaction chain (a `compact_boundary` marker
folds N older messages into the summary that follows it); `trace_session` and
`trace_event` answer "what replaced this / what did it replace" in both
directions. `SessionTrace::same_origin` reports sessions whose first message
uuid matches — recursive persists no explicit fork lineage, so transcript
identity is the derived signal (documented as such).

### 3. Model-facing tools — `src/knowledge/session_query.rs`

`session_search` / `session_event_search` (eager) and `session_trace` /
`session_event_trace` / `session_event_read` (deferred). All five share ONE
`SessionQuery` (one connection, one cold-read cache) and are registered by
`build_standard_tools`. **Workspace boundary**: a `cwd` argument outside the
calling session's workspace is refused with `Error::ToolRejected` — a caller
asking for another project's history is told no, never silently narrowed. `..`
is resolved lexically before the containment check, so it cannot climb out.

### 4. Streaming ZIP export — `src/session/export.rs`

`export_session_tree()` streams a ZIP (`ZipWriter::new_stream`, no whole-archive
buffer) of the root session, its `derived_from` descendants (nested under
`<parent>/children/<child>/`) and the session directory's attachments
(everything but `transcript.jsonl`, `.meta.json` and the `.lock` sentinel),
plus a `manifest.json`. Only one export per session runs at a time — a
process-wide `ExportGuard`; a second is refused with the new
`Error::ExportInProgress`.

### 5. Provenance seam

`SessionMeta.derived_from` (optional, serde-default) +
`SessionWriter::set_derived_from()` (written straight to `.meta.json`, so a
fork that crashes before its first `finish()` still knows its source). This is
the relation the export tree and `session_trace`'s lineage read.

## Dependencies (invariant #6 justification)

New Cargo feature `session-index = ["dep:rusqlite", "dep:zip"]`, enabled in
`default`:

- **rusqlite** — already a workspace dependency behind `vector-memory`
  (`bundled` SQLite, which `libsqlite3-sys` builds with `-DSQLITE_ENABLE_FTS5`).
  FTS5 is the derived read model the goal asks for; no new third-party crate is
  introduced and `Cargo.lock` is unchanged.
- **zip** — already a workspace dependency behind `skill-hub`; used for the
  streaming session-tree export.

## Verification

- 54 new unit tests across the four new modules (28 index, 6 relations, 5
  export, 14 session-query) plus the registry wiring test
  (`build_standard_tools_registers_the_session_query_family`), including the
  three acceptance properties:
  - **index is pure derived** — `deleting_the_database_rebuilds_identical_results`
    (drop the DB file, rebuild, compare hits + headers byte-for-byte) and
    `foreign_application_id_is_rebuilt_in_place`.
  - **cwd boundary** — `cwd_outside_the_workspace_is_refused` (both search
    tools) and `cwd_at_or_below_the_workspace_is_allowed`;
    `normalize_falls_back_to_lexical_for_missing_paths` pins the `..` case.
  - **export tree + single download** — `export_walks_child_sessions`,
    `export_includes_transcript_meta_and_attachments`,
    `only_one_export_per_session_is_running`.
- `cargo test --workspace` — 4369 passed / 0 failed (54 test binaries).
- `cargo clippy --all-targets --all-features -- -D warnings` — clean.
- `cargo fmt --all -- --check` — clean.
- `bash .dev/scripts/agent-mutants.sh` — 6 mutants tested: 4 caught, 2
  unviable, 0 missed.

## Environment note

One `cargo test --workspace` run on this (heavily loaded, several pipelines at
once) box failed `tests/resume_by_id.rs::lock_thread_safety_serialises_open_existing`
with `SessionLockBusy`. That test file sets process-global session env vars from
several parallel tests, so it races with itself under load; it passes 3/3 run in
isolation and the next full-workspace run was green. Not touched here.

## Deliberately not done

- **No HTTP route and no CLI subcommand.** HTTP sessions are persisted through
  `StorageBackend` (flat `<workspace>/.recursive/sessions/<id>.jsonl`), not the
  per-workspace session directory the index and the tree export walk, so a
  `GET /sessions/{id}/export` would export a session that is not the one on
  disk. `export_session_tree` is the library entry point a channel can call
  once the HTTP session layout and the JSONL layout converge.
- **Cross-session fork lineage is not persisted by any producer today**, so
  `same_origin` (transcript identity) is the derived signal; `derived_from` is
  the declared one, set by whoever writes such a session.
