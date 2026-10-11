# manual-20261011 — #158 compaction must not swallow the leading System prefix

- Date: 2026-10-11
- Goal: issue #158 (cluster #186 “上下文与压缩契约”)
- Baseline HEAD: 14bac15a (pipeline branch on 6e791fae)

## Goal

Cross-turn / emergency / manual / intra-turn compaction used to drain
`transcript[..split]` and splice the summary at index 0, deleting the rebuilt
System prompt (tools/sandbox rules, persona, AGENTS.md constraints) on the
first compaction. Fix: always keep the leading `Role::System` run verbatim and
splice the summary *after* it — `[prefix(System)…, summary, kept…]`.

## Files touched

- `src/compact/mod.rs`
  - new `Compactor::leading_system_prefix_len` (leading System run, stops at a
    compaction summary so summaries cannot accumulate in the frozen prefix).
  - `safe_split_point` → `split.max(prefix_len)`.
  - `split_for_compaction` → summarised slice is `transcript[prefix..split]`;
    `split <= prefix` is rejected as “nothing to compact”.
  - `compact()` feeds only `transcript[prefix..split]` to the summariser.
  - `apply_to_transcript` → `drain(prefix..split)` + `insert(prefix, summary)`;
    `removed = split - prefix`.
- `src/kernel.rs` / `src/run_core.rs`
  - `RunCore.intra_turn_compactions` reported on `RunInnerOutcome`; kernel keys
    the summary-forwarding off it instead of “summary is at index 0”.
  - locates the intra-turn summary by its marker.
- `src/runtime.rs`
  - cross-turn + emergency compaction locate the summary by marker (events,
    persistence) and insert reinjected attachments directly after it.
  - `compact_partial_before` keeps the prefix; `compact_partial_after` clamps
    its pivot out of the prefix.
- `src/http/cold_load.rs`, `src/http/agui.rs`
  - drop only a leading *non-summary* System message, so a stored compaction
    summary survives cold load / AGUI thread resume.
- `src/llm/anthropic.rs`
  - `extract_system_message` joins *all* leading System messages into the
    top-level `system` field (the Messages API has no mid-array system role),
    so the summary/attachments no longer land as invalid `messages` entries.

## Tests added / updated

- New: `compaction_keeps_system_prefix_verbatim`,
  `summary_is_inserted_after_prefix`,
  `repeated_compaction_does_not_accumulate_summaries`,
  `leading_system_prefix_len_stops_at_compaction_summary`.
- Strengthened: `safe_split_point_keeps_system_message_in_kept`,
  `apply_to_transcript_splices_and_returns_counts`,
  `apply_to_transcript_compacts_normally_with_real_conversation`,
  `validate_boundary_preserving` splice, run_core `maybe_compact_*`,
  `compact_partial_before_summarizes_prefix_keeps_suffix`,
  `tests/http_context_parity.rs` (summary now follows the System prompt).
- New: `normalize_keeps_compaction_summary_behind_system_prompt` /
  `normalize_keeps_leading_compaction_summary_without_prompt` (cold load),
  `load_thread_seed_keeps_a_compaction_summary` (AGUI),
  `extract_system_message_joins_leading_systems` (Anthropic).

## Gates

- `cargo test --workspace` — 64 binaries, all ok (EXIT=0).
- `cargo clippy --all-targets --all-features -- -D warnings`.
- `cargo fmt --all`.

## Notes

- Neighbour issues #156/#161 are NOT in this branch; the
  `cold_load`/`agui` “keep a leading summary” guard plus the Anthropic
  coalescing are the minimal pieces of that contract needed so #158 does not
  introduce an intermediate broken state.
