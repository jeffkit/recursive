# Goal 382 — Persist partial content on stream interrupt

**Roadmap**: Kernel / runtime — durability. A network drop (Wi-Fi flapping,
laptop lid closing mid-reply) currently loses the entire turn's assistant
content: it never reaches disk, so the next "继续" turn feeds the model a
transcript with a gaping hole where the reply should be.

**Design principle check**:
- Implemented as: extend the two SSE parsers to *return* what they already
  accumulated instead of dropping it on a read error, and widen the existing
  `Error::Cancelled` branch in `run_inner` to also cover stream-interrupted
  partial results. No new capability branches, no new tool, no new finish
  reason — the partial content rides the existing `make_cancelled_outcome`
  → `emit_turn_messages` → `MessageAppended` path.
- ❌ Does NOT add a new branch that does agent work inside
  `run_core.rs::run_inner`. The `Error::Cancelled` arm at `run_core.rs:1320`
  already exists (Invariant #7 wiring); this goal only changes *which* errors
  reach it, not the loop's structure. `tests/invariants/loop_size_orthogonality.rs`
  stays green.
- ❌ Does NOT introduce a new `FinishReason` variant. The partial turn
  finishes as `FinishReason::Cancelled` (Invariant #7: finish reasons are
  data, not errors; the transcript is persisted before the CLI maps the
  reason to an exit code).

## Why (verified 2026-08-03 by reading the code)

1. **`src/llm/openai.rs:668` — a stream read error discards all accumulated
   content.** `parse_sse_stream` builds up `content`, `reasoning_content`,
   and `tool_call_builders` in local variables (lines 631-636) as deltas
   arrive. On `Some(Err(e))` from `bytes_stream.next()` it does
   `return Err(self.make_err(...))` — the locals are dropped, the partial
   reply is gone. The error is a *plain* `Error::Msg`, **not** `Error::Cancelled`.

2. **`src/llm/anthropic.rs:356` — symmetric behaviour.** The Anthropic
   parser accumulates into `SseAccum` (line 333) and hits the identical
   `Some(Err(e)) => return Err(self.make_err(...))` on a read error, dropping
   `acc`.

3. **`src/run_core.rs:1331` — a non-`Cancelled` error short-circuits the
   transcript save.** `dispatch_llm_step` returns the `make_err` from (1)/(2);
   the match arm `Err(e) => return Err(e)` propagates it straight up. Only
   `Err(Error::Cancelled)` (line 1320) is routed to `make_cancelled_outcome`,
   which packages the turn into a `RunInnerOutcome` that the caller persists.

4. **`src/runtime.rs:283` — the propagation skips `emit_turn_messages`.**
   `Err(e) => return Err(e)` happens *before* line 285's
   `self.emit_turn_messages(&turn_outcome).await`. So neither the memory
   transcript nor the on-disk JSONL ever sees the assistant message for the
   interrupted turn. The user message *was* persisted (line 262,
   `append_user_message`, runs before the LLM call), producing a transcript
   on disk that ends with an unanswered user turn.

5. **`src/runtime.rs:651` — the next turn's context is built from the
   in-memory transcript.** When the user types "继续", `run()` calls
   `execute_kernel_turn`, which builds `TurnContext { messages:
   Arc::clone(&self.transcript), .. }`. Because the interrupted assistant
   message was never added to `self.transcript`, the model sees
   `[..., user_N, user_继续]` with no assistant reply between them — the
   "forgot the previous turn" symptom.

## Scope (do exactly this, no more)

### 1. `src/llm/openai.rs` — return partial content on stream read error

In `parse_sse_stream`, change the `Some(Err(e))` arm (line 668). Instead of
`return Err(self.make_err(...))`, if the accumulated `content` is non-empty
OR `tool_call_builders` has any entry, synthesise a partial `Completion`:

```rust
Some(Err(e)) => {
    let have_partial = !content.is_empty()
        || !reasoning_content.is_empty()
        || !tool_call_builders.is_empty();
    if have_partial {
        tracing::warn!(
            target: "recursive::llm",
            error = %e,
            content_len = content.len(),
            "SSE stream interrupted; returning partial completion"
        );
        return Ok(self.build_completion(
            content,
            reasoning_content,
            tool_call_builders,
            Some("interrupted".to_string()), // finish_reason
            usage.take(),
        ));
    }
    return Err(self.make_err(format!("SSE stream read error: {e}")));
}
```

If there is nothing accumulated yet, keep the current `Err` (a connection
drop before the first token is just a transport error — nothing to save).
Extract the existing end-of-stream `Completion` assembly (the code after the
loop that builds the return value from the same locals) into a small
`build_completion` helper so the partial path and the normal path share it.
Do the same for the `Error::Cancelled` arm (line 659): a user-initiated
cancel mid-stream should *also* return the partial `Ok(Completion)` when
content exists, so cancellation benefits from the same persistence.

### 2. `src/llm/anthropic.rs` — symmetric change

Apply the identical change to the `Some(Err(e))` arm at line 356 and the
`Error::Cancelled` arm at line 347, using `SseAccum`'s accumulated state.
Extract a shared builder helper mirroring the openai.rs one. The
`finish_reason` for a partial Anthropic completion should be
`Some("interrupted")` (the runtime only inspects finish_reason to pick
between `NoMoreToolCalls` and `ProviderStop`; an unknown value would map to
`ProviderStop` — see step 3).

### 3. `src/run_core.rs` — treat a partial/interrupted completion as a cancellable turn

After `dispatch_llm_step` returns `Ok((completion, new_final_message))`
(line 1317), add a check: if `completion.finish_reason == Some("interrupted")`,
the stream was cut partway. Push the partial assistant message onto the
transcript and finish the turn as `Cancelled` (so it persists via the
existing `make_cancelled_outcome` path):

```rust
let (completion, new_final_message) =
    match self.dispatch_llm_step(&specs, step, &mut total_usage).await {
        Ok(v) => v,
        Err(crate::error::Error::Cancelled) => {
            return Ok(self.make_cancelled_outcome(
                step, final_message, total_usage, tool_audits,
            ));
        }
        Err(e) => return Err(e),
    };
// NEW: a stream-interrupted completion arrives as Ok with finish_reason
// "interrupted" and partial content. Persist it as a partial assistant
// message and end the turn as Cancelled (Invariant #7: transcript saved).
if completion.finish_reason.as_deref() == Some("interrupted") {
    self.push_message(Message::assistant(completion.content.clone()));
    if let Some(rc) = completion.reasoning_content.clone() {
        self.attach_reasoning_content(rc);
    }
    return Ok(self.make_cancelled_outcome(
        step,
        Some(completion.content), // partial reply becomes final_message
        total_usage,
        tool_audits,
    ));
}
```

`push_message` / `attach_reasoning_content` are already used by
`handle_no_tool_calls` (lines 626-627) — mirror that exact pattern. The
partial message is now in `self.messages`, which `make_cancelled_outcome` →
`make_outcome` packages into `new_messages`, which `runtime.rs:emit_turn_messages`
writes to disk. No new branch does agent work — this is error routing for an
existing branch outcome.

### 4. Tests

- `src/llm/openai.rs` `#[cfg(test)] mod tests`: add
  `parse_sse_stream_returns_partial_on_mid_stream_error`. Build a fake
  `reqwest::Response` (use the existing test helpers in this file — search
  for how other parse_sse_stream tests construct responses) that yields one
  good `data:` chunk with content, then an `Err` on the next `next()`.
  Assert the returned `Completion` has `content == "<the delta>"` and
  `finish_reason == Some("interrupted")`.
- `src/llm/anthropic.rs`: mirror the test for the Anthropic parser using
  its existing SSE test fixtures.
- `src/run_core.rs` or `src/runtime.rs` (wherever interrupted-turn
  integration is easiest to drive): a test that feeds a provider stub
  returning `Completion { finish_reason: Some("interrupted"), content: "partial…", .. }`
  and asserts (a) the outcome's `finish_reason == Cancelled`, and (b)
  `outcome.new_messages` contains an assistant message with the partial
  content. If a full runtime test is too heavy, a `RunCore`-level unit test
  with a stub provider (pattern: `tests/` or existing `RunCore` tests) is
  acceptable — search for `RunCore` test helpers first.

## Files NOT to touch

- `src/kernel.rs` — stateless, nothing to change here.
- `crates/recursive-tui/**` — TUI interrupt handling is Goal 383.
- `src/http/**` — HTTP resume already uses `CancellationToken`; out of scope.
- `src/session/**` — the persistence sink already handles
  `MessageAppended`; no change needed there.
- `.dev/flows/`, `.dev/scripts/`, `.flowcast/` — supervisor infrastructure.
- `tests/invariants/**` — the invariants must stay green; do not weaken them.

## Acceptance

- `cargo build --workspace` green.
- `cargo test --workspace` green.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` clean.
- `cargo test --test loop_size_orthogonality` green (Invariant #1 not broken).
- `cargo test --test finish_reason_data` green (Invariant #7 not broken).
- Grep: `rg '"interrupted"' src/llm/openai.rs src/llm/anthropic.rs` — both
  files use the `"interrupted"` finish_reason sentinel for partial returns.
- Grep: `rg 'finish_reason.as_deref\(\) == Some\("interrupted"\)' src/run_core.rs`
  — the run_inner routing of partial completions to Cancelled is present.
- Grep: `rg 'return Err\(self.make_err' src/llm/openai.rs src/llm/anthropic.rs`
  — the stream-read-error arms now guard on `have_partial` before returning
  Err (the grep should show the partial-check wrapping the Err, not a bare
  return).
- Headline tests by name:
  `cargo test parse_sse_stream_returns_partial` — new parser tests green.
  `cargo test interrupted` — the run_core/runtime integration test green.

## Notes for the agent (traps)

- **Do not add `Error::Interrupted` or a new `FinishReason`.** Invariant #7
  is explicit: finish reasons are data, the transcript must be saved before
  the CLI maps reason → exit code. Route through the *existing*
  `FinishReason::Cancelled`. The `"interrupted"` string is a provider-level
  finish_reason sentinel carried inside `Completion`, converted to
  `Cancelled` at the run_core boundary — it never becomes a top-level
  `FinishReason` or `Error` variant.
- **Keep the two parsers symmetric.** Whatever you do in `openai.rs::parse_sse_stream`,
  mirror in `anthropic.rs::parse_sse_stream`. Reviewers check for provider
  parity.
- **Empty-accumulation case must still error.** A connection drop before the
  first token has nothing to save — keep returning `Err` there. Only
  synthesise a partial `Completion` when content/tool_calls/reasoning exist.
- **The partial assistant message must go through `push_message`.** That's
  how it enters `self.messages`, which is what `make_outcome` drains into
  `new_messages` for `emit_turn_messages` to persist. If you bypass
  `push_message`, the message won't reach disk.
- **Tool calls in a partial completion are dangerous.** A half-received
  `tool_call` (truncated JSON arguments) would create an orphan call with
  no result. For the partial path, it's safest to *drop* tool_call_builders
  and only persist the text/reasoning content. Note this in the journal.
- **cargo-fmt + clippy are enforced gates** — run both before finishing.
- **Journal**: write `.dev/journal/manual-20260803-goal382-stream-interrupt-persist.md`
  with Date / Goal / Files touched / Tests added / Notes (especially the
  tool-call-drop decision and the provider-parity note).
