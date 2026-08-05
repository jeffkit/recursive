# Goal 385 — Invariant size headroom (kernel / runtime / run_inner / run_core)

**Roadmap**: Self-improve pipeline reliability. Without headroom, the next
feature goal that adds ~10 lines near the loop will fail
`tests/invariants/loop_size_orthogonality.rs` and roll back — a systemic
blocker for the Flowcast self-improve flow.

**Design principle check**:
- Implemented as: extract already-isolated helpers / test modules into
  sibling files under `src/runtime/` or `src/run_core/` (or thin `mod`
  splits), **without changing behaviour**. Pure mechanical move + update
  the invariant limits only if extraction alone is insufficient — prefer
  extraction over raising limits.
- ❌ Does NOT add features, change finish reasons, or touch tool semantics.
- ❌ Does NOT branch inside `run_inner`'s control flow — only relocate code
  that already lives outside the 150-line body, or shrink the body by
  extracting a named helper that already exists inline.

## Why (verified 2026-08-04)

Measured against `tests/invariants/loop_size_orthogonality.rs`:

| Guard | Actual | Limit | Headroom |
|-------|--------|-------|----------|
| `src/kernel.rs` total lines | 998 | 1000 | **2** |
| `src/runtime.rs` total lines | 3692 | 3700 | **8** |
| `RunCore::run_inner` body | 147 | 150 | **3** |
| `src/run_core.rs` production (pre-`mod tests`) | ~1467 | 1500 | **33** |

Goal 358 already added the production-size guard; it did **not** create
breathing room. `run_inner` looks small, but sibling helpers in the same
file absorb every new capability. The self-improve loop will soon collide
with these ceilings on unrelated goals.

## Scope (do exactly this, no more)

### 1. Create headroom — targets after this goal

`src/kernel/` and `src/run_core/` do not exist at the baseline. Creating these
module directories is explicitly in scope; `src/runtime/` is the existing
split-module precedent. Work in phases (kernel, runtime, then run-core), and
run the relevant tests after each phase rather than moving all three large
files in one rewrite.

| Guard | Target headroom | How |
|-------|-----------------|-----|
| `kernel.rs` | ≥ 80 lines free (≤ 920) | Move a coherent non-loop chunk (e.g. builder helpers, pure utilities already at the bottom) into `src/kernel/` submodule(s). Keep `AgentKernel` / `TurnContext` / `TurnOutcome` public API stable via `pub use`. |
| `runtime.rs` | ≥ 150 lines free (≤ 3550) | Continue the existing pattern (`src/runtime/builder.rs`, `checkpoint.rs`): extract another cohesive block (e.g. goal-wiring, event-sink wiring, or a cluster of `&mut self` helpers that are already called from few sites). |
| `run_inner` body | ≥ 30 lines free (≤ 120) | Extract 1–2 already-sequential blocks into `RunCore` methods (e.g. the shutdown/wall/mailbox/transcript preamble, or the post-LLM "no tools → finish / push assistant / emit / execute" epilogue) **without** changing order or semantics. |
| `run_core.rs` production | ≥ 100 lines free (≤ 1400) | Prefer moving large helpers (`execute_tool_calls`, `process_tool_results`, `dispatch_llm_step`, …) into `src/run_core/` modules (`tools.rs`, `llm_step.rs`, …) re-exported/used by `RunCore`. Do **not** raise the 1500 limit unless extraction is exhausted and you document why in the journal. |

### 2. Keep public API stable

- `use recursive::{AgentKernel, AgentRuntime, …}` must still compile.
- No new public types unless required for the split; prefer `pub(crate)`.
- Update `docs/architecture/agent-loop.md` **only** if file paths it cites move (one sentence / path fix — no rewrite).

### 3. Tests

- All existing invariant tests must stay green.
- If you extract a pure helper from `run_inner`, add a unit test that pins
  its behaviour (or move an existing test next to it).
- Explicitly run the invariant suites for loop size, finish-reason-as-data,
  and tool-call/tool-result pairing. Also run the existing named tests for
  cancellation / interruption paths touched by Goals 353/382/383; extraction
  must not turn a finish reason into an error or reorder emitted events.
- Do **not** weaken the numeric limits in
  `tests/invariants/loop_size_orthogonality.rs` unless extraction is genuinely
  exhausted, the journal explains why, and the resulting limit still leaves
  at least 50 lines free. Raising a limit is an exception, never the default
  implementation.

## Files NOT to touch

- Tool implementations under `src/tools/` (except imports if a type moved).
- `.dev/flows/`, `.flowcast/`.
- TUI / HTTP / MCP surfaces.
- Goal 386+ behaviour (default TUI launch, etc.).

## Acceptance

- `cargo test --workspace` green.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` clean.
- `cargo test --test loop_size_orthogonality` green.
- `cargo test --test finish_reason_data` and
  `cargo test --test tool_call_pairing` green (use the actual invariant
  binary names if Cargo reports a different target name).
- The existing cancel / interrupt regression tests referenced by Goals
  353/382/383 exist and pass by name; record the exact filters in the journal.
- Grep / measure after the change:
  - `kernel.rs` lines ≤ 920 **or** documented exception with ≥ 50 free vs limit.
  - `runtime.rs` lines ≤ 3550 **or** same rule.
  - `run_inner` body ≤ 120.
  - `run_core.rs` production ≤ 1400.
- Journal: `.dev/journal/manual-20260804-goal385-size-headroom.md` with
  before/after line counts for all four guards.

## Notes for the agent (traps)

- **Do not** "fix" headroom by deleting comments, crushing formatting,
  moving the test boundary, or raising the limits as the first choice.
  `cargo fmt` will undo formatting tricks and the invariants count physical
  lines.
- Production paths created by the extraction must not introduce
  `unwrap()` / `expect()`; preserve invariant #5 and existing error flow.
- **Do not** move the `#[cfg(test)] mod tests` block into a smaller
  production count by cheating — the production guard uses the line of
  `mod tests {` under `#[cfg(test)]`.
- When extracting from `run_inner`, keep the **exact** early-return
  order (shutdown → wall → mailbox → transcript → compact → LLM → …).
  Invariant #7 (finish reasons are data) and cancel/interrupt paths
  (Goals 353/382/383) are easy to regress.
- `kernel.rs` is 2 lines under the limit — even adding a blank line can
  fail CI. Extract first, then format.
- Prefer `apply_patch` / surgical moves; do not rewrite entire 3k-line
  files in one Write.
