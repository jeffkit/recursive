# Goal 34 — Anthropic Provider adapter

**Roadmap**: 2.3 — Anthropic Provider

**Design principle check**:
- Implemented as: **new LlmProvider adapter** `src/llm/anthropic.rs`.
  Trait `LlmProvider` is unchanged. The agent loop sees an
  `Arc<dyn LlmProvider>` and doesn't care about wire format.
- ❌ Does NOT branch inside `agent.rs`.

## Why

Users want to switch between OpenAI-compatible APIs and
Anthropic's Messages API. The structural differences (system message
as a top-level field, `content` blocks vs string, different
`tool_use` payload shape) make Anthropic ≠ OpenAI-compatible.

**Convenient testing surface**: MiniMax (`https://api.minimaxi.com/anthropic`)
and DeepSeek (`https://api.deepseek.com/anthropic`) both expose
Anthropic-compatible endpoints — so this adapter can be exercised
against existing keys without needing a new Anthropic API key.

## Scope

Touches: new `src/llm/anthropic.rs`, `src/llm/mod.rs` (re-export).

1. New file `src/llm/anthropic.rs`:
   - `pub struct AnthropicProvider { client, base_url, api_key, model,
     retry_policy }` mirroring `OpenAiProvider`'s shape.
   - `impl LlmProvider for AnthropicProvider`:
     - `async fn complete(messages, tools) -> Result<Completion>`:
       - Build the request body in Anthropic's shape:
         ```json
         {
           "model": "...",
           "max_tokens": 4096,
           "system": "<system message text>",
           "messages": [<user/assistant turns, content blocks>],
           "tools": [<tool specs in Anthropic format>]
         }
         ```
       - POST to `{base_url}/v1/messages` with header
         `x-api-key: {api_key}` and `anthropic-version: 2023-06-01`.
       - Parse response: `content[0].text` for text reply,
         `content[?].type == "tool_use"` for tool calls, map to
         `ToolCall { id, name, arguments }`.
       - Convert `usage` block to our `TokenUsage` shape.
   - Retry logic mirrors `OpenAiProvider` (reuse `RetryPolicy`).

2. In `src/llm/mod.rs`:
   - `pub use anthropic::AnthropicProvider;`

3. Tests in `src/llm/anthropic.rs`:
   - **Test A**: mock TCP server returns a canned Anthropic response;
     `complete` parses it correctly (text + token usage). **Use
     explicit reqwest timeout per AGENTS.md section 5.**
   - **Test B**: error response (e.g. 401) is mapped to `Error::Llm`
     with the model name embedded (consistency with goal-30's
     `make_err` pattern).
   - **Test C**: tool_use response shape correctly extracts a
     `ToolCall` with the expected id/name/arguments.

## Acceptance

- `cargo build` green.
- `cargo test` green (140 baseline + 3 new = 143+).
- `cargo clippy --all-targets -- -D warnings` green.
- `cargo fmt --all` clean.
- Does NOT modify `OpenAiProvider` or break any existing tests.

## Notes for the agent

- Read `src/llm/openai.rs` first as your template. The provider
  shape, retry logic, error mapping, and TokenUsage parsing are
  almost identical — only the request/response JSON differs.
- Anthropic's `system` is a top-level string field, NOT a message in
  `messages[]`. Filter the first message and extract its content if
  it's `Role::System`.
- Anthropic's `messages[]` cannot start with `assistant` — the API
  rejects. Assert this in `complete` or filter as needed.
- Tool calling: Anthropic uses `tool_use` content blocks. Each call
  has `id`, `name`, `input` (the arguments). Map `input` directly to
  our `ToolCall.arguments` (it's already a JSON object).
- Tool results come back as `tool_result` content blocks. Format
  agent's tool-result messages accordingly. (Most LLMs are forgiving;
  start with a clean Anthropic-spec mapping.)
- **MANDATORY** reqwest timeouts in tests:
  `.timeout(Duration::from_secs(2)).connect_timeout(Duration::from_secs(1))`.
- Use `apply_patch`. `.to_string()` over `.into()` in tests.
- NO new Cargo deps — `reqwest` + `serde_json` already cover this.
