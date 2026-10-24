---
type: Architecture
title: Context Compaction — the Four Contracts
description: Single ownership, canonical rebuild, single-source token accounting, observable degradation — the compaction contracts with their authoritative symbols, regressions and mutation negative controls (issue #197).
tags: [compaction, context, transcript, rebuild, tokens, issue-197]
timestamp: 2026-10-24T00:00:00Z
---

# Context Compaction — the four contracts (issue #197)

Source: `src/compact/` (`Compactor::apply`, `rebuild.rs` module doc),
`src/runtime.rs`, `src/kernel.rs`, `tests/compact_contract.rs`.

Every context compaction answers to four contracts: **one owner, one write
path** (goal 411), **canonical rebuild from the persisted boundary** (goal
413), **one source of token truth** and **observable degradation** (goal 414).
The authoritative semantics live in the code — `src/compact/rebuild.rs`'s
module doc is the contract text, `tests/compact_contract.rs` the executable
statement. This page is the map that ties each contract to its symbols,
regressions and negative controls. If code and page ever disagree, code plus
tests win: file an issue, do not patch either side casually.

## Contract 1 — one owner, one write path (goal 411, `7ad601bf`)

`AgentRuntime` is the sole holder of the authoritative transcript
(`Arc<Vec<Message>>`, `src/runtime.rs:151`); the kernel sees a copy-on-write
clone. The only summarising rewrite is
`Compactor::apply` (`pub async fn apply`,
`src/compact/mod.rs:433`): the leading System prefix survives verbatim (the
prefix run stops at the first summary — `leading_system_prefix_len`,
`src/compact/mod.rs:372`), the summary row is spliced right after it, the
tail is preserved verbatim. The runtime re-checks that shape with
`verify_compaction_rewrite` under `debug_assert!`
(`src/runtime.rs:864`, `:1063`). Intra-turn compaction runs on the kernel's
COW clone, so the wrapper can only *append* the summary and the turn tail;
the kernel locates the summary by its `is_compaction_summary` marker and
reports `intra_turn_compactions` (`src/kernel.rs:428-436`; the append-only
contract is documented at `src/runtime.rs:620-637`). Every in-place rewrite
stales the persist watermark via `mark_transcript_rewritten` (#92 resync).

Regression: `tests/compact_contract.rs::test_compact_contract_single_path`
(static half: no split computation outside `src/compact/`; behavioural half:
runtime rewrite ≡ `apply` rewrite). Negative controls, each naming its
mutation: `_negative_control_prefix_protection` (summarise from index 0 →
prompt folded into the summary), `_negative_control_tool_call_pairing`
(naive split lands on a `Role::Tool` result → preserved tail starts orphan),
`_negative_control_bypassing_apply` (a bespoke splice is not the recorded
compaction → `verify_compaction_rewrite` rejects it). Invariant #8 pairing
across compaction is pinned separately by
`tests/invariants/tool_call_pairing.rs::compaction_preserves_tool_call_pairing`.

## Contract 2 — canonical rebuild from the persisted boundary (goal 413, `f20b31cb`)

The marker row `apply` splices in *is* the persisted boundary:
`is_compaction_summary` round-trips through `TranscriptEntry` into every
writer (JSONL session log, AG-UI mirror, legacy-transcript rewrite), and the
g157 `compact_boundary` line carries the same boundary for transcripts
written before the field existed. Every consumer that turns stored rows back
into a model seed rebuilds through `src/compact/rebuild.rs` —
append-only readers (`SessionReader::load_messages*`, AG-UI
`load_thread_seed` / `rebuild_thread_seed_from_content`,
`src/http/agui.rs:662`/`:673`) go through `rebuild_declared_seed`, the one
place the declaration rule lives; HTTP cold load
(`normalize_stored_transcript`, `src/http/cold_load.rs:446`) goes through
`rebuild_seed`. The framing rules, in order: ① locate the first
`is_compaction_summary` row (a summary is real conversation state, never
prompt); ② drop the stored rebuilt prompt — the leading non-summary System
run stops at the summary, with the legacy second-row rule recovering a
pre-413 unmarked mirror summary; ③ drop the leading orphan `Role::Tool` run
(invariant #8); ④ drop empty-assistant rows (#168). The cut honours the
persisted *declaration*, not the marker row alone: an intra-turn snapshot is
`[…history…, SUMMARY, …turn tail…]`, where the rows before the marker are
the preserved tail — slicing there would delete the most recent conversation
on the next resume. Contract phrase: 「重建结果 ≡ 压缩后内存态」.

### The precedent family is the negative control

Contract 2 has no 411-style named negative control. Ratified form (owner
ruling, 2026-10-24): **the precedent family *is* the negative control** —
each framing rule above is pinned red by a named precedent, so deleting a
rule turns its precedents red. The mapping is the written carrier:

| Deleted framing rule | Precedent(s) that go red |
|---|---|
| ① key on the marker, not the role | `tests/compact_contract.rs::rebuild_156_summary_is_not_prompt` (plus `rebuild.rs` unit `rebuild_keeps_summary_first_and_tail_verbatim`) |
| ② drop the stored prompt run, stop at the summary | `rebuild_158_boundary_after_prefix` (kept prompt doubles); legacy variant `rebuild_158_legacy_unmarked_summary_survives` (folded history erased with the prompt) |
| ②' recover the legacy second-row summary under a declaration | `rebuild_declared_boundary_backfills_a_legacy_summary` (plus `rebuild.rs` unit `rebuild_declared_seed_marks_a_legacy_summary_after_the_boundary`) |
| ③ drop the leading orphan tool run | `rebuild_160_orphan_results_dropped_by_canonical_path` |
| ④ drop empty-assistant rows (#168) | `rebuild.rs` unit `rebuild_empty_assistant_rows_are_filtered` (writer-side guard: `run_core.rs:3570` `stop_completion_without_content_persists_no_empty_assistant`) |
| cut at the declaration, never at a bare marker | `rebuild.rs` unit `rebuild_does_not_slice_at_a_marker_that_trails_the_history`; declaration-path consumers additionally red via the ②' row |

Headline: `test_compact_contract_rebuild_markers` (persisted marker
rebuildable, resume seed ≡ post-compaction memory); #160's consumer side is
`load_thread_seed_cuts_at_the_persisted_boundary` (`src/http/agui.rs:2650`),
and #160's resume branch shares the rebuild since `fa0a1332`
(`rebuild_thread_seed_from_content` — negative control there: restoring the
old bare parse turns both its regressions red, proven in the commit message).

## Contract 3 — one source of token truth (goal 414, `60ff94c8`)

Token normalisation happens once, in the adapter layer (#163):
`TokenUsage::input_tokens()` folds `cache_hit_tokens` in when the provider
reports `prompt_tokens` without its cache reads, and an un-normalised
reading reaching the trigger is announced with a conservative-fallback WARN
(`token_normalized_warns`, `src/compact/mod.rs:834` — a `logs_contain` text
pin). The cross-turn trigger consumes only the *last* LLM call's
`prompt_tokens`, never the turn-accumulated sum (#162, `236defd3`:
`compact_input.prompt_tokens = last_prompt_tokens`, `src/runtime.rs:553-574`;
the cache fields stay turn-accumulated because their consumer is
CompactionBoundary telemetry, not the trigger). The consumer-side nail
(goal 414): `should_compact_usage` folds cache reads itself, so a raw-field
fallback cannot silently decide; the byte estimate is only a fallback when
usage is unavailable (== 0).

Regressions: `tests/compact_contract.rs::test_compact_contract_token_normalized`
and the runtime-level pair
`cross_turn_compaction_ignores_accumulated_prompt_tokens` /
`cross_turn_compaction_fires_on_last_prompt_tokens`
(`src/runtime/tests.rs:2474` / `:2531`). Negative control:
`_negative_control_unnormalized_trigger_bypass` — mutation: the trigger
falls back to the raw `prompt_tokens` field; on the #162 shape (100 fresh +
900 cached vs threshold 500) the must-fire compaction stops firing and the
assertion goes red.

## Contract 4 — degradation is observable (goal 414, `60ff94c8`)

Any replacement of conversation rows by a summary — cross-turn, emergency,
manual, microcompaction placeholder swap — emits a WARNING on target
`recursive::compact` carrying the splice's field of record: `before`,
`after`, `summarized_start`, `summarized_end`, `removed`, `summary_chars`,
`step` (`src/compact/mod.rs:562-573`). A legitimate no-op stays on the
debug channel and never warns. This is the owner ruling 「任何降级必须可观测」
applied to compaction.

Regressions: runtime-level
`tests/compact_contract.rs::test_compact_contract_degradation_warning`
(when the transcript was rewritten, the WARNING fired — boundary visible in
the transcript; and the no-op side: below the char threshold nothing is
replaced), plus the in-module text pins
`test_compact_contract_degradation_warning` (`before=7` / `after=4` /
`summarized_start/end`, `src/compact/mod.rs:876`) and
`test_compact_contract_microcompaction_warns` (`:943`). Implicit negative
control: deleting the `tracing::warn!` turns both `logs_contain` pins red.

## Contract × regression × negative control

| Contract | Authoritative landing | Regression | Negative control |
|---|---|---|---|
| 1 single ownership | goal 411 `7ad601bf`: transcript's sole holder is `AgentRuntime`; every rewrite goes through `Compactor::apply` | `test_compact_contract_single_path` (static scan + behavioural half) | 3 named: `prefix_protection` / `tool_call_pairing` / `bypassing_apply` |
| 2 canonical rebuild | goal 413 `f20b31cb`: `src/compact/rebuild.rs` (`rebuild_seed` / `rebuild_declared_seed`) + three consumers (CLI resume / AG-UI thread reload / cold load) unified; #160 resume closed by `fa0a1332` | `rebuild_markers` headline + precedent family (incl. legacy backfill) | the precedent family *is* the negative control (delete a framing rule → its precedent goes red, table above) |
| 3 token single source | #163 (adapter normalisation) → #162 `236defd3` → goal 414 nail `60ff94c8` (un-normalised input WARN + conservative reading) | `token_normalized` (+ `_warns` text pin `mod.rs:834`) + runtime pair `src/runtime/tests.rs:2474`/`:2531` | `negative_control_unnormalized_trigger_bypass` |
| 4 observable degradation | goal 414 `60ff94c8`: replacement/microcompaction WARNING (before/after/summarized range, target `recursive::compact`); legitimate no-op stays debug | `degradation_warning` (runtime level, `tests/compact_contract.rs:706` + `mod.rs:876` text pin + `microcompaction_warns` `:943`) | implicit, proven: delete the `tracing::warn!` → `mod.rs` `logs_contain` red |

## Instance × landing commit

| Instance | Status | Landing | Regression / negative control |
|---|---|---|---|
| #156 intra-turn compaction dropped whole turns | landed (evolved form) | goal 411 `7ad601bf` (single ownership + `Compactor::apply` single path). Not the issue's original `CompactionSplice`: under COW the kernel compacts its clone and reports `intra_turn_compactions`, locating the summary by the `is_compaction_summary` marker (`kernel.rs:428-436`); the runtime only *appends* summary + tail (`runtime.rs:620-637` contract comment) | `rebuild_156_summary_is_not_prompt` + `single_path` behavioural half |
| #158 system prompt summarised away | landed | `c5457d21` (leading System prefix contract `leading_system_prefix_len`, `mod.rs:372`) + goal 411 folded it into `apply` | negative control `prefix_protection` + precedents `rebuild_158_*` (×2) |
| #160 AG-UI reload ignored the compaction boundary | landed | goal 413 `f20b31cb` (first load `load_thread_seed` through canonical rebuild) + `fa0a1332` (interrupt-resume branch switched to `rebuild_thread_seed_from_content`, removing the second bare parse) | existing `load_thread_seed_cuts_at_the_persisted_boundary` + `rebuild_160` precedent + fa0a1332's 2 regressions (negative control: restoring the bare parse turns all red, commit message) |
| #161 cold load dropped the compaction summary | landed | goal 413 `f20b31cb` (`normalize_stored_transcript` through rebuild, summary kept first) | `cold_load.rs:644`/`:657` (`out[0].is_compaction_summary`) + `rebuild_161` precedent |
| #162 cross-turn compaction consumed turn-accumulated tokens | landed | `236defd3` (`runtime.rs:553-574` `compact_input.prompt_tokens = last_prompt_tokens`, cache fields kept for telemetry) + goal 414 `60ff94c8` (consumer-side nail) | runtime-level `cross_turn_compaction_ignores_accumulated_prompt_tokens` (`:2474`) / `_fires_on_last_prompt_tokens` (`:2531`) + negative control `unnormalized_trigger_bypass` |
| #196 empty assistant row persisted | landed | `f72f6b11` (`has_persistable_interrupt` criterion reused from #168: no text, no reasoning → skip push + WARN) | `stop_completion_without_content_persists_no_empty_assistant` (`run_core.rs:3570`) |
| adjacent #159 UTF-8 panic on re-injection | landed | `e88131b9` + `d43e8d15` (salvage) | char-boundary tests |
| adjacent #163 adapter usage normalisation | landed (CLOSED) | one normalisation in the adapter layer + the 414 nail | `token_normalized_warns` (`mod.rs:834`, incl. the conservative fallback reading) |

## Honest boundaries

- **Tenancy/identity is deliberately absent** (owner ruling: runtime tenant
  transparency): the kernel carries no identity field by design, and no
  compaction contract branches on tenant/session ownership — that belongs to
  the serving edge (#146/#152, tracked there).
- **Event-stream durability is #132's domain**, not folded in here; see
  [persistence.md](persistence.md). ACP's dormant persistence row is likewise
  mapped there, not here.
- **Threshold values are tuning, not contract** (#163-adjacent, separate
  discussion); this page pins only the normalisation and consumption rules.
- This page describes landed code as of goal 424. It is a map, not a
  specification to re-implement against: the code and
  `tests/compact_contract.rs` remain the single authority.
