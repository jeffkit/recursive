# Manual edit: p1-1-run-inner-split

**Date**: 2026-07-05
**Goal**: Land P1-1 from the architecture review — split `RunCore::run_inner`
into sibling phase helpers so the loop body stays small enough that
invariant #1 ("agent loop stays small") is meaningful again. Before
this PR the body was 394 lines and the protective gate added in PR #7
only had to stay under 400; after, the body is 117 lines and the gate
is tightened to 150.
**Branch**: `refactor/run-inner-split` (worktree `.worktrees/p1-run-inner-split`)
**Base**: `f76aacc` (origin/main after PR #6 + PR #7 merged)

## Files touched

- `src/run_core.rs` — seven new helpers on `RunCore<'a>`:
  - `make_outcome(self, ...) -> RunInnerOutcome` — single factory for
    the seven-field outcome struct, replacing six inline construction
    sites. This is the only helper that consumes `self`; everything
    else borrows.
  - `check_shutdown(&self, step, &total_usage) -> Option<(FinishReason, usize)>`
    — top-of-step cancellation check.
  - `enforce_transcript_budget(&mut self, step, &total_usage) -> Option<(FinishReason, usize)>`
    — trim, measure, maybe-terminate with `TranscriptLimit`.
  - `drain_mailbox(&mut self)` — pull coordinator messages into the
    transcript.
  - `handle_no_tool_calls(&mut self, &Completion, step) -> Option<FinishReason>`
    — push assistant message, classify finish (`ProviderStop` vs
    `NoMoreToolCalls`), emit `TurnFinished`.
  - `process_tool_results(&mut self, &results, step, &mut recent_errors,
    &mut tool_audits, &mut skill_injector) -> Option<FinishReason>` —
    sentinel pre-pass, sliding-window stuck detection with deferred
    termination, Globs skill injection. **Invariant #8** (tool_call ↔
    tool_result pairing) preserved: sentinel scans-then-flushes
    atomically, stuck detection pushes every tool_result before
    recording the deferred finish verdict.
  - `dispatch_llm_step(&mut self, &specs, step, &mut total_usage)
    -> Result<(Completion, Option<String>)>` — stream forwarder, LLM
    call, drain forwarder, inline `<think>` extraction, latency/usage
    emit, ordered `Reasoning` then `AssistantText` emit. Does NOT
    touch `self.messages` (caller decides which path takes the
    completion).

- `tests/invariants/loop_size_orthogonality.rs` —
  `run_inner_function_body_stays_small` threshold tightened from
  400 → 150 lines (new baseline is 117). Updated doc-comment to
  reference the P1-1 split and list the extracted helpers.

## Tests added

None. The existing `stuck_detection_*` / `tool_call_pairing::*` /
`runtime::*` tests pin the behaviour of every helper; this PR is a pure
structural refactor and the existing 2030-test suite is the safety net.

## Quality gates

- `cargo test --workspace` — 2030 passed, 0 failed across all crates
- `cargo clippy --all-targets --all-features -- -D warnings` — clean
- `cargo fmt --all --check` — clean
- `cargo test --test invariants` — green at the new 150-line threshold

## Staged commits

Each stage was committed independently with full quality gates run
before the next stage started:

1. `make_outcome` factory — 394 → 375 lines
2. `check_shutdown` — 375 → 356
3. `enforce_transcript_budget` — 356 → 340
4. `drain_mailbox` — 340 → 324
5. `handle_no_tool_calls` — 324 → 314
6. `process_tool_results` — 314 → 195 (the largest single drop)
7. `dispatch_llm_step` — 195 → 117
8. tighten invariant threshold 400 → 150 (this commit)

## Notes (non-obvious decisions)

### Sibling helpers, not a state machine

The architecture review floated both options. Sibling helpers won
because (a) the diff is a series of `let result = self.helper(args);`
extractions with no control-flow restructuring, (b) each stage was
reviewable in isolation, and (c) the loop body remains a literal
`loop { drain_mailbox(); check_budget(); call_llm().await?;
if no_tools { break } dispatch_tools().await?; }` over calls — a state
machine would have introduced a `match state { ... }` that arguably
adds ceremony without clarifying the linear step sequence.

### Why helpers return `Option<...>` instead of `ControlFlow<...>`

`run_inner` is `async fn(mut self) -> Result<RunInnerOutcome>`. The
helpers cannot consume `self` (they're called inside a `loop` that
needs `self` for the next iteration); only `make_outcome` does.
So helpers borrow `&mut self` and return `Option<(finish, step)>` or
`Option<FinishReason>`; the call site then routes through
`make_outcome` for the actual outcome construction. `ControlFlow` would
have required either moving `self` into the helper (breaks the loop) or
introducing a `Self`-reconstruction path (worse than the current
shape). The `Option`-returns-plus-`make_outcome`-at-return-site
pattern keeps the borrow story simple.

### Why `dispatch_llm_step` does NOT push to `self.messages`

The two callers — `handle_no_tool_calls` and the tool-dispatch path
(`push_message(Message::assistant_with_tool_calls(...))`) — push
*different* shapes onto the transcript. Pushing inside the helper
would have required returning a "which-path" enum, which is just the
existing no-tools-vs-tools branch with worse names. The helper returns
the raw `Completion` and the loop body makes the transcript decision.

### Why `process_tool_results` takes `&mut` refs to loop-local accumulators

`recent_errors`, `tool_audits`, `skill_injector` are declared inside
`run_inner`'s body and persist across steps. Moving them onto `self`
would change `RunCore`'s field shape (and the `Kernel → RunCore`
handoff boundary), which is out of scope for P1-1. Passing `&mut` is
the surgical option. If a future refactor moves these onto `self`,
the helper signature shrinks by three parameters.

### Invariant #8 preservation in `process_tool_results`

The two known traps — sentinel duplicating results and stuck-detection
orphaning tool_use blocks — are documented in the helper's
doc-comment. The existing `stuck_detection_window_and_rate`,
`stuck_detection_partial_errors_below_threshold`,
`stuck_detection_reports_most_repeated_tool`, and
`tool_call_pairing::*` tests pin these invariants behaviourally.

### What was NOT done in this PR

- `execute_tool_calls` (the inner tool dispatch with parallel/serial
  batches and plan-mode gating) is still ~280 lines on its own. It was
  not part of `run_inner`'s body so it does not affect the invariant
  threshold, but it is the next candidate if a future round wants to
  slim `run_core.rs` further.
- The five `tracing::info!("agent.run.complete")` lines (one per
  termination path) could consolidate into a single helper, but each
  carries slightly different fields (`steps`, `tokens_in`,
  `tokens_out`, `finish`, `llm_latency_ms`) and consolidating them
  would not reduce `run_inner`'s body further.

## Next session pickup

P1-1 is done. The remaining architecture-review items are:

- **P1-2**: `AgentRuntime` field reorganization + lock-hierarchy doc
  (`docs/INTERNALS.md`). MEDIUM risk. Now that `run_inner` is small,
  this is the next natural target.
- **P1-3**: kernel-vs-platform crate split (open an issue first; this
  affects publish + downstream imports).

If picking up P1-2, start by reading `src/runtime.rs` fields
(11 fields, 7 sync primitives mixed) and the HTTP → session → runtime
→ kernel call chain for the lock hierarchy.
