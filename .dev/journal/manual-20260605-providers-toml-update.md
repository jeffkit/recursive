# Manual edit: providers-toml-update

**Date**: 2026-06-05
**Goal**: Update providers.toml with current model names, context windows, pricing, and API endpoints
**Files touched**:
- `providers.toml`
- `src/cost.rs` (test fixtures updated)
- `src/llm/mod.rs` (test fixtures updated)
- `src/config.rs` (test fixture updated)
- `src/tui/app/state.rs` (test fixture updated)

**Tests added**: none (existing tests updated to match new data)

**Notes**:
- DeepSeek: `anthropic_api_base` fixed from `https://api.deepseek.com` to `https://api.deepseek.com/anthropic`
- DeepSeek: replaced deprecated `deepseek-chat`/`deepseek-reasoner` (废弃 2026-07-24) with `deepseek-v4-flash`/`deepseek-v4-pro`; kept old names pointing to new pricing for backward compat
- DeepSeek: context window updated 64K → 1M, pricing drastically reduced
- Anthropic: context window updated 200K → 1M for Opus/Sonnet; pricing updated (Opus $15→$5, Haiku $0.80→$1.00)
- OpenAI: added gpt-5.4 / gpt-5.4-mini; updated o4-mini pricing; kept gpt-4o/gpt-4o-mini
- MiniMax: context window 1M→1048576; removed MiniMax-Text-01 (obsolete); added MiniMax-M2.7; added cache_hit pricing $0.06/M
- Gemini: added 3.x series (3.5-flash, 3.1-pro-preview, 3.1-flash-lite); added cache_hit pricing for all models
- Groq: updated Kimi model ID case (Kimi-K2-Instruct → kimi-k2-instruct); added llama-4-scout
- xAI: replaced grok-3/grok-4/grok-3-mini with current lineup grok-4.3/grok-build-0.1
- StepFun: removed step-1-8k (obsolete); updated pricing for step-3-7-flash
- Zhipu: replaced glm-4-plus/glm-4-air/glm-z1-flash with glm-5/glm-4.7/glm-4.7-flash
- Moonshot: updated default model to kimi-k2.6; updated API base URL; added kimi-k2.6 entry
- Hunyuan: updated hunyuan-turbos pricing (0.60→0.11 input, 2.40→0.28 output)
- Mistral: updated mistral-small context window 32K→131K
