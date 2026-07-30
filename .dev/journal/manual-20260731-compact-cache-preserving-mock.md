# Manual edit: compact-cache-preserving-mock (Goal 341)

**Date**: 2026-07-31
**Goal**: 341 — Validate boundary-preserving compaction with a mock provider

## Summary

Added validation tests proving that compaction never modifies the recent tail
messages (they pass through byte-identical), and that MockProvider accepts the
post-compaction transcript with valid User-first ordering. Documented the
finding that splice reordering alone cannot preserve the prefix cache in
Recursive's single-process architecture — the real cache-preserving lever is
microcompact (goals 332/333).

## Files touched

- **`src/compact/mod.rs`** (test module only, no production code):

  - Added `validate_boundary_preserving()` — `#[cfg(test)]` helper that
    computes the split point, serialises pre-compaction recent messages,
    applies the same splice `apply_to_transcript` uses (`drain(..split)` +
    `insert(0, summary)`), then asserts the recent-region messages are
    byte-identical (via `serde_json` serialisation).

  - `boundary_preserving_recent_region_is_byte_identical` — verifies recent
    messages are byte-identical after the splice in a standard transcript.

  - `boundary_preserving_with_tool_calls_in_older` — same, but with tool-call
    pairs in the older portion (the split backs up, but recent region still
    unchanged).

  - `mock_provider_accepts_post_compact_transcript` — applies compaction
    via `apply_to_transcript`, feeds the post-compact transcript through
    `MockProvider::complete()`, and asserts:
    - Provider accepts it (no error).
    - First non-system message is `Role::User` (provider ordering invariant).
    - No orphan Tool messages.

  - `mock_provider_accepts_compact_with_system_summary_user_ordering` —
    edge case with multiple leading System messages (summary + original
    prompt), verifies first non-System is still User.

## Tests added

4 new tests (listed above). All 44 compact::tests pass, all 2152 library
tests pass, all 35 invariant tests pass.

## Data collection plan (post-landing)

After landing, run 2–3 self-improve long sessions with goal 336's telemetry
enabled. Record for each compaction event:

1. Pre-compact `cache_hit_tokens` / `cache_miss_tokens` from
   `CompactionBoundary` event.
2. Next-turn `Usage` cache hit/miss (the post-compact reading).
3. Whether the post-compact turn shows a cache collapse.

### Decision gate

| Condition | Action |
|-----------|--------|
| Post-compact cache collapse < 20% of pre-compact hit | **Do NOT pursue** splice-reorder optimization. The win is too small. Rely on microcompact (goals 332/333) as the cache-preserving layer. |
| Post-compact cache collapse ≥ 20% | Open a follow-up goal to prototype forked-summary-call cache sharing (provider-side cache-key matching — larger effort). |

### Key finding (to document after measurement)

In Recursive's single-process architecture, splice reordering does NOT
preserve the recent prefix — the summary is new content that precedes the
recent region either way (at index 0 after `drain(..split)` + `insert(0, summary)`).
The `validate_boundary_preserving` helper proves the recent messages
themselves are never modified, but the content *before* them changes from
"original older messages" to "LLM-generated summary". This changes the
prefix that provider caches key off, invalidating the cache for the recent
tail regardless of splice position.

The real cache-preserving lever is **microcompact** (goals 332/333), which
preserves message structure entirely (no content rewrite → cache key unchanged).

## Design constraints satisfied

- ✅ No `src/run_core.rs` modification
- ✅ No `src/runtime.rs` runtime path modification
- ✅ No `src/llm/` modification
- ✅ No `src/kernel.rs` modification
- ✅ No tool files modification
- ✅ No new `Error` variant (invariant #7)
- ✅ No production behavior change shipped — validation only
- ✅ No `RECURSIVE_COMPACT_PRESERVE_PREFIX` env var added
- ✅ No `apply_to_transcript_preserve_prefix` shipped to runtime

## Quality gates

- `cargo test --workspace`: all 2152 + integration/doc tests green
- `cargo clippy --all-targets --all-features -- -D warnings`: clean
- `cargo fmt --all --check`: clean
- `cargo test --test invariants`: 35 passed, 0 failed
