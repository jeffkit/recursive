---
type: Index
title: LLM Providers Overview
description: ChatProvider trait and all concrete LLM provider implementations. OpenAI-compatible (DeepSeek, GLM, MiniMax, Moonshot, Ollama) and Anthropic (Claude).
tags: [providers, llm, architecture]
timestamp: 2026-06-18T10:00:00Z
---

# LLM Providers

Source: `src/llm/`

The agent depends only on the `ChatProvider` trait — provider details are
hidden behind it. Swapping providers requires only a config change.

## ChatProvider Trait (src/llm/mod.rs)

```rust
#[async_trait]
pub trait ChatProvider: Send + Sync {
    async fn complete(&self, messages: &[Message], tools: &[ToolSpec]) -> Result<Completion>;
    fn supports_deferred_tools(&self) -> bool { false }
    async fn complete_structured(...) -> Result<Value> { ... }
}
```

## Implementations

* [OpenAI-Compatible](openai-compat.md) — `OpenAiProvider`: DeepSeek, GLM, MiniMax, Moonshot, Together, Ollama, OpenAI
* [Anthropic](anthropic.md) — `AnthropicProvider`: Claude models, extended thinking

## MockProvider

- **Source**: `src/llm/mock.rs`
- **Feature**: `test-utils`
- Used in unit tests to inject scripted completions without hitting the network.

## Pricing Catalog

`src/llm/pricing.rs` contains per-million-token pricing for known models.
`pricing_for(model)` returns `ModelPricing { input_per_million, output_per_million }`.
Used by cost tracking after each completion.

## Deferred Tool Loading

Both `OpenAiProvider` and `AnthropicProvider` implement the deferred tool
pattern: a small `tool_search` tool is always available, and the full catalog
is loaded lazily when the model requests a specific tool name. Reduces prompt
size for models with large tool catalogs.

## Related Concepts

- [Agent Loop](../agent-loop.md) — where ChatProvider::complete is called
- [Overview](../overview.md) — component map
