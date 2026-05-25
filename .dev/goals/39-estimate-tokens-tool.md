# Goal 39 — `estimate_tokens` Tool (Phase 1.4)

> **Roadmap**: feature 1.4, S size, Medium impact. Closes Phase 1.
> **Design principle check**: orthogonal — adds a new `Tool`
> implementation; no existing file's behavior changes. Pluggable —
> registered alongside other tools in `main::build_tools`. Testable —
> pure function, no IO, no LLM calls.

## What

Add a `Tool` named `estimate_tokens` that takes either:

- `text: string` — estimate tokens for a literal string, OR
- `path: string` — estimate tokens for the contents of a file
  (resolved via `resolve_within`, sandboxed).

Returns a JSON-ish string like:
`tokens≈1234 (chars=4936, method=chars-over-4)`.

## Why

The agent currently has no way to gauge how much budget a file or
piece of text will consume in its transcript. This led to repeated
truncation-driven failures in earlier batches (g07, g19). With this
tool the agent can plan ahead — read 200 lines at a time vs. the
whole file — and avoid hitting `max_transcript_chars`.

A model-agnostic char/4 heuristic is good enough for budget planning.
(GPT/Claude/DeepSeek all converge to ~3.5-4.5 chars/token in English;
code is closer to 3.) We deliberately do NOT pull in a real tokenizer
crate — that would add a heavy native dependency for marginal gain.

## API

```rust
// src/tools/estimate_tokens.rs
pub struct EstimateTokens {
    workspace: PathBuf,
}

impl EstimateTokens {
    pub fn new(workspace: impl Into<PathBuf>) -> Self;
}
```

JSON schema:
```json
{
  "type": "object",
  "properties": {
    "text": { "type": "string" },
    "path": { "type": "string" }
  },
  "anyOf": [{"required": ["text"]}, {"required": ["path"]}]
}
```

Exactly one of `text`/`path` must be set. If both, prefer `text` (or
error — pick one and document).

## Tests

- `estimate_text_basic` — known string, asserts token count ≈ len/4.
- `estimate_path_reads_file` — write a file in tmpdir, read it.
- `estimate_path_outside_workspace` — must error (sandboxing).
- `estimate_neither_arg` — must error.

## Wiring

- `src/tools/mod.rs`: add `pub mod estimate_tokens;` +
  `pub use estimate_tokens::EstimateTokens;`.
- `src/main.rs::build_tools`: register it.
- `src/config.rs::default_system_prompt`: one line mentioning the
  tool name so the agent knows it exists.

## Acceptance

- `cargo build` green.
- `cargo test` green; +4 new tests.
- `cargo clippy --all-targets -- -D warnings` clean.
- `cargo fmt --all -- --check` clean.
