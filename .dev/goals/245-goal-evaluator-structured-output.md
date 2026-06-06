# Goal 245 — GoalEvaluator: use structured output instead of text prefix

**Roadmap**: Arch-review bugfixes (high severity)

**Design principle check**:
- Implemented as: change GoalEvaluator to use `complete_structured()` with JSON schema
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

The GoalEvaluator checks whether the agent has completed its goal by calling
the LLM and checking if the first line starts with "YES" or "NO". Any
preamble, formatting variation, or model localisation will silently
misclassify — false NO keeps the agent looping, false YES terminates early.
`Compactor` already uses `complete_structured()` for typed responses; the
same pattern should be applied here.

## Scope (do exactly this, no more)

### 1. `src/run_core.rs` — change GoalEvaluator to use structured output

Read the `GoalEvaluator` struct and its `evaluate()` / `check()` method in
`src/run_core.rs`. It currently calls the LLM and parses the text response
for "YES"/"NO".

Replace the plain text call with a structured call. The schema should be:

```json
{
  "type": "object",
  "properties": {
    "completed": { "type": "boolean" },
    "reason": { "type": "string" }
  },
  "required": ["completed", "reason"]
}
```

Use whichever method the LLM provider exposes for structured/JSON output
(look at how `Compactor` does it in `src/compact.rs` — it uses
`provider.complete_structured()` or similar). Mirror that approach exactly.

The return value of `evaluate()` should still be `bool` (just read
`response.completed`).

If `complete_structured()` is not available or returns an error, fall back
to the existing text-prefix parsing as a last resort (don't break the
evaluator if the provider doesn't support structured output).

### 2. Tests

Update or add a test in `src/run_core.rs` `#[cfg(test)]` that verifies
the structured response `{ "completed": true, "reason": "..." }` is parsed
correctly to `true`, and `{ "completed": false, "reason": "..." }` to
`false`.

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- GoalEvaluator uses structured output when available
- Existing GoalEvaluator tests still pass

## Notes for the agent

- Read `src/run_core.rs` GoalEvaluator in full before editing.
- Read `src/compact.rs` to understand how structured output is used there.
- Read `src/llm/mod.rs` `LlmProvider` trait to find the structured output method.
- **DO NOT modify** `src/agent.rs`, `src/runtime.rs`, `src/llm/`, `src/config.rs`.
- **DO NOT call `exit_plan_mode` or `request_plan_mode`.** You are running
  headless; the plan gate has no reviewer. Just read and edit directly.
