# Goal 260 — Pass transcript tail to goal evaluator (M-2)

**Roadmap**: Code quality — architecture review follow-up (P2 backlog)

**Design principle check**:
- Implemented as: tail-slice the transcript before passing to the judge LLM
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

Architecture review (`docs/review/architecture-review-2026-06-07.md`,
item M-2) identified a latency/cost bug in the goal-loop judge call.
At `src/runtime.rs:872`:

```rust
let verdict = evaluator.evaluate(&condition, self.transcript()).await?;
```

The judge (`GoalEvaluator::evaluate`) is an LLM call that decides
whether the goal condition is met. The signature already takes
`transcript: &[Message]` — a slice — so the API is right. The
**caller** is wrong: it passes `self.transcript()` (the entire
transcript) instead of a tail slice.

The transcript grows by 2-3 messages per turn (user prompt + assistant
reply + tool result), so after `max_turns=20` turns the judge call
ships ~60 messages. Worse, after compaction the judge also sees the
compaction summary text, doubling the payload on every turn. This
compounds with `max_turns`: the longer the loop runs, the more
expensive the judge becomes.

The judge is supposed to evaluate **recent progress** — "did the last
few turns move toward the condition?" A tail slice is the correct
input. The full transcript is only useful for replay/debugging, not
for the judge's per-turn decision.

## Scope (do exactly this, no more)

### 1. `src/runtime.rs` — pass a tail slice to the evaluator

Replace the call at line 872:

```rust
let verdict = evaluator.evaluate(&condition, self.transcript()).await?;
```

with:

```rust
let tail = self.transcript_tail(GOAL_EVAL_TRANSCRIPT_TAIL);
let verdict = evaluator.evaluate(&condition, tail).await?;
```

(`GOAL_EVAL_TRANSCRIPT_TAIL` is a private const at the top of the
`impl AgentRuntime` block — see step 2.)

### 2. `src/runtime.rs` — add `transcript_tail` accessor and the const

Add a private const near the top of the `impl AgentRuntime` block
(or at module scope, whichever is idiomatic — look at how other
nearby tunables like `MAX_QUEUED_MESSAGES` are defined):

```rust
/// How many of the most-recent transcript messages to send to the
/// goal-evaluator judge on each turn. Recent progress is what the
/// judge needs; full history is only useful for replay. Default 12
/// covers ~3-4 turns of context. Configurable via
/// `RECURSIVE_GOAL_EVAL_TAIL` (parse as usize, default 12).
const GOAL_EVAL_TRANSCRIPT_TAIL: usize = 12;
```

Then add the accessor next to the existing `pub fn transcript()`:

```rust
/// Return the most-recent `n` transcript messages, or the full
/// transcript if `n >= transcript.len()`. Used by the goal-loop
/// judge to keep the per-turn payload bounded as the transcript
/// grows.
pub fn transcript_tail(&self, n: usize) -> &[Message] {
    let len = self.transcript.len();
    if n >= len {
        &self.transcript
    } else {
        &self.transcript[len - n..]
    }
}
```

(Note: the field is `self.transcript` per the existing `transcript()`
accessor at `src/runtime.rs:622`. If the field is private, use
`self.transcript()` or a clone-of-slice as appropriate.)

### 3. `src/runtime.rs` — env var override (optional, small)

Allow the tail size to be overridden at startup for tests/benchmarking:

```rust
// At the top of run_goal_loop or wherever the const is read:
let tail = std::env::var("RECURSIVE_GOAL_EVAL_TAIL")
    .ok()
    .and_then(|s| s.parse::<usize>().ok())
    .unwrap_or(GOAL_EVAL_TRANSCRIPT_TAIL);
let tail = self.transcript_tail(tail);
let verdict = evaluator.evaluate(&condition, tail).await?;
```

If the env var adds too much noise, skip this step and use the const
directly. The 12-message default is a reasonable choice; tests can
override by creating a runtime with a small `max_turns`.

### 4. Tests

Add unit tests next to the existing goal-loop tests (search for
`run_goal_loop` in `src/runtime.rs`):

- `transcript_tail_returns_full_when_n_exceeds_len`: build a runtime
  with a 3-message transcript, call `transcript_tail(10)`, assert
  it returns all 3.
- `transcript_tail_returns_last_n`: build a runtime with a 5-message
  transcript, call `transcript_tail(2)`, assert it returns messages
  [3] and [4] (the last 2).
- `transcript_tail_handles_zero`: call `transcript_tail(0)`, assert
  it returns an empty slice (no panic).
- (if step 3 is implemented) `goal_evaluator_receives_tail_only`:
  start a goal loop with a 20-message transcript and verify the
  judge's input is bounded. The simplest version: in the existing
  `run_goal_loop_*` tests, after one judge call, log the input
  length or use a `MockProvider` that asserts the message count.

### 5. Verify

```bash
cargo test --lib runtime::
cargo test --lib runtime_goal::
cargo test --bin recursive
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all
```

All must be clean. The change must not regress existing goal-loop
tests (`run_goal_loop_*`, `drain_queue_*`, etc.).

## Acceptance

- `run_goal_loop` passes a tail slice of the transcript to
  `GoalEvaluator::evaluate`, not the full transcript
- `transcript_tail` is a new public method on `AgentRuntime` with
  well-defined edge-case behavior (full when n >= len, empty when
  n == 0, last n otherwise)
- The default tail size is 12 messages (~3-4 turns)
- A new test or two cover the new accessor
- `GoalEvaluator::evaluate` signature is unchanged (it already takes
  a slice — the fix is in the caller)
- All quality gates clean

## Notes for the agent

- Read `src/runtime.rs:622` to confirm the field name (`transcript`)
  and how the existing `transcript()` accessor returns it.
- The `GoalEvaluator` lives in `src/runtime_goal.rs`; do NOT change
  its signature. The fix is entirely on the caller side.
- `self.transcript` may be `Vec<Message>` or `Arc<Vec<Message>>` —
  handle both. If it's `Arc<Vec<Message>>`, return `&[Message]` via
  the inner Vec.
- The 12-message default is a guess. If you find a similar tunable
  already in the codebase, follow the same pattern (e.g. is there a
  `MAX_TRANSCRIPT_TAIL` or similar? grep first).
- DO NOT add a `transcript_head` accessor — keep the diff small. Only
  `transcript_tail` is needed.
- DO NOT change compaction. Compaction reducing the transcript is
  orthogonal — this goal just bounds the judge's per-turn input.
- DO NOT add observability/metrics for the tail length. If the user
  wants metrics, that's a separate goal.

## Out of scope (DO NOT do these)

- Don't change `GoalEvaluator::evaluate`'s signature.
- Don't add a `transcript_head` or other accessor.
- Don't change compaction (`src/compact.rs`).
- Don't add metrics for tail length or judge payload size.
- Don't refactor `run_goal_loop` more broadly — the change is at
  line 872 (one call) plus the new accessor.
