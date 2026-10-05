# Manual — issue #92 (continuation): per-turn transcript persistence + close Redis direction

- **Date**: 2026-10-05
- **Issue**: #92 `feat(http): cloud-runtime 在 http 模式被显式拒绝接线 + transcript 仅 teardown 落盘`
- **Worktree**: `.flowcast/runs/pipeline-92-1005124456/worktree` (branch `v2-pipeline-92-1005124456-cont`, based on `1e86598b`)
- **Reporter re-review**: `okguitar` accepted suggestion 1 (S3 wiring + honest docs) and kept the
  issue open for the remaining two:
  1. per-turn / incremental transcript persistence (append-only),
  2. Redis hot-state landed **or** the direction explicitly closed.
- **Round 2 (reviewer `NEEDS_FIX`)**: the first cut inferred "the on-disk prefix is
  unchanged" from a *message count*. A length is not evidence about content — the
  reviewer reproduced a completed, 200-ACKed turn that was silently never written
  (post-compaction length == watermark → neither branch wrote anything, yet the
  watermark advanced). Fixed by comparing content (see "Fix round 1" below).

## What this change does

**Suggestion 2 — per-turn append-only transcript persistence.**
`AgentRuntime` now persists each turn's transcript growth through the injected
`StorageBackend` at the end of `drive_turn` **for the REST sessions that own a
stable session id**. A crashed or OOM-killed gateway loses at most the in-flight
turn instead of everything since the last teardown.

- `StorageBackend::append_transcript` (new, defaulted): load-extend-save fallback
  so every backend is correct; `LocalStorageBackend` overrides it with a real
  JSONL append. Appending an empty slice is a no-op.
