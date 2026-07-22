# Goal 331 — Add a circuit breaker for proactive compaction failures

**Roadmap**: Compaction upgrade (WS-1b — stop retrying doomed compaction)

**Design principle check**:
- Implemented as: a `consecutive_compact_failures: u32` counter on `RunCore`
  and `AgentRuntime`, checked at the top of the proactive compaction paths.
- ❌ Does NOT branch inside `src/run_core.rs::RunCore::run_inner` — only the
  bodies of `maybe_compact` / `maybe_compact_cross_turn` change.
- ❌ Does NOT introduce a new `Error` variant (invariant #7). Compaction
  failures become best-effort (logged + counted + event-emitted), not fatal
  to the turn — see "Behavior change" below.

## Why

When the provider is irrecoverably over context (e.g. `prompt_too_long` that
compaction itself cannot fix, or a transient network/JSON error), the
proactive compaction paths retry on every step (intra-turn) and every turn
(cross-turn). On a stuck session this hammers the API with doomed
compaction attempts — fake-cc measured ~250K wasted API calls/day globally
from this, and capped it with `MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES = 3`.

Recursive has no such cap today. A single malformed provider response or an
over-limit context that compaction cannot shrink triggers unbounded
compaction retries for the rest of the session.

Additionally, Recursive currently treats a compaction `Err` as fatal to the
turn (`self.maybe_compact(step).await?` propagates). Compaction is an
optimization, not a correctness requirement — a failed compaction should
not crash an otherwise-healthy turn. fake-cc treats autocompact as
best-effort (catch, count, continue). This goal aligns that semantic.

## Scope (do exactly this, no more)

### 1. Constant

In `src/compact/mod.rs`, add:
```rust
/// Stop attempting proactive compaction after this many consecutive
/// failures. Emergency compaction (`compact_on_overflow`) is exempt —
/// it is the last-resort recovery for an already-failed turn.
pub const MAX_CONSECUTIVE_COMPACT_FAILURES: u32 = 3;
```

### 2. `src/run_core.rs` — best-effort + circuit breaker in `maybe_compact`

- Add field `consecutive_compact_failures: u32` to `RunCore`, init `0` next
  to `last_prompt_tokens` (added in goal 330).
- At the top of `maybe_compact` (`run_core.rs:621`), after resolving the
  compactor, add the breaker check:
  ```rust
  let compactor = match &self.compactor {
      Some(c) => c,
      None => return Ok(()),
  };
  if self.consecutive_compact_failures >= MAX_CONSECUTIVE_COMPACT_FAILURES {
      self.emit(AgentEvent::CompactionSkipped {
          step,
          reason: CompactionSkipReason::CircuitBreaker,
      });
      return Ok(());
  }
  ```
- Wrap the existing `compactor.apply_to_transcript(...)` call so that:
  - On `Err(e)`: `tracing::warn!(error=%e, "proactive compaction failed");`
    increment `self.consecutive_compact_failures`; emit
    `AgentEvent::CompactionSkipped { step, reason: CompactionSkipReason::Error }`;
    return `Ok(())` (do NOT propagate the error).
  - On `Ok(Some(_))` (compaction ran): reset
    `self.consecutive_compact_failures = 0`; dispatch `PostCompact` and
    emit `Compacted` as today.
  - On `Ok(None)` (transcript too short): leave the counter unchanged and
    return `Ok(())`.
- Change the call site `self.maybe_compact(step).await?` at `run_core.rs:941`
  to `self.maybe_compact(step).await;` — it no longer returns a propagable
  error. (Keep the `?` only if `maybe_compact` still returns `Result<()>`
  for the `None`-compactor early return; the point is no `Err` escapes.)

### 3. `src/runtime.rs` — same pattern in `maybe_compact_cross_turn`

- Add field `consecutive_compact_failures: u32` to the `AgentRuntime` state
  holding the compactor (mirror the existing `compactor: Option<Compactor>`
  field location).
- Apply the same breaker check + best-effort `Err` handling as above.
- **Do NOT** change `compact_on_overflow` (`runtime.rs:433`) — emergency
  compaction remains fatal-ish: if it fails, the caller propagates the
  original context-exceeded error. The breaker is for *proactive* paths only.

### 4. Event + enum

In `src/event.rs`, add to `AgentEvent`:
```rust
CompactionSkipped { step: usize, reason: CompactionSkipReason },
```
and add:
```rust
pub enum CompactionSkipReason {
    CircuitBreaker,
    Error,
}
```
Both `Debug`+`Clone`+`PartialEq` to match the existing event variants. If
`AgentEvent` already has a `Compacted` variant, place `CompactionSkipped`
adjacent to it. Update every `match` on `AgentEvent` across the codebase
(TUI sink, HTTP sink, journal writer, tests) to handle the new variant —
most can use it in an existing catch-all or log it. Grep
`AgentEvent::Compacted` to find the match sites.

### 5. Tests

In `src/run_core.rs` tests:
- `circuit_breaker_skips_after_max_failures` — use a `MockProvider` whose
  `complete_structured` / `complete` returns `Err` for the compaction call;
  run enough steps to exceed `MAX_CONSECUTIVE_COMPACT_FAILURES`; assert the
  provider is no longer called for compaction on subsequent steps and
  `CompactionSkipped{CircuitBreaker}` is emitted.
- `circuit_breaker_resets_on_success` — fail twice, then succeed, then fail
  again; assert the counter reset to 0 after the success (the second streak
  starts fresh).
- `compaction_failure_does_not_fail_turn` — a compaction `Err` no longer
  propagates; the turn continues and finishes with a normal finish reason.

In `src/runtime.rs` tests:
- `cross_turn_circuit_breaker_skips` — mirror the above for the cross-turn
  path.

## Acceptance

- `cargo test --workspace` green; `cargo clippy --all-targets --all-features
  -D warnings` clean; `cargo fmt --all` clean.
- A compaction `Err` in `maybe_compact` / `maybe_compact_cross_turn` does NOT
  propagate to the caller (turn no longer fails).
- After 3 consecutive compaction failures, subsequent proactive
  compaction attempts are skipped and emit `CompactionSkipped{CircuitBreaker}`.
- `compact_on_overflow` is unchanged (still returns the original error on
  failure; no breaker).
- All `AgentEvent` match arms compile (no non-exhaustive warnings).

## Notes for the agent

- **Behavior change (intended):** compaction failures are now best-effort,
  not turn-fatal. This is a deliberate alignment with fake-cc and the
  principle that compaction is an optimization. Document this in the
  journal. If a reviewer objects, the fallback is to gate this behind
  `RECURSIVE_COMPACT_BEST_EFFORT=1` (default on) — but prefer the
  unconditional change with the journal rationale.
- The breaker counts *consecutive* failures. A single success resets it.
  `Ok(None)` (transcript too short to compact) is NOT a failure and must NOT
  increment the counter — otherwise a short transcript would exhaust the
  breaker without ever failing.
- `MAX_CONSECUTIVE_COMPACT_FAILURES = 3` matches fake-cc. Make it a `pub const`
  so a later goal could expose it via env if needed (do NOT add the env in
  this goal).
- When wrapping the `Err`, use `tracing::warn!`, not `error!` — compaction
  failure is expected under provider pressure, not a crash.
- **DO NOT modify** `src/llm/`, `src/kernel.rs`, `src/compact_on_overflow`
  logic, `src/http/` request handlers (only the event match arms if they
  exhaustively match), or tool files.
- Journal entry: `.dev/journal/manual-<YYYYMMDD>-compact-circuit-breaker.md`,
  explicitly calling out the fatal→best-effort behavior change.
