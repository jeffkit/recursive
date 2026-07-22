# Goal 337 — Self-rescue when the compaction request itself hits prompt-too-long

**Roadmap**: Compaction upgrade (WS-4a — unblock stuck over-limit sessions)

**Design principle check**:
- Implemented as: a `truncate_head_for_retry` helper in `src/compact/retry.rs`
  + a retry loop inside `Compactor::compact`.
- ❌ Does NOT branch inside `src/run_core.rs::RunCore::run_inner`.
- ❌ Does NOT introduce a new `Error` variant (invariant #7) — reuses the
  existing context-window-exceeded detection.

## Why

`Compactor::compact` asks the provider to summarize the older transcript
slice. On an extremely long session that older slice can itself exceed the
provider's context window, so the summarization call returns a
context-window-exceeded error — and the user is stuck: the very operation
meant to shrink the context cannot run.

fake-cc handles this with `truncateHeadForPTLRetry`: drop the oldest
API-round groups from the to-summarize set and retry, up to 3 times,
falling back to dropping 20% of groups when the error's token gap is
unparseable. Recursive has no API-round concept, but it can group by
`Role::User` boundaries and drop the oldest groups until the estimated
size falls below a target, reusing `safe_split_point` to preserve tool-call
pairing.

This is the "unblock the stuck user" escape hatch — lossy but better than a
dead session. It is distinct from `compact_on_overflow` (which compacts the
*live* transcript when a normal turn overflows); this fixes the case where
the *compaction call* itself overflows.

## Scope (do exactly this, no more)

### 1. `src/compact/retry.rs` — new module

```rust
//! Drop oldest message groups from a to-summarize slice so the
//! summarization request itself fits the context window.

use crate::compact::Compactor;
use crate::message::{Message, Role};

pub const MAX_PTL_RETRIES: usize = 3;
/// Fallback drop fraction when no token-gap signal is available.
const FALLBACK_DROP_FRACTION: usize = 5; // drop oldest 1/5 of groups

/// Group `messages` into segments each starting at a `Role::User` (or
/// `Role::System`) message; the first group may start with system/preamble.
/// Returns the group boundaries (Vec of (start, end) index ranges).
pub fn group_by_user_boundary(messages: &[Message]) -> Vec<(usize, usize)> { /* ... */ }

/// Drop the oldest groups until the remaining slice's estimated chars fall
/// below `target_chars` (or, if `target_chars` is `None`, drop
/// `1/FALLBACK_DROP_FRACTION` of groups). Never drop all groups — keep at
/// least one to summarize. Returns `None` if nothing can be dropped (only
/// one group). Reuses `Compactor::safe_split_point` semantics on the
/// resulting head so no tool-call pair is split.
pub fn truncate_head_for_retry(
    messages: &[Message],
    target_chars: Option<usize>,
) -> Option<Vec<Message>> { /* ... */ }
```

`group_by_user_boundary`: iterate; start a new group at each index where
`messages[i].role == Role::User || Role::System` (and i > 0). The first
group starts at 0.

`truncate_head_for_retry`:
1. Group the input. If `< 2` groups, return `None`.
2. Compute `target`: if `Some(t)`, drop oldest groups until cumulative
   dropped chars ≥ (total − t); else drop `max(1, groups/FALLBACK_DROP_FRACTION)`.
3. Keep at least one group. If drop count < 1, return `None`.
4. Slice `groups[drop_count..].flatten()`. If the result starts with
   `Role::Assistant` (because group 0 with the preamble was dropped),
   prepend a synthetic `Message::user("[earlier conversation truncated for
   compaction retry]")` so the first non-system message is a User (provider
   requirement) — mirror fake-cc's `PTL_RETRY_MARKER`.
5. Run the result through a pairing-safe check: ensure no `Role::Tool`
   message at the head (it would be orphaned). If the head is `Role::Tool`
   or `Role::Assistant`-with-tool-calls, retreat via the same logic as
   `Compactor::safe_split_point`. (Simplest: call `safe_split_point` on the
   truncated slice with `keep_n = truncated.len()` and drain the prefix it
   returns — that guarantees a valid head.)

### 2. `src/compact/mod.rs` — register + retry loop

`pub mod retry;` + `pub use retry::{truncate_head_for_retry, MAX_PTL_RETRIES};`.

In `Compactor::compact` (`compact/mod.rs:271`), wrap the summarization call
(the `try_structured_compact` + free-text fallback) in a retry loop:
```rust
let mut to_summarize = older.to_vec();
let mut ptl_attempts = 0;
let summary = loop {
    match self.summarize(provider, &to_summarize, step).await {
        Ok(text) => break text,
        Err(e) if is_context_window_exceeded(&e) => {
            ptl_attempts += 1;
            if ptl_attempts > MAX_PTL_RETRIES { return Err(e); }
            let target = estimate_target_from_error(&e); // parse token gap, or None
            match truncate_head_for_retry(&to_summarize, target) {
                Some(truncated) => to_summarize = truncated,
                None => return Err(e),
            }
        }
        Err(e) => return Err(e),
    }
};
```
Extract the existing `try_structured_compact` + free-text fallback into a
private `summarize(&self, provider, older_text_or_messages, step) -> Result<String>`
helper so the retry loop calls it cleanly. `is_context_window_exceeded` is
already used in `runtime.rs` — reuse it (move/`pub use` it from where it
lives so `compact/mod.rs` can call it; confirm its location by grepping).
`estimate_target_from_error` parses the token gap from the provider error
string if present (best-effort; return `None` on parse failure → fallback
drop fraction).

### 3. Tests

`src/compact/retry.rs`:
- `group_by_user_boundary_splits_on_user_messages`
- `truncate_head_drops_oldest_groups` — 4 groups, target drops 1 → returns
  groups[1..].
- `truncate_head_returns_none_when_one_group`
- `truncate_head_keeps_at_least_one_group` — target would drop all → keeps
  the last group.
- `truncate_head_prepends_user_marker_when_head_is_assistant` — drop group
  0 (preamble) → result starts with a synthetic User marker.
- `truncate_head_preserves_pairing` — input with a tool-call pair spanning
  a group boundary → result has no orphan `Role::Tool` at head (verified via
  `safe_split_point` retreat).
- `truncate_head_fallback_drop_fraction` — `target=None` → drops
  `ceil(groups/5)` groups.

`src/compact/mod.rs`:
- `compact_retries_on_ptl_then_succeeds` — `MockProvider` whose first
  summarization call returns a context-window-exceeded `Err`, second
  returns a valid summary; assert `compact` returns `Ok` with the summary
  and the provider was called twice.
- `compact_gives_up_after_max_retries` — provider always returns PTL `Err`
  → `compact` returns `Err` after `MAX_PTL_RETRIES+1` calls.
- `compact_non_ptl_error_propagates_immediately` — a non-PTL `Err` → no
  retry, propagates on first call.

## Acceptance

- `cargo test --workspace` green; clippy clean; fmt clean.
- A PTL error during summarization no longer dead-ends; up to 3 retries
  with head truncation.
- Non-PTL errors propagate immediately (no retry).
- `tests/invariants/tool_call_pairing.rs` green (truncation preserves
  pairing).

## Notes for the agent

- **Reuse `is_context_window_exceeded`** — find its definition (grep) and
  make it accessible to `compact/mod.rs` (it may already be `pub` in
  `runtime.rs` or `error.rs`; if private, expose it via a small `pub use`
  or move to `error.rs`). Do not duplicate the detection logic.
- **Token-gap parsing is best-effort.** Providers format the gap
  differently (OpenAI/DeepSeek vs Anthropic). Parse what you can; on
  failure return `None` and use the fallback drop fraction. Do not
  over-invest in per-provider parsers — the fallback is safe.
- The retry loop is inside `Compactor::compact`, so it covers both the
  intra-turn (`run_core::maybe_compact`) and cross-turn
  (`runtime::maybe_compact_cross_turn`) callers automatically — no
  per-call-site change needed.
- This goal does NOT change `compact_on_overflow` (that path already
  handles the live-turn overflow; the PTL self-rescue is for the
  *summarization* call).
- **DO NOT modify** `src/run_core.rs`, `src/runtime.rs` (beyond any
  `is_context_window_exceeded` visibility tweak), `src/llm/`,
  `src/kernel.rs`, or tool files.
- Journal entry: `.dev/journal/manual-<YYYYMMDD>-compact-ptl-retry.md`.
