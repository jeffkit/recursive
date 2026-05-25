# Goal 41 — Structured Output (Phase 4.3)

> **Roadmap**: feature 4.3, S size, Medium impact.
> **Design principle check**: orthogonal — extends `LlmProvider` with
> an optional method that defaults to falling through to existing
> `complete`. Pluggable — opt-in per call site, no behavior change
> for callers who don't ask for it. Testable — Mock + OpenAI both
> get unit tests.

## What

Add a way for the agent to request a structured JSON response from
the LLM, conforming to a caller-supplied JSON schema. This uses
OpenAI's `response_format: { type: "json_schema", json_schema: {...} }`
API which MiniMax and DeepSeek both honor.

API sketch:
```rust
// src/llm/mod.rs (added to LlmProvider trait)
pub struct StructuredRequest {
    pub messages: Vec<Message>,
    pub schema: serde_json::Value,  // JSON schema for the response
    pub schema_name: String,
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    // existing methods...

    /// Request a JSON response conforming to a caller-supplied schema.
    /// Default impl: error. Providers that support structured output
    /// override this.
    async fn complete_structured(
        &self,
        _req: StructuredRequest,
    ) -> Result<serde_json::Value> {
        Err(Error::Config("provider does not support structured output".into()))
    }
}
```

Then `OpenAiProvider` overrides this to send the
`response_format.json_schema` block and parse the assistant response
as JSON.

## Why

Many internal agent operations would benefit from being JSON-typed:

- `Compactor::compact()` currently asks for free-text summary;
  structured output could enforce `{summary: string, kept_facts:
  string[]}`.
- Future "plan-then-act" prompts (probably batch-15) need a
  structured plan object.
- Permission hooks (3.4) could ask the LLM "is this tool call safe?
  return {allow: bool, reason: string}".

We're not wiring any callers yet — that's deliberate. This goal is
the **plumbing only**, just like Anthropic Provider (g34) was the
plumbing for that backend. Once it exists, future goals can use it.

## Tests

- `mock_structured_returns_default_error` — default impl returns an
  error, not a panic.
- `openai_structured_includes_schema_in_request_body` — wire up a
  mock HTTP server and assert the request body has the right shape.
- `openai_structured_parses_response_json` — same mock server returns
  a known JSON; assert the parsed value matches.

## Wiring

- `src/llm/mod.rs`: add `StructuredRequest`, extend trait.
- `src/llm/openai.rs`: implement the new method.
- `src/llm/mock.rs`: keep default-error behavior (no override needed).
- No `main.rs` changes — this goal does not add a CLI surface.

## Acceptance

- `cargo build` green.
- `cargo test` green; +3 new tests.
- `cargo clippy --all-targets -- -D warnings` clean.
- `cargo fmt --all -- --check` clean.
- Existing tests untouched.

## Out of scope

- Migrating `Compactor` to use this — separate goal.
- Anthropic provider's structured-output equivalent — separate goal.
