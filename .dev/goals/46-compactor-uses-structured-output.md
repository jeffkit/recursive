# Goal 46 — Compactor uses Structured Output (consumer wiring)

> **Roadmap**: NOT a new feature — this is the **first real consumer**
> of g41 (structured-output). Demonstrates the plumbing actually
> works end-to-end, surfaces any bugs in the JSON-schema response
> path.
> **Design principle check**: orthogonal — only touches
> `src/compact.rs`. No new public API. Pluggable — fall back to
> existing free-text path if provider returns an error from
> `complete_structured`.

## What

Currently `Compactor::compact()` asks the model for a free-text
summary of older transcript messages. The output is then wrapped in
a synthetic system message. The problem: free text gives no
guarantees about structure, so it's hard to use programmatically
(e.g. to highlight key facts in a UI).

Migrate `Compactor::compact()` to use
`LlmProvider::complete_structured()` with the following schema:

```json
{
  "type": "object",
  "properties": {
    "summary": {
      "type": "string",
      "description": "1-3 paragraph summary of the conversation so far, preserving key decisions, file paths touched, and outcomes."
    },
    "kept_facts": {
      "type": "array",
      "items": { "type": "string" },
      "description": "Discrete facts worth remembering across compaction (e.g. 'goal=add_X_to_Y', 'compaction happened at step N', 'tool X failed 3 times')."
    },
    "next_steps": {
      "type": "array",
      "items": { "type": "string" },
      "description": "Outstanding TODOs the agent identified before compaction (each one a single-sentence imperative)."
    }
  },
  "required": ["summary", "kept_facts"]
}
```

The synthetic system message inserted after compaction should be
rendered from this object as:

```
[Context compacted at step N]

Summary: {summary}

Key facts to remember:
- {kept_facts[0]}
- {kept_facts[1]}
- ...

Outstanding TODOs:
- {next_steps[0]}
- ...
```

## Why

- **End-to-end validation of g41.** This is the first caller; if
  the structured API has any bug (request shape, response parsing,
  schema validation), it'll surface here.
- **Better compaction quality.** Structured output forces the
  model to separately think about "what's the summary" vs. "what
  must I remember verbatim" vs. "what's still pending." Free-text
  summaries collapse all three into one paragraph.
- **Downstream value.** The structured output can be serialized
  into the transcript with `kept_facts` as a first-class array,
  enabling future tools to introspect "what does the agent know."

## Fallback path

If `complete_structured()` returns an error (e.g. provider doesn't
support it), `Compactor::compact()` MUST fall back to the existing
free-text path. The fallback test should pass.

## Tests

- `compactor_structured_happy_path` — MockProvider with
  `complete_structured` returning the schema-conformant JSON.
  Assert the synthetic message contains the expected sections.
- `compactor_falls_back_on_structured_error` — MockProvider
  returns `Err` from `complete_structured`. Assert Compactor
  retries with free-text and proceeds (existing test still
  passes).
- `compactor_structured_invalid_response_falls_back` — MockProvider
  returns Ok but with JSON that doesn't match the schema.
  Compactor must NOT crash. Either fall back or recover gracefully.

## Wiring

- `src/compact.rs`: extend `compact()` to call
  `complete_structured` first, fall back if error. Approx 80 LOC
  change.
- `src/llm/mock.rs`: extend MockProvider to support
  per-method response queues (probably already supports this for
  `complete`; mirror for `complete_structured`).

## Acceptance

- `cargo build` green.
- `cargo test` green; +3 new tests minimum. Existing
  Compactor tests still pass (because of the fallback path).
- `cargo clippy --all-targets -- -D warnings` clean.
- `cargo fmt --all -- --check` clean.

## Out of scope (defer)

- Migrating other internal callers to structured output (none
  exist yet; this IS the first one).
- Exposing the structured output to consumers (the compacted
  message stays a flat string for now).
