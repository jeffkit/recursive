---
type: Architecture
title: OpenAI-Compatible Provider
description: OpenAiProvider implements ChatProvider for any OpenAI-compatible API — DeepSeek, GLM (Zhipu), MiniMax, Moonshot, Together, Ollama, and OpenAI itself.
tags: [providers, llm, openai, deepseek, minimax, glm]
timestamp: 2026-06-18T10:00:00Z
---

# OpenAI-Compatible Provider

- **Rust struct**: `OpenAiProvider`
- **Source**: `src/llm/openai.rs`
- Always compiled (no feature flag)

## Supported Backends

Any service speaking the OpenAI Chat Completions API:

| Service | Base URL env var | Notes |
|---------|-----------------|-------|
| DeepSeek | `RECURSIVE_API_BASE=https://api.deepseek.com` | `deepseek-chat`, `deepseek-reasoner` |
| GLM (Zhipu) | `RECURSIVE_API_BASE=https://open.bigmodel.cn/api/paas/v4` | `glm-4-flash`, `glm-4-air` |
| MiniMax | `RECURSIVE_API_BASE=https://api.minimaxi.chat/v1` | `MiniMax-M3` |
| Moonshot | `RECURSIVE_API_BASE=https://api.moonshot.cn/v1` | `moonshot-v1-8k` |
| Ollama | `RECURSIVE_API_BASE=http://localhost:11434/v1` | local models |
| OpenAI | (default) | `gpt-4o`, etc. |

## Key Behaviours

- **DeepSeek max_tokens**: DeepSeek defaults to 4096 tokens per response; the provider overrides this to 8192 (or higher with `deepseek-reasoner`).
- **Reasoning content**: `reasoning_content` from DeepSeek R1 is stored in `Message` and echoed back to the API as required.
- **Retry**: MiniMax transient failures (HTTP 5xx) trigger exponential backoff.
- **Deferred tools**: Implements `supports_deferred_tools() → true` — loads tools lazily via `tool_search`.

## Config

Set via environment variables (or `Config` struct):

```
RECURSIVE_MODEL=deepseek-chat
RECURSIVE_API_KEY=sk-...
RECURSIVE_API_BASE=https://api.deepseek.com
```

## Related Concepts

- [Providers Overview](index.md)
- [Anthropic Provider](anthropic.md)
- [Agent Loop](../agent-loop.md)
