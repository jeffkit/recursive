# Goal: OpenAI provider software-layer ToolSearch fallback

## Motivation

The deferred tool loading mechanism (where low-frequency tools are withheld
from the initial LLM call and only loaded on demand via `ToolSearchTool`) is
currently only implemented in `AnthropicProvider`. Providers that use
`OpenAiProvider` (DeepSeek, MiniMax, Moonshot, etc.) always receive the full
tool list on every call, which wastes tokens and inflates the context window.

OpenAI-compatible APIs support standard function calling, which is sufficient
to implement the same ToolSearch pattern in software — no special API
extensions needed (no `defer_loading`, no `tool_reference`). The tradeoff is
slightly lower prompt cache hit rate (the tool schema content changes between
rounds), but the token savings on the initial call outweigh this.

## Background: how AnthropicProvider does it

`AnthropicProvider::complete_with_search` (in `src/llm/anthropic.rs`) runs a
loop:
1. Send only eager tools + `ToolSearchTool` to the API (`defer_loading: true`
   marks deferred tools so the model knows they exist but can't call them yet).
2. If the model calls `ToolSearchTool`, resolve the query against deferred
   tools, inject the matched schemas as `tool_reference` content blocks, and
   recurse.
3. Otherwise return the completion.

For OpenAI providers, `defer_loading` and `tool_reference` are not available.
The equivalent is:
1. Send only eager tools + `ToolSearchTool` (as a normal function).
2. If the model calls `ToolSearchTool`, execute the search, return the matched
   tool schemas as a plain JSON `tool_result`, then send a new request with
   the full schemas of the matched tools appended to the eager list.
3. Otherwise return the completion.

## Reference code

- `src/llm/anthropic.rs`: `run_search_aware_loop`, `tool_search_spec`,
  `TOOL_SEARCH_TOOL_NAME`, `MAX_SEARCH_ROUNDS`
- `src/llm/search.rs`: `KeywordSearchEngine`, `ToolSearchEngine`, `SpecWithHint`
- `src/llm/mod.rs`: `LlmProvider::complete_with_search`,
  `LlmProvider::stream_with_search` trait methods (and their default impls)

## Requirements

### 1. Override `complete_with_search` in `OpenAiProvider`

In `src/llm/openai.rs`, add a `search_engine` field to `OpenAiProvider`
(same pattern as `AnthropicProvider`) and override `complete_with_search`:

```rust
async fn complete_with_search(
    &self,
    messages: &[Message],
    eager_tools: &[(ToolSpec, Option<String>)],
    deferred_tools: &[(ToolSpec, Option<String>)],
) -> Result<Completion> {
    self.run_search_loop(messages, eager_tools, deferred_tools, vec![], 0).await
}
```

Implement `run_search_loop`:
- Build the request with: eager tools + `ToolSearchTool` (name =
  `"ToolSearchTool"`, same spec as in `AnthropicProvider::tool_search_spec()`).
  Do NOT include deferred tools in the initial request.
- If the response contains a `ToolSearchTool` call:
  - Resolve the query via `self.search_engine.resolve(query, deferred_tools)`.
  - Build a `tool_result` message with the matched tools' full JSON schemas as
    the content (plain text JSON, not `tool_reference` blocks).
  - Append the matched `ToolSpec`s to the running `loaded_tools` list.
  - Recurse with the updated messages and `loaded_tools` appended to
    `eager_tools` for the next round.
- Cap at `MAX_SEARCH_ROUNDS` (import or redefine as `3`).
- If no `ToolSearchTool` call, return the completion as-is.

### 2. Override `stream_with_search` in `OpenAiProvider`

Same logic as above but using `stream_inner` for each round instead of
`complete` / `complete_inner`. Pass `stream_tx` through every round so
partial tokens reach the UI continuously.

### 3. Add `with_search_engine` builder

```rust
pub fn with_search_engine(mut self, engine: Arc<dyn ToolSearchEngine>) -> Self {
    self.search_engine = engine;
    self
}
```

Default to `KeywordSearchEngine::new()` in `OpenAiProvider::new`.

### 4. Tests

Add unit tests in `#[cfg(test)] mod tests` in `src/llm/openai.rs`:

- `openai_search_loop_resolves_deferred_tool`: use `MockProvider`-style
  approach with a custom `ToolSearchEngine` that always returns `["MyTool"]`.
  Set up two mock responses: first returns a `ToolSearchTool` call, second
  returns normal content. Assert the second request's tool list includes
  `MyTool`'s full schema.
- `openai_search_loop_caps_at_max_rounds`: configure mock to always return
  `ToolSearchTool` calls. Assert we stop after `MAX_SEARCH_ROUNDS` and return
  the last completion without infinite looping.
- `openai_search_loop_skips_when_no_deferred`: pass empty `deferred_tools`.
  Assert only one request is made (no search round-trip needed).

## Out of scope

- Do not touch `AnthropicProvider`, `MockProvider`, `run_core.rs`, or any
  caller outside `src/llm/openai.rs`.
- Do not add new dependencies.
- The `ToolSearchTool` result content format for OpenAI is plain JSON text
  (not `tool_reference` blocks) — keep it simple, the model understands JSON.

## Definition of done

- `cargo build` and `cargo test --lib` green.
- All existing tests in `src/llm/openai.rs` still pass.
- 3 new unit tests added and passing.
- When `OpenAiProvider` is used with deferred tools, the first LLM request
  does NOT include deferred tool schemas; they are only added after a
  `ToolSearchTool` call resolves them.

## Final summary

List files touched, new public surface (`with_search_engine`), and test
result line.
