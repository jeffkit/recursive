# Goal 332 — Add a no-LLM `Microcompactor` for intra-turn tool-result pruning

**Roadmap**: Compaction upgrade (WS-2a — proactive count-based pruning)

**Design principle check**:
- Implemented as: a new `Microcompactor` in `src/compact/micro.rs`, invoked
  from `run_core.rs` once per step **before** `maybe_compact`.
- ❌ Does NOT branch inside `src/run_core.rs::RunCore::run_inner` — adds one
  method call (`self.microcompact(step)`) adjacent to the existing
  `self.maybe_compact(step)` call, not a new `if`/`match` arm in the loop.
- ❌ Does NOT remove tool messages — it replaces their `content` with a
  placeholder, so tool-call ↔ tool-result pairing (invariant #8) is
  preserved: the `Role::Tool` message stays at its index, only its text
  changes.

## Why

Today the only intra-turn context relief is `maybe_trim_transcript`
(`run_core.rs:583`), which fires **reactively** only when the transcript
exceeds the hard `max_transcript_chars` budget, and replaces large tool
results with `TRIM_PLACEHOLDER`. It is char-budget-driven and last-resort.

Long sessions accumulate many bulky tool results (Read, Bash, Grep, Glob,
SearchFiles outputs) long before the char budget trips, so the agent keeps
paying for re-sending stale tool output every step. fake-cc's microcompact
clears old tool results **proactively** by tool-result *count* (not char
budget), keeping the most recent N verbatim, with **no LLM call** — it is
far cheaper than the LLM-summary compaction in `maybe_compact`.

This goal adds that proactive count-based layer for the intra-turn path.
It runs before `maybe_compact`, so after pruning the transcript may drop
back under the compaction threshold and the expensive LLM summary is
skipped entirely. The existing `maybe_trim_transcript` char-budget backstop
stays untouched as the hard limit.

## Scope (do exactly this, no more)

### 1. `src/compact/micro.rs` — new module

```rust
//! No-LLM proactive pruning of old tool results by count.

use crate::message::{Message, Role};

/// Placeholder reused from `run_core::TRIM_PLACEHOLDER` semantics.
/// Re-exported here so the microcompactor and the char-budget trim use the
/// same marker text.
pub const MICROCOMPACT_PLACEHOLDER: &str = "[older tool output trimmed to fit budget]";

/// Minimum tool-result size worth pruning; shorter results are kept verbatim
/// (matches `run_core::MIN_TRIM_LENGTH`).
pub const MIN_PRUNE_LENGTH: usize = 200;

#[derive(Debug, Clone)]
pub struct Microcompactor {
    /// Prune when the number of `Role::Tool` messages exceeds this.
    pub trigger_tool_count: usize,
    /// Keep this many most-recent tool results verbatim.
    pub keep_recent: usize,
}

impl Default for Microcompactor {
    fn default() -> Self {
        Self { trigger_tool_count: 12, keep_recent: 4 }
    }
}

impl Microcompactor {
    /// Prune oldest tool-result contents in place. Returns the number pruned.
    /// Does NOT remove messages — only replaces `content` of `Role::Tool`
    /// messages older than `keep_recent` when total tool count exceeds
    /// `trigger_tool_count`. Tool messages shorter than `MIN_PRUNE_LENGTH`
    /// are left untouched (not worth the placeholder swap).
    pub fn prune(&self, messages: &mut [Message]) -> usize { /* ... */ }
}
```

`prune` logic:
1. Collect indices of all `Role::Tool` messages.
2. If `count <= self.trigger_tool_count`, return `0`.
3. Otherwise, the candidates for pruning are all tool messages **except**
   the last `self.keep_recent`. Iterate them oldest-first; for each whose
   `content.len() > MIN_PRUNE_LENGTH` and whose content is not already the
   placeholder, replace `content` with `MICROCOMPACT_PLACEHOLDER`. Stop once
   `count - pruned <= self.trigger_tool_count` (prune only enough to get
   back under the trigger — don't prune everything eligible).
4. Return the number pruned.

No LLM call. No provider access. Pure transcript mutation.

### 2. `src/compact/mod.rs` — register the module

Add `pub mod micro;` and `pub use micro::{Microcompactor, MICROCOMPACT_PLACEHOLDER};`
at the top of `compact/mod.rs` alongside the existing items.

### 3. `src/run_core.rs` — wire intra-turn

- Add field `microcompactor: Option<Microcompactor>` to `RunCore`, init
  `None` by default. Add a builder setter `pub fn microcompactor(mut self, m: Microcompactor) -> Self`
  on the `RunCore` builder (mirror the existing `compactor(...)` setter).
- Add a method:
  ```rust
  fn microcompact(&mut self, step: usize) {
      let Some(m) = &self.microcompactor else { return; };
      let pruned = m.prune(Arc::make_mut(&mut self.messages));
      if pruned > 0 {
          self.emit(AgentEvent::Microcompact { step, pruned });
      }
  }
  ```
- Call it in the step loop at `run_core.rs:940`, immediately **before**
  `self.maybe_compact(step).await;` (note: after goal 331, `maybe_compact`
  no longer uses `?`):
  ```rust
  // ---- microcompact (no-LLM proactive prune) -----------------------------
  self.microcompact(step);
  // ---- compaction -------------------------------------------------------
  self.maybe_compact(step).await;
  ```
- Do NOT remove or change `maybe_trim_transcript` — it remains the
  char-budget hard backstop invoked from `enforce_transcript_budget`'s
  neighborhood.

### 4. `src/event.rs` — new event

Add to `AgentEvent`:
```rust
Microcompact { step: usize, pruned: usize },
```
`Debug`+`Clone`+`PartialEq` like siblings. Update all `AgentEvent` match
arms (grep `AgentEvent::Compacted`).

### 5. Builder wiring (`crates/recursive-cli/src/cli/builder.rs`)

In `build_runtime`, after the existing `RECURSIVE_COMPACT_THRESHOLD` block
that constructs the `Compactor`, add a parallel block reading:
- `RECURSIVE_MICROCOMPACT_TRIGGER` (parse `usize`; `0` = disabled → pass
  `None`; unset = default `12`; positive = explicit).
- `RECURSIVE_MICROCOMPACT_KEEP` (parse `usize`; unset = default `4`).

When trigger > 0, construct `Microcompactor { trigger_tool_count, keep_recent }`
and pass via the builder's `microcompactor(...)` setter. Mirror the exact
env-parsing discipline of the `RECURSIVE_COMPACT_THRESHOLD` block (the
`Ok("0")|Ok("off")|Ok("false") => disabled` pattern). Put the env-parse
logic in a `fn build_microcompactor_from_env(raw: Option<String>, keep_raw: Option<String>) -> Option<Microcompactor>`
helper so it can be unit-tested without touching the process environment
(same pattern as the TUI `build_compactor_from_env` in
`crates/recursive-tui/src/runtime_builder.rs`).

### 6. Tests

In `src/compact/micro.rs` `#[cfg(test)] mod tests`:
- `prune_noop_when_under_trigger` — 5 tool messages, trigger 12 → 0 pruned.
- `prune_oldest_keeps_recent` — 15 tool messages, trigger 12, keep 4 → the
  last 4 are untouched, the oldest eligible ones are placeholdered, and
  pruning stops once count drops to trigger (so exactly 3 pruned, not all 11).
- `prune_preserves_tool_messages` — after pruning, the message count is
  unchanged and every pruned index is still `Role::Tool` (pairing intact).
- `prune_skips_short_results` — a tool result under `MIN_PRUNE_LENGTH` is
  not placeholdered even if it's old.
- `prune_idempotent` — running `prune` twice on the same transcript does
  nothing the second time (already-placeholdered content is skipped).
- `build_microcompactor_from_env` — disabled when `0`/unset-vs-set
  semantics (one sequential test, per the env-race rule in `.dev/AGENTS.md`).

In `src/run_core.rs` tests:
- `microcompact_fires_before_compact_and_skips_summary` — configure a
  `Microcompactor` with a low trigger and a `Compactor` with a char
  threshold just above the post-prune size; assert microcompact prunes
  enough that `maybe_compact` does NOT call the provider (the LLM summary
  is skipped). Use `MockProvider::calls()` to assert zero compaction calls.

## Acceptance

- `cargo test --workspace` green; `cargo clippy --all-targets --all-features
  -D warnings` clean; `cargo fmt --all` clean.
- `Microcompactor::prune` never removes a message and never changes a
  non-`Tool` message (verified by the preserve-pairing test).
- With `RECURSIVE_MICROCOMPACT_TRIGGER=0`, no microcompactor is configured
  and behavior is identical to today.
- `maybe_trim_transcript` is untouched (still the char-budget backstop).

## Notes for the agent

- **No tool-name filtering.** Unlike fake-cc's `COMPACTABLE_TOOLS` set,
  Recursive prunes all `Role::Tool` messages equally. Rationale: Recursive's
  bulky results are Read/Bash/Grep/Glob/SearchFiles; Edit/Write results are
  tiny and protected by `MIN_PRUNE_LENGTH` anyway. Adding a tool-name allowlist
  would require mapping `tool_call_id → tool_name` across the
  Assistant/Tool pair, which is extra coupling for no gain. Keep it simple.
- **Pairing safety (invariant #8):** because we only swap `content` and
  never `drain`/`remove`, the `Role::Tool` message stays immediately after
  its parent `Role::Assistant` with the matching `tool_calls` id. No orphan
  is possible. The `tests/invariants/tool_call_pairing.rs` suite must still
  pass — run it explicitly.
- The `MICROCOMPACT_PLACEHOLDER` text is intentionally identical to
  `run_core::TRIM_PLACEHOLDER` so the transcript reads consistently whether
  the prune came from microcompact or the char-budget trim. Do not invent a
  different marker.
- `prune` takes `&mut [Message]`; the caller passes
  `Arc::make_mut(&mut self.messages)` so the COW clone-on-write is respected
  (same pattern as `maybe_trim_transcript`).
- **DO NOT modify** `src/runtime.rs` cross-turn path in this goal (that is
  goal 333), `src/llm/`, `src/kernel.rs`, `maybe_trim_transcript`, or tool
  files. The TUI `runtime_builder.rs` microcompactor wiring is a separate
  follow-up (mirror the CLI builder change) — include it only if trivial;
  otherwise note it as a follow-up in the journal.
- Journal entry: `.dev/journal/manual-<YYYYMMDD>-compact-microcompact-intra.md`.
