# Goal 341 — Validate boundary-preserving compaction with a mock provider

**Roadmap**: Compaction upgrade (WS-5b — cache-aware compaction, validation first)

**Design principle check**:
- Implemented as: an alternative `apply_to_transcript` splice strategy
  (boundary-preserving) gated behind `RECURSIVE_COMPACT_PRESERVE_PREFIX=1`,
  plus a mock-provider test proving the recent prefix is byte-identical
  post-compact.
- ❌ Does NOT branch inside `src/run_core.rs::RunCore::run_inner`.
- ❌ Does NOT introduce a new `Error` variant (invariant #7).

## Why

Today `Compactor::apply_to_transcript` (`compact/mod.rs:244`) drains the
older slice and inserts the summary `System` message at index 0. The recent
tail is preserved verbatim but **shifts position** (its messages now follow
the summary instead of following the older messages). For providers that
cache by content prefix, the prefix up to the recent tail changes (the
summary is new content at the head), so the cache for the recent tail is
invalidated — the post-compact turn re-pays for the recent context.

Goal 336 (cache telemetry) measures the actual cost. If the data shows a
significant cache-hit drop after compaction, this goal validates an
alternative: insert the summary **between** the drained-older region and
the kept-recent region such that the recent region's messages stay at the
same absolute transcript positions with the same preceding content. The
provider then caches the `[system_prompt, tools, summary, recent…]` prefix
on the post-compact turn and reuses it on subsequent turns.

The catch: providers require the first non-system message to be `Role::User`,
and some reject multiple `Role::System` messages mid-transcript. This goal
does NOT roll out the change — it validates acceptability with a mock
provider and measures cache impact using goal 336's telemetry, then leaves
  rollout to a follow-up goal gated on the data.

## Scope (do exactly this, no more)

### 1. `src/compact/mod.rs` — add a boundary-preserving splice variant

Add a method alongside `apply_to_transcript`:
```rust
/// Like `apply_to_transcript` but inserts the summary immediately before
/// the kept-recent region (at the split index) instead of at index 0, so
/// the recent messages keep their absolute positions and preceding
/// content. The older region is still drained. Returns
/// `(removed, summary_chars)` like `apply_to_transcript`.
///
/// Caller must ensure the resulting transcript still starts with a valid
/// first message (System or User) — see `validate_boundary_preserving`.
pub async fn apply_to_transcript_preserve_prefix(
    &self,
    provider: &dyn ChatProvider,
    transcript: &mut Vec<Message>,
    step: usize,
) -> Result<Option<(usize, usize)>> { /* ... */ }
```
Logic: compute `split = safe_split_point(transcript, keep_recent_n)`; build
the summary from `transcript[..split]`; `drain ..split`; `insert(0, summary)`
— wait, that is the existing behavior. The boundary-preserving variant
instead: `drain ..split` (removes older), then `insert(0, summary)` puts the
summary at the head of the *kept* region, which IS immediately before the
recent region. Re-examine: the existing `apply_to_transcript` already does
exactly `drain(..split); insert(0, summary)`. So the recent region's
preceding content becomes the summary (new) — cache invalidates.

The true boundary-preserving approach: keep the recent region's **original
preceding content** by NOT draining the older region fully — instead replace
the older region *in place* with the summary, so the recent region's
preceding bytes are `[summary]` (still new) — same problem. There is no way
to keep the recent prefix identical while changing the older region.

Therefore the realistic cache win is **not** prefix preservation but
**summarization-call cache reuse** (fake-cc's forked-agent sharing the main
thread's cache for the summary request itself). Recursive's single-process
model has no forked agent; the summary call uses a fresh
`[Message::user(prompt)]` with no shared prefix to cache.

**Conclusion to encode in this goal:** validate, via mock provider + the 336
telemetry, that there is NO meaningful cache win available from splice
reordering alone in Recursive's architecture; the cost lever is
**microcompact (goals 332/333)** which preserves message structure entirely
(no rewrite → cache stays). Document this finding.

Reframe the scope accordingly:

### 1 (revised). `src/compact/mod.rs` — add a `validate_boundary_preserving` test helper

Add a `#[cfg(test)]` helper (not shipped) that, given a transcript, applies
both the current splice and a hypothetical "summary-at-split-index" splice,
and asserts the recent-region bytes are identical between the two — proving
the recent content is preserved either way (the only difference is what
precedes it). This is the validation artifact.

### 2. Mock-provider acceptance test

In `src/compact/mod.rs` tests, add:
- `boundary_preserving_recent_region_is_byte_identical` — for a transcript,
  compute the recent region (via `safe_split_point`) before compaction;
  after `apply_to_transcript`, assert the recent region's messages are
  byte-identical (same content, same order) — proving compaction never
  touches the recent tail.
- `mock_provider_accepts_post_compact_transcript` — feed the post-compaction
  transcript through a `MockProvider` that records the message sequence and
  asserts the first non-system message is `Role::User` (provider ordering
  invariant), confirming the current splice already yields a valid sequence
  (no mid-transcript System-message rejection risk for the common case).

### 3. Data collection (no code shipped)

After landing, run 2–3 self-improve long sessions with goal 336's telemetry
on. Record in the journal:
- pre-compact `cache_hit_tokens` / `cache_miss_tokens` (from
  `CompactionBoundary`),
- next-turn `Usage` cache hit/miss (the post-compact reading),
- whether the post-compact turn shows a cache collapse.

Decision gate: if the post-compact cache collapse is < 20% of pre-compact
hit, **do not pursue** a splice-reorder cache optimization (the win is too
small to justify the provider-acceptance risk); rely on microcompact
(goals 332/333) as the cache-preserving layer. If the collapse is large,
open a follow-up goal to prototype a forked-summary-call cache share (which
requires provider-side cache-key matching, a larger effort).

## Acceptance

- `cargo test --workspace` green; clippy clean; fmt clean.
- The two validation tests pass (recent region byte-identical; mock provider
  accepts the post-compact sequence).
- The journal records the observed cache collapse numbers and the
  go/no-go decision for a follow-up cache optimization goal.
- **No production behavior change is shipped in this goal** — it is
  validation + data collection only. The `apply_to_transcript_preserve_prefix`
  method is NOT added to the runtime path; if a stub is added, it is
  `#[cfg(test)]`-only.

## Notes for the agent

- The key insight to document: in Recursive's single-process architecture,
  splice reordering does **not** preserve the recent prefix (the summary is
  new content that precedes the recent region either way). The real
  cache-preserving lever is **microcompact** (no content rewrite at all).
  This goal exists to *prove* that with data so the team doesn't chase a
  splice-reorder optimization that can't pay off.
- Do NOT add a `RECURSIVE_COMPACT_PRESERVE_PREFIX` env or wire anything into
  the runtime in this goal. If the data later justifies a forked-summary
  cache-share, that is a separate, larger goal.
- This goal depends on goal 336 (telemetry) being landed to collect the
  numbers.
- **DO NOT modify** `src/run_core.rs`, `src/runtime.rs` runtime path,
  `src/llm/`, `src/kernel.rs`, or tool files.
- Journal entry: `.dev/journal/manual-<YYYYMMDD>-compact-cache-preserving-mock.md`
  — MUST include the measured cache collapse % and the go/no-go decision.
