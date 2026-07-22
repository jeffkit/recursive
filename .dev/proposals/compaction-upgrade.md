# Proposal: Compaction Upgrade — Multi-layer, Cache-aware, Observability-driven

> **Status**: Draft — pending review
> **Created**: 2026-07-22
> **Baseline**: post-Goal-305 (cross-turn compaction landed), TUI auto-compaction landed (2026-07-21)
> **Scope**: Goals 329–342 (14 goals)
> **Reference**: `~/Downloads/fake-cc` (Claude Code-style compaction) used as design reference; not copied verbatim.

---

## 1. Motivation

Recursive's context compaction is a single-layer LLM-summary event: when the
transcript crosses a threshold, the older region is summarized into one
`System` message and the recent N messages are kept verbatim. This works but,
measured against the `fake-cc` reference, has gaps that matter once Recursive
targets large-scale / long-session commercial use:

- **Single layer, no cheap middle tier.** Long sessions trigger expensive
  LLM-summary calls when much of the bloat is just stale tool output that
  could be pruned without an LLM.
- **No prompt-cache awareness.** Every summary rewrites the prefix → cache
  collapses on the post-compact turn.
- **No post-compact context restoration.** The model re-`Read`s files it
  already read, re-bloating context.
- **No circuit breaker.** A provider under pressure retries compaction every
  step/turn, wasting API calls.
- **Intra-turn threshold bug.** `run_core::maybe_compact` checks only
  `threshold_chars`, ignoring the more accurate `threshold_prompt_tokens`
  — CJK long sessions compact too late.
- **No PTL self-rescue.** An extremely long session can deadlock when the
  summarization call itself hits prompt-too-long.
- **No recompaction telemetry.** Pathological "compact → still over → compact
  again" chains are invisible.
- **Weak free-text fallback.** Providers without structured output get a
  ≤300-word vague summary, contributing to recompaction.

This proposal upgrades compaction to a **multi-layer, cache-aware,
observability-driven** system while preserving Recursive's 8 invariants and
the "small kernel" discipline (no new branches in `run_inner`).

## 2. Design Principles