- `AgentRuntimeBuilder::persist_transcript_per_turn(bool)` (default off — the
  CLI/TUI already write their own per-message session JSONL and must not be
  double-written). The REST **session-creation sites** opt in:
  `create_session`, `fork_session` and `cold_load::build_restored_runtime`. It is
  deliberately not in the shared `build_session_runtime[_parts]` factory: AG-UI
  builds through the same factory but reseeds its transcript from the
  client-supplied `messages` on every run, so the watermark premise ("on-disk
  prefix == runtime prefix") does not hold there — AG-UI stays teardown-only.
  One-shot `/run` and triggers have no session id, so the flag is a no-op there.
- Watermark (round-2 semantics): the runtime tracks how many leading messages are
  an **exact prefix** of the stored record. It is resolved on first persist by
  re-reading the backend and comparing content (`is_stored_prefix`), which makes
  a cold-loaded session correct (its reassembled system prompt is not the stored
  message at index 0, so it resyncs with one full save and appends afterwards)
  and a fresh session correct (empty backend → everything is appended).
- `transcript_rewritten` (round 2): set by every in-place rewrite of
  `self.transcript` — cross-turn compaction, emergency (`compact_on_overflow`) and
  manual compaction (`compact_now` / `compact_partial_before|after`),
  `Microcompactor::prune` (content swapped at an unchanged index and length),
  `set_transcript`, `truncate_transcript`. While set, the reused watermark is
  discarded and the backend is re-read, so an append can never land on a prefix
  that no longer describes these indices. Cleared after a successful persist.
- Durable watermark: it is advanced **only when the write succeeds**. A
  transient storage error resets it to `None`, so the next turn re-reads the
  backend and appends the gap — a failed turn is repaired instead of being
  skipped forever (which would break the "loses at most the in-flight turn"
  promise on the S3 path). A watermark `load_transcript` failure likewise skips
  the turn rather than risk duplicating the prefix.
- Legacy-safe appends: the pre-#92 writer emitted `lines.join("\n")` with no
  trailing newline, so `LocalStorageBackend::append_transcript` first emits the
  missing separator when the existing file is non-empty and unterminated
  (seek-to-end, one byte — O(1)). `load_transcript` additionally ignores a
  truncated **unterminated** final line (a torn append) while still rejecting a
  malformed *terminated* line as corruption.
- Teardown (DELETE / idle eviction / graceful shutdown) still does a final full
  save, now as a resync rather than the only write.
- Honest docs for the S3 path: S3 objects cannot be appended to, so
  `S3StorageBackend` takes the load-extend-save fallback and rewrites the whole
  object each turn. CHANGELOG/README/website (en+zh) now say so instead of
  claiming a per-turn append.

**Suggestion 3 — Redis direction closed.**
Per-turn S3 transcripts + cold load already cover crash recovery, so a shared
Redis session table is redundant for `recursive http`. Rather than ship a
connection pool that is never used, the direction is closed:

- `docker-compose.yml` no longer provisions a `redis` service, its env block or
  the `redis_data` volume; the compose stack is now `recursive` + `localstack`.
- `RECURSIVE_REDIS_URL` is documented as ignored (a note is logged);
  `.env.example` drops it.
- README + website (en/zh: `deployment/cloud.md`, `deployment/docker.md`,
  `deployment/index.md`, `guide/config.md`, `cli/http.md`, `cli/sessions.md`,
  Vitepress sidebar) now say "S3" not "S3 + Redis" and describe the per-turn
  semantics.
- `RedisSessionStore` stays available through the library API
  (`AgentRuntimeBuilder::session_store`).

## Fix round 1 (reviewer blocker: the length-based watermark)

`drive_turn` runs cross-turn compaction *before* the persist, and compaction
drains the front of the transcript and splices a summary in. With the shipped
`keep_recent_n = 8` the post-compaction length is `keep_recent_n + 1 + attachments`,
which — depending on how many messages the turn added — can equal the previous
turn's watermark (nothing written, yet marked persisted: the ACKed turn is lost)
or exceed it (an append whose start index no longer matches the stored file, so
messages are dropped or duplicated; for tool-using turns that can even store an
orphaned `Role::Tool` result, violating invariant #8). Inferring "prefix
unchanged" from a count was the root cause; `Microcompactor::prune` (content
rewrite, same length) and a compaction summary at index 0 being mistaken for the
runtime's missing system prompt were the same bug.

Fix: content comparison plus an explicit rewrite signal.

- `persist_transcript_turn` reuses the watermark only while
  `transcript_rewritten` is false; otherwise it re-reads the backend and only
  appends when `is_stored_prefix(stored, transcript)` — an exact, index-by-index
  prefix — holds. Anything else is a full `save_transcript`.
- The `+1` system-prompt shift heuristic is **gone**; a stored record that is not
  this runtime's exact prefix is resynced, which covers the legacy file, the
  cold-loaded session and the compaction summary uniformly.
- `mark_transcript_rewritten()` is called from every in-place rewrite site (one
  place, so the flag cannot be forgotten per site silently).

Reproduction now covered by tests (both previously-failing shapes):
`per_turn_persistence_persists_after_a_length_preserving_compaction`
(post-compaction length == watermark → previously nothing written) and
`per_turn_persistence_resyncs_after_cross_turn_compaction` (post-compaction
length == watermark + 1 → previously a misaligned append). The reviewer's own
scratch repro (seeded system message + `.system_prompt(...)`, `keep_recent_n=8`,
compaction on turn 1) now ends with `disk == memory` and the ACKed answer on
disk; the scratch test was run and deleted, tree left byte-identical.

## Files touched

| File | Change |
|------|--------|
| `src/storage/mod.rs` | `StorageBackend::append_transcript` (defaulted load-extend-save); 2 default-path tests |
| `src/storage/local.rs` | native JSONL append; separator for pre-#92 / torn files; tolerant torn-trailing-line read; `save_transcript` newline-terminates each line; 6 tests |
| `src/runtime.rs` | `persist_transcript_per_turn` + `persisted_transcript_len` + `transcript_rewritten` fields; `persist_transcript_turn` (prefix check + rewrite flag) called from `drive_turn`; `mark_transcript_rewritten` at every rewrite site; `is_stored_prefix` |
| `src/runtime/builder.rs` | `persist_transcript_per_turn(bool)` setter + field plumbing |
| `src/http/handlers.rs` | opt in at `create_session` / `fork_session` (and set the fork's session id); NOT in the shared factory; comment updates |
| `src/http/cold_load.rs` | opt in for `build_restored_runtime` |
| `src/http/mod.rs` | `AppState.storage` doc: per-turn persist + teardown resync |
| `docker-compose.yml`, `.env.example`, `CHANGELOG.md`, `README.md`, `website/**` | Redis direction closed; per-turn semantics + S3 rewrite cost documented |
| `src/runtime/tests.rs` | `RecordingStorage` (save/append/load counters) + `FlakyStorage` + 11 runtime tests |
| `tests/http.rs`, `tests/http_common/mod.rs` | `MemoryStorage` records appends separately from full saves; 1 end-to-end test |

## Tests added

- `src/storage/local.rs`: `append_transcript_extends_existing_transcript`,
  `append_transcript_creates_missing_session`, `append_transcript_empty_slice_is_noop`,
  `append_transcript_handles_legacy_file_without_trailing_newline`,
  `load_transcript_ignores_truncated_trailing_line`,
  `load_transcript_rejects_corrupt_terminated_line`.
- `src/storage/mod.rs`: `default_append_transcript_extends_existing_transcript`,
  `default_append_transcript_empty_slice_is_noop` (the non-native S3 default path).
- `src/runtime/tests.rs`: `per_turn_persistence_appends_only_the_delta` (also pins
  one backend read for two turns: the fast path), `per_turn_persistence_is_off_by_default`,
  `per_turn_persistence_does_not_duplicate_seeded_transcript`,
  `per_turn_persistence_resyncs_when_stored_record_is_not_a_prefix`,
  `per_turn_persistence_repairs_after_a_failed_write`,
  `per_turn_persistence_resyncs_a_disk_prefix_without_system_message`,
  `per_turn_persistence_resyncs_after_cross_turn_compaction` (round 2),
  `per_turn_persistence_persists_after_a_length_preserving_compaction` (round 2),
  `per_turn_persistence_resyncs_after_microcompact_prune` (round 2),
  `per_turn_persistence_is_a_noop_when_backend_matches_memory`,
  `is_stored_prefix_compares_content_not_length`.
- `tests/http.rs`: `post_message_appends_transcript_before_teardown` (POST
  `/sessions/:id/messages` with no DELETE already leaves the transcript on the
  backend, via append not full save).

Tests that asserted exact full-save counts on DELETE
(`delete_session_persists_transcript_with_tool_pairing`,
`delete_persists_each_session_transcript_separately`) still pass because
`MemoryStorage` records the runtime's per-turn `append_transcript` in a separate
`appends` bucket.

## Quality gates

Run in the worktree:

- `cargo fmt --all` — clean.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — exit 0.
- `cargo test --workspace` — exit 0 (0 failures).

Mutation spot-checks done by hand (temporarily flipping new logic, then
restoring the file byte-identically): the `mark_transcript_rewritten` body, the
rewrite-flag guard, the flag clearing and `is_stored_prefix`'s length comparison
are each killed by at least one of the new tests.

Not run here: Docker e2e (needs a release binary + image build). The change
does not alter e2e behaviour.
