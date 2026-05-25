# Goal 07 — Per-run transcript size limit

## What

Give `Agent` an optional ceiling on **total transcript size in
characters**, and a new `FinishReason::TranscriptLimit` for when it's
reached. Plumb a `--max-transcript-chars` CLI flag through so users
can opt in.

## Why

Goal 06 surfaced the real cost driver: prompt tokens accumulate
because each LLM call re-sends the full transcript. A long-running
agent loop will eventually charge you in O(steps × transcript_size)
tokens. We don't have a way to cap that today — `--max-steps` only
caps *count* of steps, not *size* of the conversation.

Adding a hard size ceiling lets a user say "I don't care if you're
still working, stop when the transcript hits 100 KB." That's a
predictable cost ceiling, expressed in plain characters (no
tokenizer dependency).

## Scope (do exactly this, no more)

### 1. `src/agent.rs`

Add a new variant to `FinishReason`:

```rust
TranscriptLimit { chars: usize, limit: usize },
```

Add a field to `Agent`:

```rust
max_transcript_chars: Option<usize>,
```

Add a builder setter on `AgentBuilder`:

```rust
pub fn max_transcript_chars(mut self, n: usize) -> Self {
    self.max_transcript_chars = Some(n);
    self
}
```

In `Agent::run`, **at the top of every iteration of the main loop**
(before the LLM call, so the next call wouldn't blow past the
ceiling), compute the running size:

```rust
let chars: usize = self.transcript.iter().map(|m| m.content.len()).sum();
if let Some(limit) = self.max_transcript_chars {
    if chars >= limit {
        let finish = FinishReason::TranscriptLimit { chars, limit };
        self.emit(StepEvent::Finished { reason: finish.clone(), steps: step });
        return Ok(AgentOutcome {
            final_message,
            transcript: std::mem::take(&mut self.transcript),
            steps: step,
            finish,
            total_usage,
        });
    }
}
```

Use `m.content.len()` only (byte length of the string field). Don't
try to also measure tool_calls / tool result JSON encoding — that's a
follow-up if we ever want it. Chars-of-content is a deliberately
simple proxy; the goal is a *predictable* ceiling, not a true token
count.

Default is `None` (unbounded), preserving existing behaviour.

### 2. `src/main.rs`

Add CLI flag:

```rust
/// Stop when total transcript content reaches this many characters.
#[arg(long, env = "RECURSIVE_MAX_TRANSCRIPT_CHARS")]
max_transcript_chars: Option<usize>,
```

If set, call `.max_transcript_chars(n)` on the builder before
`.build()`.

After `agent.run(...)` finishes, in the existing match on `outcome.finish`
(if any — otherwise in the post-run summary block), add a stderr line
for the new variant:

```
note: stopped because transcript reached <chars> chars (limit <limit>)
```

If there's no existing match, just add a single `if let` after the
usage print. Keep it simple.

### 3. Tests

Add unit tests in `src/agent.rs`:

1. `transcript_limit_stops_loop` — script the MockProvider to make
   many small tool calls; set `max_transcript_chars(50)`; assert
   the outcome's `finish` is `TranscriptLimit` and `chars >= 50`.
2. `transcript_limit_unset_runs_to_completion` — same script as a
   prior test, but no limit; assert it ends with `NoMoreToolCalls`.
3. `transcript_limit_is_checked_before_llm_call` — script a single
   massive user goal that already exceeds the limit; assert the
   agent stops at step 1 without making an LLM call. (Use a
   `MockProvider::new(vec![])` so any actual call would panic.)

## Out of scope

- Tokenizer-accurate counting. Bytes-of-content is good enough.
- Counting tool-call JSON or system prompt. The user knows their own
  system prompt size; the limit applies to the *conversation*, which
  is what grows step-by-step.
- Summarisation / compaction of old messages. That's a much bigger
  follow-up that needs design.
- Persisting the ceiling per-run anywhere.
- Touching `OpenAiProvider` or `MockProvider`.

## Definition of done

- `cargo fmt`, `cargo clippy -- -D warnings`, `cargo test` all green.
- New tests pass.
- `recursive run --max-transcript-chars 1000 "hi"` is a valid invocation
  (no parser errors).
- No new dependencies.

## Notes for the agent

- This is a self-contained change in `src/agent.rs` + a small CLI
  surface in `src/main.rs`. Use `apply_patch` for both files.
- Don't restructure existing tests. Don't rename existing variants.
- `FinishReason` is a public enum; adding a new variant is a minor
  breaking change to anyone matching exhaustively. That's acceptable
  here — the type isn't widely consumed yet.
- Keep the `chars` calculation in one place. If you ever want to
  reuse it, extract a helper, but only after the test for case 1
  passes.