1. **Don't break invariants.** No new `run_inner` branches (#1), no `unwrap`
   (#5), finish reasons stay data (#7), tool-call ↔ tool-result pairing
   preserved (#8). New concerns land in `src/compact/` submodules, not the
   kernel.
2. **Progressive, testable, rollback-safe.** One surgical goal per change,
   each with unit + invariant tests, clippy/fmt/test green. Goals are
   independently mergeable and revertable.
3. **Reuse existing data sources.** `ReadFileState`, skills catalog,
   `PlanApprovalGate`, `TodoWriteTool`, `TokenUsage.cache_*` already exist —
   no new provider plumbing for cache metrics.
4. **Don't copy fake-cc's complexity.** Drop the GrowthBook / ant-only
   feature-flag sprawl (`CACHED_MICROCOMPACT`, `CONTEXT_COLLAPSE`, `KAIROS`).
   Use static config + env overrides; keep Recursive predictable.
5. **Measure before optimizing.** Cache optimization (goal 341) is gated on
   telemetry data (goal 336), not assumed.

## 3. Module Reorganization

`src/compact.rs` → `src/compact/` directory (goal 329, behavior-preserving):

```
src/compact/
  mod.rs       # Compactor + should_compact + safe_split_point (from compact.rs)
  micro.rs     # Microcompactor (332)
  reinject.rs  # FileReinjector / SkillReinjector / PlanTodoReinjector (334/335/340)
  retry.rs     # truncate_head_for_retry (337)
  prompt.rs    # free-text prompt + format_compact_summary (339)
```

`pub use` keeps `crate::compact::Compactor` etc. resolving; call sites unchanged.

## 4. Workstreams & Goals

| Goal | WS | Title | Deps |
|---|---|---|---|
| 329 | 0 | `compact.rs` → `compact/` directory (pure `git mv`) | — |
| 330 | 1a | Unify `should_compact` predicate; fix intra-turn token-threshold bug; thread `last_prompt_tokens` into `RunCore` | 329 |
| 331 | 1b | Circuit breaker (`MAX_CONSECUTIVE_COMPACT_FAILURES=3`) + compaction failure → best-effort (not turn-fatal) | 330 |
| 332 | 2a | `Microcompactor`: no-LLM count-based proactive tool-result pruning (intra-turn) | 329 |
| 333 | 2b | Wire microcompact into cross-turn + CLI/TUI env parity | 332, 330 |
| 334 | 3a | Re-inject recently-read files post-compact (`ReadFileState.recent_files`) | 329 |
| 335 | 3b | Re-inject invoked skills (transcript-scan `LoadSkill` calls) | 334 |
| 336 | 5a | `CompactionBoundary` cache hit/miss telemetry | 329 |
| 337 | 4a | PTL self-rescue: `truncate_head_for_retry` + retry loop in `Compactor::compact` | 329, 330 |
| 338 | 6a | Recompaction-in-chain tracking telemetry | 336 |
| 339 | 6b | Free-text fallback → 9-section prompt + `<analysis>` strip | 329 |
| 340 | 3c | Re-inject pending plan + todo list | 335 |
| 341 | 5b | Cache-preserving splice validation (mock) + go/no-go data | 336 |
| 342 | 4b | `/compact-before` `/compact-after` partial TUI commands | 334, 335, 340 |

### 4.1 WS-1 — Unify + Circuit Breaker (330, 331)

**330** extracts `Compactor::should_compact(estimate_chars, last_prompt_tokens)`
— the exact logic currently inline at `runtime.rs:378` — and calls it from
both `run_core::maybe_compact` and `runtime::maybe_compact_cross_turn`. Fixes
the intra-turn bug (was char-only). Adds `RunCore::last_prompt_tokens`
captured from `completion.usage` at `run_core.rs:203`.

**331** adds `consecutive_compact_failures` to `RunCore` and `AgentRuntime`.
After 3 consecutive compaction `Err`s, proactive compaction is skipped
(`CompactionSkipped{CircuitBreaker}`). Compaction `Err` becomes best-effort
(log + count + emit, not propagate) — **deliberate behavior change**: compaction
is an optimization, not a correctness requirement; a failed compact must not
crash an otherwise-healthy turn. Emergency `compact_on_overflow` is exempt.

### 4.2 WS-2 — Microcompact (332, 333)

**332** adds `Microcompactor { trigger_tool_count: 12, keep_recent: 4 }` in
`compact/micro.rs`. When `Role::Tool` message count exceeds the trigger, the
oldest tool results (beyond `keep_recent`) have their `content` replaced
with `MICROCOMPACT_PLACEHOLDER` (identical to `run_core::TRIM_PLACEHOLDER`).
**No message is removed** → tool-call pairing is structurally preserved.
No tool-name allowlist (Recursive's `MIN_PRUNE_LENGTH=200` protects small
Edit/Write results; simpler than fake-cc's `COMPACTABLE_TOOLS`). Runs
intra-turn before `maybe_compact`; if it drops the transcript back under
threshold, the LLM summary is skipped entirely. Wired via
`RECURSIVE_MICROCOMPACT_TRIGGER` / `RECURSIVE_MICROCOMPACT_KEEP` env.

**333** runs the same `Microcompactor::prune` at the top of
`maybe_compact_cross_turn` before the `should_compact` decision, so the
cross-turn path benefits from the same no-LLM relief. CLI + TUI builders
share one env contract.

### 4.3 WS-3 — Post-compact Re-injection (334, 335, 340)

All three emit only `Role::System` attachments → no orphan tool result
possible (#8 safe). Inserted after the summary, before the preserved tail:
`[summary, file-atts, skill-atts, plan/todo-atts, ...preserved]`.

**334** `FileReinjector`: top-5 recently-read files from
`ReadFileState::recent_files` (new `&self` accessor), 50K total / 5K per file
budget, head-truncated, deduped by path-substring heuristic against the
preserved tail.

**335** `SkillReinjector`: scan pre-compact transcript for `LoadSkill`
(`load_skill` alias) tool calls, collect distinct names in invocation order,
look up bodies in the discovered `Vec<Skill>` the builder already computes,
25K total / 5K per skill, head-truncated. **No new runtime state** — invoked
set is recovered by scan.

**340** `PlanTodoReinjector`: pending plan via new
`PlanApprovalGate::pending_plan()` accessor; todos via
`AgentRuntime::current_todos()`. `RwLock` errors handled by skip + warn (no
`unwrap`, #5).

### 4.4 WS-5 — Cache Telemetry & Validation (336, 341)

**336** adds `cache_hit_tokens` / `cache_miss_tokens` to
`CompactionBoundary` (populated from the turn's `TokenUsage`). Pure
telemetry; no behavior change.

**341** is **validation, not rollout**. Key finding documented in the goal:
in Recursive's single-process architecture, splice reordering cannot
preserve the recent prefix — the summary is new content that precedes the
recent region either way, so the cache for the recent tail invalidates
regardless. The real cache-preserving lever is **microcompact** (no content
rewrite at all → cache stays). Goal 341 proves this with a mock provider +
the 336 telemetry data, then issues a go/no-go on any further
splice-reorder or forked-summary-cache-share work. This prevents the team
chasing an optimization that can't pay off here.

### 4.5 WS-4 — PTL Self-rescue & Partial (337, 342)

**337** adds `truncate_head_for_retry` in `compact/retry.rs`: when the
summarization call returns context-window-exceeded, group the to-summarize
slice by `Role::User` boundaries, drop oldest groups (target from parsed
token gap, or fallback 1/5 of groups), keep ≥1 group, prepend a synthetic
`User` marker if the head becomes `Role::Assistant`, and run through
`safe_split_point` to guarantee no orphan `Role::Tool`. Retry loop inside
`Compactor::compact` covers both intra-turn and cross-turn callers
automatically. `MAX_PTL_RETRIES=3`. Reuses existing
`is_context_window_exceeded`.

**342** adds manual `/compact-before <turn>` / `/compact-after <turn>` TUI
commands → `AgentRuntime::compact_partial_before` / `compact_partial_after`.
Both reuse `safe_split_point` and the reinjectors. Manual-only; automatic
paths unchanged. **Touches TUI** → ships in-process harness tests in the
same commit and runs `tui-test-presence.sh` (hard gate) + `tui-mutants.sh`
(advisory) per `CLAUDE.md`.

### 4.6 WS-6 — Recompaction Telemetry & Prompt (338, 339)

**338** tracks `AgentRuntime::last_compact_turn` and adds
`is_recompaction_in_chain` / `turns_since_previous_compact` /
`previous_compact_turn` to `CompactionBoundary`. Surfaces "compacted again
K turns after the last" so pathological chains become visible and can drive
threshold tuning.

**339** upgrades the free-text fallback prompt (used only when structured
output is unavailable/invalid) to a 9-section template (Primary Request /
Key Concepts / Files & Code / Errors / Problem Solving / All user messages /
Pending Tasks / Current Work / Optional Next Step) with an `<analysis>`
drafting scratchpad stripped by `format_compact_summary` before the summary
enters context. The structured path is untouched and still preferred. The
`<analysis>` block is the key quality lever — it improves the summary without
consuming post-compact context.

## 5. Invariant Compliance

| # | Invariant | How each goal respects it |
|---|---|---|
| 1 | Loop stays small | All new logic in `src/compact/*` or `runtime.rs` methods; `run_inner` gains at most one extra method call (`self.microcompact(step)`) adjacent to existing `self.maybe_compact(step)`, no new branch. |
| 3 | Sandbox | No fs/shell changes; reinject reads only already-resolved paths from `ReadFileState`. |
| 5 | No `unwrap`/`expect` | All `RwLock`/`Mutex`/parse results handled with `match`/`?`/skip-on-err. |
| 7 | Finish reasons are data | No new `Error` variants; compaction failure becomes best-effort (331), not a finish reason. |
| 8 | Tool-call pairing | Microcompact only swaps `content` (never removes); reinject emits only `Role::System`; PTL retry + partial reuse `safe_split_point`. `tests/invariants/tool_call_pairing.rs` run per goal. |

## 6. Behavior Changes (call out in review)

1. **331**: compaction failure transitions from **turn-fatal** to
   **best-effort**. A failed compact no longer fails the turn; it is logged,
   counted, and the turn continues. This is intentional (compaction is an
   optimization) and aligns with fake-cc. Documented in 331's journal.
2. **332/333**: with `RECURSIVE_MICROCOMPACT_TRIGGER` unset, sessions now
   auto-prune old tool results where before they did not. Opt out with
   `RECURSIVE_MICROCOMPACT_TRIGGER=0`. Mirrors the 2026-07-21 TUI
   auto-compaction precedent.
3. **334/335/340**: post-compact transcripts gain `Role::System` attachment
   messages (files / skills / plan / todos). Resume readers must tolerate
   them (they are `Role::System`, skip-safe).

## 7. Risks & Mitigations

- **Mid-transcript `Role::System` rejection.** Some providers may reject a
  `System` message after the first. Mitigation: reinject goals fall back to
  `Role::User` with a `[system]` prefix if a mock-provider test fails; prefer
  `System` otherwise. Documented in 334.
- **341's negative result.** If the data shows no cache win from splice
  reordering (expected), 341 explicitly **does not** ship a production
  change — it records the no-go and closes the line of inquiry, relying on
  microcompact as the cache-preserving layer.
- **Partial-compact turn→index mapping (342).** If the TUI doesn't track
  turn boundaries cleanly, accept a message index and name the commands
  `/compact-before-index <i>` to avoid a misleading "turn" arg. Noted in 342.
- **Reinject ordering / double-inject.** Reinject runs cross-turn only in
  v1; intra-turn reinject is a follow-up to avoid double-injection
  complexity.

## 8. Landing Order (self-improve flow)

```
329 → 330 → 332 → 333 → 334 → 336 → 337 → 338 → 339 → 335 → 340 → 341 → 342
```

- 329/330 first: no behavior risk, prerequisite for most.
- 332/333 next: highest ROI (no-LLM layer).
- 334/336 early: 334 is high-value; 336 unblocks 341's data.
- 341 after 336 (needs telemetry data).
- 342 last: depends on multiple reinjectors + touches TUI test gates.

Parallelizable: 332/333 (microcompact) and 334/335/340 (reinject) and
336/338/339 (telemetry/prompt) are three independent threads once 329/330
land.

## 9. Out of Scope

- Forked-agent summary-call cache sharing (fake-cc's
  `tengu_compact_cache_prefix`) — requires provider-side cache-key matching
  across a fork; larger effort, only if 341's data justifies.
- `CONTEXT_COLLAPSE` / `KAIROS` / GrowthBook flag system — intentionally not
  ported (Recursive uses static config + env).
- Multi-agent / swarm compaction coordination — not in Recursive's
  single-agent scope.
- Intra-turn reinject — follow-up to avoid double-injection; cross-turn
  covers the main growth site.

## 10. Acceptance (per goal, summarized)

Each goal must pass:
```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace
```
TUI-touching goals (342) additionally:
```bash
.dev/scripts/tui-test-presence.sh   # hard gate
.dev/scripts/tui-mutants.sh         # advisory for manual edits
```
Every goal writes a `.dev/journal/manual-<YYYYMMDD>-<tag>.md` entry per
`CLAUDE.md`, with 331 and 341 explicitly documenting their behavior change /
go-no-go decision.

## 11. File Index

Goals: `.dev/goals/329-compact-mod-split.md` … `342-compact-partial-command.md`
Source (after 329): `src/compact/{mod,micro,reinject,retry,prompt}.rs`
Wiring: `src/runtime.rs`, `src/run_core.rs`, `src/event.rs`,
  `crates/recursive-cli/src/cli/builder.rs`,
  `crates/recursive-tui/src/runtime_builder.rs`,
  `crates/recursive-tui/src/commands.rs`, `crates/recursive-tui/src/events.rs`
Reference: `~/Downloads/fake-cc/src/services/compact/`
