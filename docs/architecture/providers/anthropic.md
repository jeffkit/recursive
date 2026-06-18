---
type: Architecture
title: Anthropic Provider
description: AnthropicProvider for Claude models. Supports extended thinking, streaming, and deferred tool loading. Feature-gated by the anthropic Cargo feature.
tags: [providers, llm, anthropic, claude]
timestamp: 2026-06-18T10:00:00Z
---

# Anthropic Provider

- **Rust struct**: `AnthropicProvider`
- **Source**: `src/llm/anthropic.rs`
- **Feature flag**: `anthropic` (opt-in at compile time)

## Supported Models

Any model in Anthropic's Claude family: `claude-opus-4-5`, `claude-sonnet-4-5`,
`claude-haiku-3-5`, etc.

## Key Behaviours

- **Extended thinking**: Supports `thinking` blocks in completions (for models
  that support it). Stored in `Message::thinking_content`.
- **Streaming**: Implements streaming via `ChatProvider::complete_stream`.
- **Deferred tools**: Implements `supports_deferred_tools() → true`.
- **Token budget**: Respects `max_tokens` config; defaults to model's context window.

## Config

```
RECURSIVE_MODEL=claude-opus-4-5
RECURSIVE_API_KEY=sk-ant-...
# RECURSIVE_API_BASE not needed — hardcoded to api.anthropic.com
```

## Related Concepts

- [Providers Overview](index.md)
- [OpenAI-Compatible Provider](openai-compat.md)
- [Agent Loop](../agent-loop.md)
