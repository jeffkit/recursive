# Manual edit: Goal 382 — persist partial content on stream interrupt

**Date**: 2026-08-03 (late) / 2026-08-04 (e2e round)
**Goal**: 382 — persist partial content on stream interrupt
**Branch/worktree**: `selfimprove-1785771776978` (goal run worktree)

## What changed

A network drop / user-initiated cancel mid-stream used to discard the whole
turn's assistant content: `parse_sse_stream` returned `Err(make_err(...))` on
a read error, `run_inner` propagated it as a non-`Cancelled` `Err`, and
`runtime.rs:283` short-circuited *before* `emit_turn_messages`, so neither the
in-memory transcript nor the on-disk JSONL ever saw the partial reply. The
next "继续" turn then fed the model `[..., user_N, user_继续]` with a gap.

Fix: the two SSE parsers now *return* what they accumulated (as a partial
`Completion` with `finish_reason: Some("interrupted")`) instead of dropping it,
and `run_inner` routes that sentinel through the existing
`FinishReason::Cancelled` → `make_cancelled_outcome` → `emit_turn_messages`
path so the partial content reaches disk. No new `Error` variant, no new
`FinishReason` (Invariant #7 preserved), no new agent-work branch in
`run_inner` (Invariant #1 preserved).

## Files touched

- `src/llm/openai.rs` — `parse_sse_stream`:
  - `Some(Err(e))` read-error arm: if `content`/`reasoning_content`/
    `tool_call_builders` accumulated anything, return
    `Ok(Self::build_completion(..., Some("interrupted"), usage.take()))`;
    otherwise keep the original `Err(self.make_err(...))` (drop before the
    first token = nothing to save).
  - `Error::Cancelled` arm (cancel token fired mid-stream): same partial
    return when content exists; else keep `Err(Error::Cancelled)`.
  - Extracted the end-of-stream `Completion` assembly into a shared
    `build_completion` static helper (used by both normal + partial paths).
- `src/llm/anthropic.rs` — identical symmetric change over `SseAccum`;
  extracted `build_completion(acc, finish_reason)` static helper.
- `src/run_core.rs`:
  - `run_inner`: after `dispatch_llm_step`, if
    `completion.finish_reason.as_deref() == Some("interrupted")`, delegate to
    new sibling helper `finish_interrupted`.
  - New sibling helper `finish_interrupted(mut self, ...)`: pushes the
    partial assistant message via `push_message` + `attach_reasoning_content`
    (same pattern as `handle_no_tool_calls`), then returns
    `make_cancelled_outcome(step, Some(content), ...)`. `make_outcome` drains
    `self.messages` into `new_messages` so `runtime.rs::emit_turn_messages`
    writes it to disk.
  - Extracted into a sibling helper (not inlined) to keep `run_inner`'s body
    ≤ 150 lines — Invariant #1 (`tests/invariants/loop_size_orthogonality.rs`).
  - Test-only fix (pre-existing race): serialised the three
    `effective_step_limit_*` tests behind a `static HARD_CAP_ENV_LOCK` mutex —
    they all mutate the process-global `RECURSIVE_HARD_STEP_CAP` and raced each
    other in parallel runs (reproduced on the clean tree before this goal's
    changes; see Notes).

## Tests added

- `src/llm/openai.rs` `parse_sse_stream_returns_partial_on_mid_stream_error` —
  mock TCP server sends one valid `data:` chunk, then closes with a
  `Content-Length` larger than the body so the client's next
  `bytes_stream().next()` returns a truncated-body read error; asserts
  `content == "partial"`, `finish_reason == Some("interrupted")`, and no tool
  calls.
- `src/llm/anthropic.rs` `parse_sse_stream_returns_partial_on_mid_stream_error`
  — symmetric: two text deltas, then truncated body; asserts
  `content == "Hello partial"`, `finish_reason == Some("interrupted")`.
- `src/run_core.rs` `interrupted_completion_persists_partial_content_as_cancelled`
  — `MockProvider` returns `Completion { finish_reason: Some("interrupted"),
  content: "partial reply…", reasoning_content: Some("half a thought") }`;
  asserts outcome `finish_reason == Cancelled`, `final_message ==
  Some("partial reply…")`, and `outcome.messages` contains exactly one
  assistant message with the partial content + reasoning (i.e. it rides the
  existing `new_messages` persistence path).

## Notes

- **Tool calls are dropped in the partial path** (both providers): a
  half-received `tool_call` (truncated JSON arguments) would create an orphan
  `tool_call` with no matching result and break Invariant #8 on the next turn.
  The partial `Completion` therefore always carries empty `tool_calls`; only
  text/reasoning content is persisted. `have_partial` still *counts*
  `tool_call_builders` / `acc.tool_calls` so a stream that was cut while
  emitting tool-call deltas still triggers the partial path (it just persists
  whatever text/reasoning arrived — possibly an empty assistant message in the
  extreme case, which is harmless and ends the turn Cancelled).
- **Provider parity**: the two parsers are symmetric by construction — same
  `have_partial` guard, same `"interrupted"` sentinel, same
  `tool_calls`-dropped decision, same shared builder helper shape.
- **`"interrupted"` is a provider-level sentinel only** — it lives inside
  `Completion::finish_reason`, is matched at the `run_core` boundary, and is
  converted to the *existing* `FinishReason::Cancelled`. It never becomes a
  top-level `FinishReason` or `Error` variant (Invariant #7).
- **Invariant #1 guard**: adding the routing inline pushed `run_inner` to 161
  lines (limit 150). Per the invariant's own guidance ("extract another phase
  into a sibling helper — NOT to bump the threshold"), the logic moved into
  `finish_interrupted`; body is back to 147 lines.
- **Pre-existing flaky test fixed en route**: the three
  `effective_step_limit_*` tests in `src/run_core.rs` mutate the same
  process-global env var and fail nondeterministically when run in parallel
  (reproduced on the clean tree: 2/3 failed). They now share a
  `HARD_CAP_ENV_LOCK` mutex. Unrelated to Goal 382 but needed to keep
  `cargo test --workspace` green.
- Verification: `cargo check -p recursive-agent`, `cargo test -p
  recursive-agent --lib` (2220 pass), `cargo test --test invariants` (39
  pass), headline tests (`parse_sse_stream_returns_partial`,
  `interrupted_completion_persists_partial_content_as_cancelled`) pass.
  Full fmt/clippy/workspace gates run separately before landing.

## E2E gate round (2026-08-04) — infra, not code

The flow's e2e gate initially reported FAILED with only a truncated
`.gate-e2e-output.log` (9 lines, ending at the tsc build). Diagnosis:

1. The first gate invocation was killed mid-flight (after `argus-init`,
   during the docker image build), stranding the known state: alive mcp2cli
   session daemon (`argusai-wt-79daad1`), `wt-79daad1-aimock` container, and
   `e2e/.argusai/history.db` marked initialized (AGENTS.md failure mode #4 +
   goal-364 update).
2. The killer: **the docker build of `recursive:e2e-wt-79daad1` takes ~12-14
   minutes** (release compile of the 3 workspace crates inside the colima VM;
   the builder stage is not cached for src/ changes). The agent Bash tool caps
   commands at 300s, so any attempt to run the gate synchronously got killed
   mid-build, re-stranding the session. The build client shows ~0.00 host CPU
   while the VM compiles, which looks like a hang but is not.
3. Remedy applied (per AGENTS.md): `rm -rf e2e/.argusai`,
   `mcp2cli --session-stop argusai-wt-79daad1`, `docker rm -f
   wt-79daad1-aimock`, kill any orphaned `docker build`/`mcp2cli argus-build`
   processes. Then re-ran the gate via a background job (3600s timeout):
   `sh .dev/scripts/e2e-gate.sh > .gate-e2e-output.log 2>&1`, waited ~15 min
   for the build, and got:
   `[e2e-gate] smoke PASSED ✓` / `GATE_EXIT=0` — smoke suite 3/3 passed
   (status=passed, totals {passed:3, failed:0, total:3}).
4. Post-run state verified clean: no e2e containers, no argusai session, no
   builds; removed the leftover `e2e/.argusai` so the flow's backstop gate
   re-run starts fresh (avoids SESSION_EXISTS).

**Lesson for future gates**: the recursive e2e gate is a 15-30 minute
operation (mostly the docker release build). Never run it through a tool with
a short timeout; use a background job and poll. A truncated gate log ending at
the plugin build is the signature of a killed run, not a code failure — check
for stranded sessions/containers first, clean per AGENTS.md failure modes #4/#5,
then re-run to completion.
