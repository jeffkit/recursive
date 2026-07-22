# Goal 330 — Unify the compaction threshold decision into `Compactor::should_compact`

**Roadmap**: Compaction upgrade (WS-1a — fix intra-turn token-threshold bug + DRY)

**Design principle check**:
- Implemented as: a new `Compactor::should_compact` method called from both
  `run_core.rs::RunCore::maybe_compact` and `runtime.rs::AgentRuntime::maybe_compact_cross_turn`.
- ❌ Does NOT branch inside `src/run_core.rs::RunCore::run_inner` — the
  `self.maybe_compact(step).await?` call site is unchanged; only the body of
  `maybe_compact` changes.
- ❌ Does NOT introduce a new `Error` variant (invariant #7).

## Why

The threshold decision is duplicated in two places and they disagree:

1. `src/runtime.rs:378` (`maybe_compact_cross_turn`) prefers the token
   threshold when `threshold_prompt_tokens` is set and `last_prompt_tokens > 0`,
   falling back to the char estimate:
   ```rust
   let should_compact = match (compactor.threshold_prompt_tokens, last_prompt_tokens) {
       (Some(token_threshold), actual) if actual > 0 => actual >= token_threshold,
       _ => Compactor::estimate_chars(&self.transcript) >= compactor.threshold_chars,
   };
   ```

2. `src/run_core.rs:627` (`maybe_compact`, intra-turn) checks **only** the char
   estimate and ignores `threshold_prompt_tokens` entirely:
   ```rust
   let chars = Compactor::estimate_chars(&self.messages);
   if chars < compactor.threshold_chars {
       return Ok(());
   }
   ```

This is a bug: the intra-turn path never uses the more-accurate token
threshold even when the API reports `prompt_tokens`. On CJK workloads the
char estimate (4 chars/token) underestimates token density, so intra-turn
compaction fires too late or never while the model is already over context.

The fix: extract one `should_compact` predicate on `Compactor` that both sites
call. To use it intra-turn, `RunCore` must remember the previous step's
`prompt_tokens` (the per-step `completion.usage` at `run_core.rs:203` is
currently accumulated into `total_usage` then dropped).

## Scope (do exactly this, no more)

### 1. `src/compact/mod.rs` — add `should_compact`

Add a method to `Compactor`:
```rust
/// Decide whether compaction should run, preferring the token-based
/// threshold (accurate, CJK-safe) when actual API `prompt_tokens` are
/// available, falling back to the char estimate.
pub fn should_compact(&self, estimate_chars: usize, last_prompt_tokens: u32) -> bool {
    match (self.threshold_prompt_tokens, last_prompt_tokens) {
        (Some(token_threshold), actual) if actual > 0 => actual >= token_threshold,
        _ => estimate_chars >= self.threshold_chars,
    }
}
```
This is the exact logic currently inline in `runtime.rs:378`, lifted verbatim
into a method so both call sites share it.

### 2. `src/run_core.rs` — thread `last_prompt_tokens` into `maybe_compact`

- Add a field to the `RunCore` struct: `last_prompt_tokens: u32`, initialized
  to `0` in the constructor (find the existing `compactor: None,` init block
  near `run_core.rs:1246` and add `last_prompt_tokens: 0,` alongside it).
- In `dispatch_llm_step` at the `if let Some(u) = completion.usage` block
  (`run_core.rs:203`), after accumulating into `total_usage`, also store the
  per-step value:
  ```rust
  if let Some(u) = completion.usage {
      *total_usage = total_usage.accumulate(u);
      self.last_prompt_tokens = u.prompt_tokens;   // NEW
      self.emit(AgentEvent::Usage { ... });
  }
  ```
- In `maybe_compact` (`run_core.rs:621`), replace the char-only guard:
  ```rust
  // before
  let chars = Compactor::estimate_chars(&self.messages);
  if chars < compactor.threshold_chars {
      return Ok(());
  }
  // after
  let chars = Compactor::estimate_chars(&self.messages);
  if !compactor.should_compact(chars, self.last_prompt_tokens) {
      return Ok(());
  }
  ```
  Keep the `PreCompact` hook dispatch and the rest of the body unchanged.

### 3. `src/runtime.rs` — call the shared predicate

In `maybe_compact_cross_turn` (`runtime.rs:372`), replace the inline `match`
(`runtime.rs:378-381`) with:
```rust
let chars = Compactor::estimate_chars(&self.transcript);
let should_compact = compactor.should_compact(chars, last_prompt_tokens);
if !should_compact {
    return Ok(());
}
```
Remove the now-redundant separate `let chars = Compactor::estimate_chars(...)`
that follows the old match (it was computed twice — once for the decision,
once after). Keep one `chars` computation and pass it to both `should_compact`
and the `PreCompact` hook.

### 4. Tests

In `src/compact/mod.rs` `#[cfg(test)] mod tests`:
- `should_compact_uses_token_threshold_when_available` — `threshold_prompt_tokens=Some(1000)`,
  `last_prompt_tokens=1000` → true; `999` → false.
- `should_compact_falls_back_to_chars_when_tokens_zero` —
  `last_prompt_tokens=0` → uses char threshold.
- `should_compact_falls_back_to_chars_when_threshold_none` —
  `threshold_prompt_tokens=None` → uses char threshold.

In `src/run_core.rs` tests:
- `maybe_compact_uses_token_threshold_intra_turn` — configure a compactor
  with a high `threshold_chars` (so char check alone would NOT fire) and a
  low `threshold_prompt_tokens`; set `core.last_prompt_tokens` above the
  token threshold; assert compaction fires (transcript shrinks). Use
  `MockProvider`. This is the regression test for the bug.

## Acceptance

- `cargo test --workspace` green, including the new tests.
- `cargo clippy --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` clean.
- `run_core.rs::maybe_compact` and `runtime.rs::maybe_compact_cross_turn`
  both call `compactor.should_compact(...)`; neither contains an inline
  threshold `match` or a bare `chars < threshold_chars` guard anymore.
- `RunCore` has a `last_prompt_tokens: u32` field updated from
  `completion.usage` per step.

## Notes for the agent

- The `should_compact` logic must be byte-for-byte equivalent to the existing
  `runtime.rs:378` match — this goal is a refactor + bug fix, not a
  threshold-value change. Do not retune the 0.8 / reserve constants.
- `last_prompt_tokens` is `u32`, not `Option<u32>` — `0` means "no reading
  yet" and the predicate already handles `actual > 0`. No `Option` needed.
- The intra-turn `maybe_compact` runs at the START of each step
  (`run_core.rs:941`, before the LLM call), so `self.last_prompt_tokens`
  holds the PREVIOUS step's reading on step N≥2, and `0` on step 1. That is
  correct: we compact based on the most recent known context size.
- **DO NOT modify** `src/llm/`, `src/kernel.rs`, `src/agent/types.rs`,
  `src/message.rs`, `crates/`, `src/http/`, or any tool file. Only
  `src/compact/mod.rs`, `src/run_core.rs`, `src/runtime.rs` are in scope.
- Do NOT add a circuit breaker in this goal — that is goal 331. This goal is
  strictly the predicate unification + intra-turn token threading.
- Journal entry: `.dev/journal/manual-<YYYYMMDD>-compact-unify-should-compact.md`.
