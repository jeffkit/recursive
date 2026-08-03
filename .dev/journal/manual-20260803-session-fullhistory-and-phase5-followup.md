# Manual edit: session full-history display/export + Phase-5 follow-up (mutants jobs cap, mcp_e2e retry)

**Date**: 2026-08-03 (evening)
**Context**: the 2026-08-03 morning self-improve session ended right after the user's
"好的，开始修吧" (no agent response). The user then hand-edited the working tree
(22:54–22:57) with two coherent pieces of work and opened a fresh session with
"继续". The supervisor (this session) reviewed, verified, fixed one clippy issue,
and landed the work.

## 1. Feature: session full-history display/export with compaction markers

Goal: TUI resume / `session show` / `session export` should show the **whole**
conversation, not just the post-compaction tail that the model seed uses.

- `src/session/serialize.rs` — new `LoadedEntry` enum
  (`Message(Box<TranscriptEntry>) | CompactBoundary { turn, removed }`).
  The box keeps the enum small (clippy::large-enum-variant); `entry_to_message`
  was already `pub` via the session re-exports.
- `src/session/reader.rs` — new `SessionReader::load_full_history`:
  keeps every JSONL line, surfaces each `compact_boundary` system entry as a
  `CompactBoundary` marker, skips corrupt lines (mirrors `load_transcript`).
  `load_messages` / `load_transcript` (the model-seed paths) are **unchanged** —
  they still strip pre-boundary messages.
- `src/session/mod.rs` — `ExportedTranscript` gains
  `compact_boundaries: Vec<ExportedCompactBoundary>` (`#[serde(default)]`,
  additive for the export JSON); `message_count` now counts the full history
  (messages only, boundary markers excluded). New
  `ExportedCompactBoundary { message_index, turn, removed, timestamp }`.
- `crates/recursive-cli/src/main.rs` — `session show` uses `load_full_history`
  and prints each boundary inline (`compact: ⊕ N messages folded into summary`).
- `crates/recursive-tui/src/backend.rs` — session resume now does **two loads**:
  `load_messages` → `rt.set_transcript` (model seed, unchanged semantics) and
  `load_full_history` → UI blocks (full conversation).
- `crates/recursive-tui/src/app/render.rs` — `blocks_from_messages` refactored
  into a shared `push_blocks_for_message`; new `blocks_from_loaded_history`
  renders each `CompactBoundary` as an inline `TranscriptBlock::Compacted`
  marker (`⊕ Conversation compacted: …`), matching the live-compaction display.
- `src/lib.rs` — `team` module gated behind `coordinator-mode` (it was compiled
  but referenced nowhere; the gates build with `--features ...coordinator-mode`,
  so this only stops dead compilation in non-coordinator builds).

## 2. Phase-5 follow-up: cargo-mutants job cap + mcp_e2e retry

Follow-up to the 2026-08-03 rescue infra fixes (`152578c`, `e1103c9`):

- `.dev/scripts/agent-mutants.sh` — cap default `JOBS` at **8**. The 14-core
  box's uncapped `hw.ncpu` (=14) saturated CPU/IO: crashed the mcp_e2e baseline
  (mock-server 2s handshake timeout) and took ~4.8h for 83 mutants — **slower**
  than 4–8 jobs, not faster. Explicit `--jobs N` still overrides.
- `tests/mcp_e2e.rs` — mock-server spawn now retries (3 attempts, linear
  backoff) for modes that must succeed; the `timeout` mode must **not** retry
  (it exists to exercise the read-timeout path), so it is excluded. This
  separates a transient cold-start failure under parallel load from a genuinely
  hung server.

## 3. Verification (all green)

- `cargo check --workspace` — clean
- `cargo clippy -p recursive-agent -p recursive-tui -p recursive-cli --all-targets --all-features -- -D warnings` — clean
- `cargo fmt --all` — clean
- New tests:
  - `session::reader::tests::load_full_history_includes_pre_compact_messages_and_boundaries`
  - `session::reader::tests::load_full_history_no_boundary_returns_all_messages`
  - `session::tests::export_includes_full_history_and_compact_boundaries`
  - `app::render::tests::blocks_from_loaded_history_renders_compact_boundary_inline`
  - `app::render::tests::blocks_from_loaded_history_multiple_boundaries`
- Regression: `tests/compact_boundary.rs` (10), `tests/usage_tracking.rs` (4),
  `tests/mcp_e2e.rs` (17) — all pass.

## 4. Commits

- `fix(phase5): cap cargo-mutants jobs at 8 + retry mcp mock spawn under parallel load`
- `feat(session): full-history display/export with compaction markers`
- `chore(deps): bump plist 1.8→1.10, quick-xml 0.38→0.41` (Cargo.lock; was in the
  user's working tree, kept as a standalone commit so it can be dropped if unintended)

## 5. Follow-up notes

- The `supervisor-backup-20260803-163725/` backup dir from the morning rescue is
  still untracked in the workspace; safe to delete once the rescue commits are
  confirmed (they are: `168a122`, `46ba0d5`, `d580f4e`).
- Goals 380/381 (`swallowed-errors`, `deadline-and-eventloop-tests`) have not
  been run yet — candidates for the next self-improve batch.
