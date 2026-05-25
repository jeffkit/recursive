# Goal: surface token usage end-to-end

## Motivation

Today the agent has no idea how many tokens it has spent. Users can't budget
cost, can't debug "why is this conversation so long", and can't compare
providers. Most OpenAI-compatible servers already return `usage` in every
chat completion response — we just throw it away. Read it, accumulate it
across a run, and let observers (CLI, library callers, custom UIs) see it.

## Requirements

### 1. New `TokenUsage` type

In `src/llm/mod.rs` add:

```rust
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

impl TokenUsage {
    /// Saturating element-wise add. Used to accumulate across LLM calls.
    pub fn add(self, other: TokenUsage) -> TokenUsage {
        TokenUsage {
            prompt_tokens: self.prompt_tokens.saturating_add(other.prompt_tokens),
            completion_tokens: self.completion_tokens.saturating_add(other.completion_tokens),
            total_tokens: self.total_tokens.saturating_add(other.total_tokens),
        }
    }
}
```

### 2. Extend `Completion`

Same file — add an `Option<TokenUsage>` field:

```rust
pub struct Completion {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    pub usage: Option<TokenUsage>,   // NEW — None when the provider didn't report
}
```

Update every construction site in the crate (mock, openai, all tests) so it
still compiles. For existing tests that didn't care, just put `usage: None`.

### 3. Parse `usage` in `OpenAiProvider`

In `src/llm/openai.rs`, the JSON response from OpenAI-compatible servers
includes an optional `usage` block, e.g.

```json
{
  "choices": [...],
  "usage": { "prompt_tokens": 41, "completion_tokens": 7, "total_tokens": 48 }
}
```

Some servers omit it, or omit individual sub-fields. Define a deserializer
that tolerates absence (use `#[serde(default)]` plus optional sub-fields, or
parse manually from `serde_json::Value`). When the block is absent, set
`Completion::usage = None`. When present but partial, fill missing fields
with `0`.

### 4. Mock provider stays scripted

`MockProvider` already drives every test deterministically. No change to its
behaviour required — just make sure existing call sites compile, and add a
**new** constructor or builder that lets a test specify `usage` per scripted
completion. Pick whichever is cleanest. A reasonable shape:

```rust
impl MockProvider {
    pub fn with_usage(scripts: Vec<Completion>) -> Self { /* identical to ::new */ }
}
```

(In other words: nothing forces a separate constructor — the existing one
already takes a full `Completion`, so callers can simply set `usage: Some(...)`
in their scripted completions. Choose the cleanest path; just ensure tests
can drive usage.)

### 5. Accumulate in `AgentOutcome`

In `src/agent.rs`, `AgentOutcome` gains:

```rust
pub struct AgentOutcome {
    pub final_message: Option<String>,
    pub transcript: Vec<Message>,
    pub steps: usize,
    pub finish: FinishReason,
    pub total_usage: TokenUsage,   // NEW — sum across all LLM calls in this run
}
```

Inside `Agent::run`, maintain a running `TokenUsage` initialised to default,
and after every `self.llm.complete(...)` returns, do:

```rust
if let Some(u) = completion.usage {
    total_usage = total_usage.add(u);
}
```

Set `total_usage` on the outcome at every return point (the normal exit, the
stuck exit, the budget-exceeded path).

For `BudgetExceeded`: the function currently returns `Err(...)`. Don't change
that — usage information for an exceeded run is fine to discard at this
stage. Only the two successful return paths (`NoMoreToolCalls`/`Stuck`) need
to populate `total_usage`.

### 6. New step event

In `src/agent.rs`, `StepEvent` gains:

```rust
StepEvent::Usage { usage: TokenUsage, step: usize },
```

Emitted **immediately after** an LLM call returns with `usage.is_some()`.
Place it after the `AssistantText` emission (if any) but before tool
processing. Updates to the existing event-ordering test (`emits_events_in_order`)
are expected — extend the script to include `usage` and the expected sequence.

### 7. CLI surface

In `src/main.rs`:

- For `run`: after the agent completes, print one line to stderr:
  `tokens: prompt=X completion=Y total=Z` (only if `total > 0`).
- For `repl`: print the same line after each turn.

Don't add new CLI flags — this is information that should just appear.

## Tests to add

In `src/llm/mod.rs` or a sibling test module:

1. `TokenUsage::add` is saturating and commutative.
2. `TokenUsage::default()` is all zeros.

In `src/llm/openai.rs` tests:

3. Parsing a real-shaped JSON response with `usage` populates `Completion::usage`.
4. Parsing a response that omits `usage` yields `Completion::usage = None`.
5. Parsing a partial `usage` (e.g. only `total_tokens`) fills the missing
   fields with `0` and still returns `Some`.

In `src/agent.rs` tests:

6. After a 2-LLM-call run where each scripted completion reports
   `{prompt:10, completion:5, total:15}`, the outcome's `total_usage` is
   `{prompt:20, completion:10, total:30}`.
7. A run where the provider never returns `usage` ends with `total_usage`
   at its `Default` (all zeros).
8. `StepEvent::Usage` is emitted once per LLM call that reported usage.

All existing tests must continue to pass.

## Out of scope

- Cost estimation (price tables per model). Just count tokens.
- Streaming. `usage` arrives in the final chunk of a stream, but we don't
  stream yet.
- Persisting usage across runs.
- Per-tool / per-step cost breakdown beyond what the events already imply.

## Done = all of:

- `cargo fmt --all -- --check` clean
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo test` green (existing 55 + the new tests listed above)
- A manual `recursive run "say hi and then stop"` prints a non-zero
  `tokens: ...` line at the end (when the configured provider returns usage)
