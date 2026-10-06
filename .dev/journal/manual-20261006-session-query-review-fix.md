# Issue #131 review fix — relation windows, unwired-seam docs

- date:        2026-10-06
- goal:        issue #131 (session retrieval family) — address the independent
  reviewer's NEEDS_FIX on the landing in `manual-20261005-session-query.md`
- mode:        orchestrator-direct (review fix round)
- verdict:     completed

## Blocking finding: `replacements()` traced the wrong window

The reviewer was right: `first_replaced = summary_index - removed` assumed the
folded block is the messages *immediately before* the marker. The producer does
the opposite — `Compactor::apply_to_transcript` drains `transcript[..split]`
(the oldest messages), summarises that prefix, and appends the summary behind
the marker, so the folded block is a prefix of the log and the marker sits at
the end of the history it summarises.

Fix (`src/session/relations.rs`): rebuild the window with a cursor over the log
— each marker folds the `removed` oldest messages no earlier marker already
folded — and derive `superseded_by` from `[first_replaced, first_replaced +
replaced)` instead of `[first_replaced, summary_index)`. The count is clamped to
the messages actually present before the marker: a resumed run can write a
marker claiming more messages than the log holds (six such transcripts are on
this box), and without the clamp the window would contain the marker's own
summary. `Replacement`'s doc now states the window, and the module doc + the
`session-query` architecture doc were corrected to the prefix model.

## Non-blocking observations, all addressed

- **Docs claiming behaviour that no producer drives.** `sessions.md` and
  `session-query.md` now say plainly that the active-lease seam has no caller
  (the stat revision is what keeps the index current today), that
  `set_derived_from` has no in-tree producer (so an export is one session plus
  attachments for now), and that `export_session_tree` has no CLI/HTTP entry
  point.
- **`cwd` implied a filter it does not apply.** Descriptions (both tools),
  the module doc and `session-query.md` now state it is a claim to *check*
  against the workspace boundary, not a narrowing — the index covers exactly one
  workspace.
- **Stale `dir` column.** `refresh()` compared only the stat revision, so a
  session whose files kept their mtime+size after the session root moved was
  skipped and `transcript()` read the dead path. `stored_revision` became
  `stored_state` (`dir` + `revision`); a path change now forces a re-index.
- **`set_derived_from` had no test in its own file** (invariant #4's letter).

## Files touched

- `src/session/relations.rs` — cursor-based `replacements`, window-based
  `superseded_by`, docs, reworked fixtures + a real-compaction test.
- `src/knowledge/session_query.rs` — two tool descriptions, module doc, updated
  expectations.
- `src/session/index.rs` — `stored_state` (dir + revision).
- `src/session/writer.rs` — `set_derived_from` unit test.
- `docs/architecture/sessions.md`, `docs/architecture/tools/session-query.md`.

## Tests added / changed

- **New** `session::relations::tests::a_real_compaction_folds_the_oldest_messages`
  — drives a real `AgentRuntime` + `SessionPersistenceSink` compaction (three
  turns, `keep_recent_n(2)`) instead of a hand-placed marker, then asserts the
  first fold starts at message 0, that the summary's own
  `[compacted: N messages …]` header agrees with the reported count, and that
  the newest message before the marker is *not* attributed to that replacement.
  Under the old formula the first window was `[2, 4)` for a marker at index 4
  with `removed = 2`, so this test fails on the old code.
- **New** `session::index::tests::a_moved_session_directory_is_re_indexed`.
- **New** `session::writer::tests::set_derived_from_is_persisted_immediately`.
- Reworked `replacements_count_messages_only`, `trace_event_reports_both_directions`
  (`first_replaced` 1 → 0; the "untouched" case is now the kept message, not
  index 0), `session_trace_explains_a_compaction`,
  `session_event_trace_reports_the_replacement` — the hand-placed fixture now
  follows the producer layout (transcript, marker, summary, tail), and each
  scenario pins both a folded and a kept message.

## Verification

- `cargo test --lib --all-features` — 3074 passed, 0 failed.
- `cargo clippy --all-targets --all-features -- -D warnings` — clean (exit 0).
- `cargo fmt --all -- --check` — clean.
- `cargo test --workspace` — 4372 passed, 0 failed (60 test binaries).
- **The new tests fail on the old formula** (checked, not assumed): with
  `first_replaced = summary_index - removed` and the `..summary_index` window
  temporarily restored, the real-compaction test reports
  `Replacement { summary_index: 4, first_replaced: 2, replaced: 2 }` — the
  newest messages — instead of `first_replaced: 0`, and all five window-pinning
  tests fail
  (`replacements_count_messages_only`, `trace_event_reports_both_directions`,
  `a_real_compaction_folds_the_oldest_messages`,
  `session_trace_explains_a_compaction`,
  `session_event_trace_reports_the_replacement`). Restored and re-run green.

## Environment note

The volume was at 2.2 GiB free when this round started (`cargo test --lib` had
died with `No space left on device` for the reviewer). Cleared this worktree's
`target/debug/incremental` (7.2 GiB, regenerable) and ran every cargo command
with `CARGO_INCREMENTAL=0`; no source or artifact needed by other worktrees was
touched.
