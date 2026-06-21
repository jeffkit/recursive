# Manual edit: cache-hit-rate

**Date**: 2026-06-21
**Goal**: Fix the TUI status-bar cache-hit percentage. It showed >100%
(old denominator was `total_input`) and, after a previous patch, was stuck
at ~100% (denominator `total_cache_hit + total_cache_miss` accumulated over
the whole session — the cached prefix is re-read every step, so misses get
drowned out). Two root causes:

1. **Provider semantic mismatch.** DeepSeek reports
   `prompt_tokens = cache_hit + cache_miss`, so `hit/(hit+miss)` is the true
   per-prompt rate. Anthropic reports `input_tokens`, `cache_read_input_tokens`
   and `cache_creation_input_tokens` *separately*; the parser only put
   `cache_creation` into `cache_miss_tokens`, so the denominator excluded the
   fresh uncached input → wrong rate.
2. **Cumulative vs per-turn.** The bar summed cache tokens across the whole
   session, which trends to ~100% regardless of real per-turn behaviour.

**Fixes**:
- Normalised the cross-provider contract: `cache_hit + cache_miss == total
  input tokens`. Anthropic parser now folds `input_tokens + cache_creation`
  into `cache_miss_tokens` (both streaming + non-streaming paths). Documented
  the invariant on `TokenUsage`. Side benefit: warm-turn cost now charges the
  fresh input tokens that were previously uncounted.
- Added per-turn counters `turn_cache_hit` / `turn_cache_miss` to
  `UsageStats`, accumulated in `record_with_cache`, zeroed by a new
  `begin_turn()` called from the `TurnStarted` event handler.
- Status bar now renders the most-recent-turn rate from the per-turn counters.

**Files touched**:
- `src/llm/chat.rs` (TokenUsage invariant doc)
- `src/llm/anthropic.rs` (streaming + non-streaming usage normalisation, test)
- `src/tui/cost.rs` (per-turn counters, begin_turn)
- `src/tui/app/event_loop.rs` (reset on TurnStarted, test)
- `src/tui/ui/status.rs` (per-turn rate, tests)

**Tests added**:
- `anthropic::parses_usage_correctly` updated to assert `hit+miss == total
  input`.
- `event_loop::turn_started_resets_per_turn_cache_but_keeps_totals`.
- `status::cache_hit_rate_uses_current_turn_not_session_totals`; existing
  cache-rate tests retargeted to the per-turn fields.

**Notes**: Cost accuracy still has minor unaddressed nuances (Anthropic cache
*writes* bill at ~1.25× input and cold-turn creation is charged at 1× via the
`prompt_tokens` else-branch). Deferred — out of scope for the display fix.
Cumulative session hit-rate could be surfaced separately (e.g. in the cost
modal) later.
