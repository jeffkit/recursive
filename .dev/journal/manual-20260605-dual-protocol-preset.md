# Manual edit: dual-protocol-preset

**Date**: 2026-06-05
**Goal**: Allow providers.toml presets to declare both OpenAI-compatible and Anthropic
Messages API endpoints, so users can select protocol via provider_type without manually
specifying api_base.
**Files touched**:
- `providers.toml` — added `anthropic_api_base` to deepseek and minimax presets
- `src/providers.rs` — added `anthropic_api_base: Option<String>` field to `ProviderPreset`
- `src/config.rs` — moved `provider_type` resolution before `api_base`; when
  `provider_type = "anthropic"` and the preset has `anthropic_api_base`, that URL
  is used instead of the default `api_base`
**Tests added**: none (1101 existing lib tests pass)
**Notes**:
- Explicit `api_base` in config file or env var still wins — preset selection is only
  the fallback, preserving backward compatibility.
- User workflow: set `preset = "deepseek"` + `provider_type = "anthropic"` in
  config.toml; system auto-picks `https://api.deepseek.com` (Anthropic endpoint)
  instead of `https://api.deepseek.com/v1` (OpenAI endpoint).
