# LLM Gateway Compatibility Guide

This page collects the implicit wire-protocol constraints Recursive has
already learned the hard way (#15 / #16 / #17), and the known pitfalls of
common LLM gateways / translation proxies (new-api, one-api, Bedrock). Each
constraint lists: background, trigger, symptom (the typical gateway error),
workaround, code anchor, and regression test. The code anchors are the
source of truth — if this page and the code disagree, trust the code.

---

## 1. Anthropic `input_schema`: no top-level `anyOf` / `oneOf` / `allOf` (#15)

**Background.** Anthropic's Messages API rejects `oneOf` / `allOf` / `anyOf`
at the **top level** of a tool's `input_schema` with HTTP 400. Combinators
nested *inside* a property are fine.

**Trigger.** A tool schema whose top level (not inside `properties`) contains
`anyOf` / `oneOf` / `allOf`, sent via the Anthropic API or an
Anthropic-compatible translation path (e.g. Bedrock).

**Symptom.** The very first request fails with 400, complaining the
`input_schema` is invalid.

**Workaround.**
- Recursive already strips top-level combinators at runtime and hoists
  properties: `sanitize_input_schema()` in `src/llm/anthropic.rs:811`
  (call site ~L800). MCP tools with third-party schemas are covered by this.
- For tool authors: keep the top level of your schema to
  `type` / `properties` / `required` only; put combinators *inside* the
  specific property. See `src/tools/estimate_tokens.rs` — its schema once
  violated this rule and was the direct cause of #15 (since fixed).

**Regression tests.** `src/llm/anthropic.rs:1993-2057` (top-level combinators
never go on the wire); `src/tools/registry.rs:994`
(`standard_tool_schemas_have_no_top_level_combinators` over the canonical
tool set).

---

## 2. OpenAI tool-call assistant with empty content must serialize `content: null` (#16)

**Background.** OpenAI's own API tolerates `"content": ""` on an assistant
message that carries `tool_calls`, but strict OpenAI→Anthropic translation
gateways (some new-api / one-api channels) materialize the empty string as an
*empty text block* and reject it.

**Trigger.** Using a strict OpenAI→Anthropic translation gateway, when an
assistant message has `content: ""` together with `tool_calls`.

**Symptom.** HTTP 400: `text content blocks must be non-empty`.

**Workaround.** Recursive's OpenAI provider already emits `content: null`
(the canonical OpenAI wire shape for a tool-call-only assistant message) in
`serialize_message`, `src/llm/openai.rs:1050-1056`. If you build your own
client or proxy layer around Recursive, follow the same rule: empty content +
tool_calls ⇒ `null`, never `""`.

**Regression test.** `src/llm/openai.rs:2294`
(`serialize_message_assistant_tool_calls_empty_content_is_null`); a
companion case at ~L2316 asserts non-empty content is preserved.

---

## 3. `--model` override must re-derive `max_tokens` (#17)

**Background.** `max_tokens` limits are per-model (preset-driven). Switching
models with `--model` to one with a lower cap while the config file sets a
higher `agent.max_tokens` yields a request that exceeds the new model's limit.

**Trigger.** `--model` (or `--provider`) switches to a model whose
`max_tokens` ceiling is lower than the `agent.max_tokens` in your config
file.

**Symptom.** The first request after the switch fails with 400
(`max_tokens` above the model's limit).

**Workaround.**
- `Config::resolve_max_tokens()` (`src/config.rs`) resolves the priority:
  `RECURSIVE_MAX_TOKENS` env var > preset's `ModelSpec.max_tokens` > config
  file `agent.max_tokens` > crate default. The CLI
  (`crates/recursive-cli/src/main.rs`) and the TUI
  (`crates/recursive-tui/src/runtime_builder.rs`) both re-derive
  after an override, so this is handled automatically.
- Escape hatch: set `RECURSIVE_MAX_TOKENS` explicitly to pin the value.

**Regression tests.** `src/config.rs:1226`
(`resolve_max_tokens_rederives_after_model_override`) and the three variant
tests following it.

---

## 4. Known gateway pitfalls & triage checklist

### new-api / one-api
- Strict OpenAI→Anthropic translation channels hit the #16 empty-content
  trap (empty string ⇒ empty text block ⇒ 400).
- **Triage:**
  1. Compare a direct `curl` to the upstream provider vs. through the
     gateway with the identical payload.
  2. Check the gateway's upstream request log; confirm the assistant
     tool-call message went out as `content: null`, not `""`.
  3. If the payload shows `content: null` upstream and still 400s, suspect
     the gateway's translation layer, not Recursive.

### Bedrock
- The converse path is subject to the #15 top-level-combinator trap in
  `input_schema`.
- **Triage:**
  1. Inspect the request body's `input_schema` — is there a top-level
     `anyOf`/`oneOf`/`allOf`?
  2. Confirm the payload was sanitized (`sanitize_input_schema()`,
     `src/llm/anthropic.rs:811`).
  3. If a third-party (MCP) tool schema still carries a top-level
     combinator on the wire, file a bug — that path is covered by tests.

### Generic steps
1. Enable debug logging and inspect the outbound payload.
2. Walk through constraints 1–3 above and eliminate each in turn.
3. Only then suspect the gateway itself (and check its own logs).
