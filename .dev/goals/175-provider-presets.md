# Goal 175 — Provider presets: vendor catalog + default switch to Anthropic

**Roadmap**: Phase 14 — TUI Polish (adjacent: config UX)

**Design principle check**:
- Implemented as: static data file + config CLI enhancement; no agent loop changes
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

Currently `recursive config init` asks users to type API base URLs manually,
with only 3-4 hard-coded examples in the help text. This is friction for new
users who don't know the exact URL for their vendor.

Two changes needed:
1. **Ship a vendor preset catalog** (`src/providers.toml` + a loader) so users
   can pick a vendor by short name and get the correct `api_base`, `provider_type`,
   and a recommended model pre-filled.
2. **Change the default provider** from OpenAI to Anthropic, matching where most
   Recursive users start today.

## Scope (do exactly this, no more)

### 1. New file `src/providers.toml` — vendor catalog

Create this file at the repo root (not inside `src/`, it's a data file embedded
via `include_str!`). Content:

```toml
# Recursive provider presets — auto-loaded at startup.
# Fields: id, name, provider_type, api_base, default_model, models[], mainland_accessible, key_env, key_url

[[providers]]
id = "anthropic"
name = "Anthropic"
provider_type = "anthropic"
api_base = "https://api.anthropic.com"
default_model = "claude-sonnet-4-6"
models = ["claude-opus-4-7", "claude-sonnet-4-6", "claude-haiku-4-5"]
mainland_accessible = false
key_env = "ANTHROPIC_API_KEY"
key_url = "https://console.anthropic.com/settings/keys"

[[providers]]
id = "openai"
name = "OpenAI"
provider_type = "openai"
api_base = "https://api.openai.com/v1"
default_model = "gpt-4o"
models = ["gpt-4o", "gpt-4o-mini", "o3", "o4-mini"]
mainland_accessible = false
key_env = "OPENAI_API_KEY"
key_url = "https://platform.openai.com/api-keys"

[[providers]]
id = "deepseek"
name = "DeepSeek"
provider_type = "openai"
api_base = "https://api.deepseek.com/v1"
default_model = "deepseek-chat"
models = ["deepseek-chat", "deepseek-reasoner"]
mainland_accessible = true
key_env = "DEEPSEEK_API_KEY"
key_url = "https://platform.deepseek.com/api_keys"

[[providers]]
id = "minimax"
name = "MiniMax"
provider_type = "openai"
api_base = "https://api.minimax.io/v1"
default_model = "MiniMax-M3"
models = ["MiniMax-M3", "MiniMax-Text-01"]
mainland_accessible = true
key_env = "MINIMAX_API_KEY"
key_url = "https://platform.minimax.io/user-center/basic-information/interface-key"

[[providers]]
id = "zhipu"
name = "智谱 AI (GLM)"
provider_type = "openai"
api_base = "https://open.bigmodel.cn/api/paas/v4"
default_model = "glm-4-plus"
models = ["glm-4-plus", "glm-4-air", "glm-z1-flash"]
mainland_accessible = true
key_env = "ZHIPU_API_KEY"
key_url = "https://open.bigmodel.cn/usercenter/apikeys"

[[providers]]
id = "moonshot"
name = "月之暗面 (Kimi)"
provider_type = "openai"
api_base = "https://api.moonshot.ai/v1"
default_model = "moonshot-v1-128k"
models = ["moonshot-v1-128k", "moonshot-v1-8k"]
mainland_accessible = true
key_env = "MOONSHOT_API_KEY"
key_url = "https://platform.moonshot.ai/console/api-keys"

[[providers]]
id = "doubao"
name = "字节跳动 Doubao (火山引擎 Ark)"
provider_type = "openai"
api_base = "https://ark.cn-beijing.volces.com/api/v3"
default_model = "doubao-seed-2-0-250615"
models = ["doubao-seed-2-0-250615", "doubao-1-5-pro-256k", "doubao-1-5-lite-32k"]
mainland_accessible = true
key_env = "ARK_API_KEY"
key_url = "https://console.volcengine.com/ark/region:ark+cn-beijing/apiKey"

[[providers]]
id = "dashscope"
name = "阿里云通义千问 (DashScope)"
provider_type = "openai"
api_base = "https://dashscope.aliyuncs.com/compatible-mode/v1"
default_model = "qwen-max"
models = ["qwen-max", "qwen-plus", "qwen3-235b-a22b"]
mainland_accessible = true
key_env = "DASHSCOPE_API_KEY"
key_url = "https://bailian.console.aliyun.com/?apiKey=1"

[[providers]]
id = "hunyuan"
name = "腾讯混元 (Hunyuan)"
provider_type = "openai"
api_base = "https://api.hunyuan.cloud.tencent.com/v1"
default_model = "hunyuan-turbos"
models = ["hunyuan-turbos", "hunyuan-t1", "hunyuan-lite"]
mainland_accessible = true
key_env = "HUNYUAN_API_KEY"
key_url = "https://console.cloud.tencent.com/hunyuan/api-key"

[[providers]]
id = "stepfun"
name = "阶跃星辰 (StepFun)"
provider_type = "openai"
api_base = "https://api.stepfun.com/v1"
default_model = "step-3-5-flash"
models = ["step-3-7-flash", "step-3-5-flash", "step-1-8k"]
mainland_accessible = true
key_env = "STEPFUN_API_KEY"
key_url = "https://platform.stepfun.com/account-info"

[[providers]]
id = "gemini"
name = "Google Gemini"
provider_type = "openai"
api_base = "https://generativelanguage.googleapis.com/v1beta/openai"
default_model = "gemini-2.5-pro"
models = ["gemini-2.5-pro", "gemini-2.5-flash", "gemini-2.5-flash-lite"]
mainland_accessible = false
key_env = "GEMINI_API_KEY"
key_url = "https://aistudio.google.com/app/apikey"

[[providers]]
id = "groq"
name = "Groq"
provider_type = "openai"
api_base = "https://api.groq.com/openai/v1"
default_model = "llama-3.3-70b-versatile"
models = ["llama-3.3-70b-versatile", "llama-3.1-8b-instant", "moonshotai/Kimi-K2-Instruct"]
mainland_accessible = false
key_env = "GROQ_API_KEY"
key_url = "https://console.groq.com/keys"

[[providers]]
id = "mistral"
name = "Mistral AI"
provider_type = "openai"
api_base = "https://api.mistral.ai/v1"
default_model = "mistral-large-latest"
models = ["mistral-large-latest", "mistral-small-latest", "codestral-latest"]
mainland_accessible = false
key_env = "MISTRAL_API_KEY"
key_url = "https://console.mistral.ai/api-keys"

[[providers]]
id = "xai"
name = "xAI (Grok)"
provider_type = "openai"
api_base = "https://api.x.ai/v1"
default_model = "grok-3"
models = ["grok-4", "grok-3", "grok-3-mini"]
mainland_accessible = false
key_env = "XAI_API_KEY"
key_url = "https://console.x.ai/"

[[providers]]
id = "ollama"
name = "Ollama (本地)"
provider_type = "openai"
api_base = "http://localhost:11434/v1"
default_model = "qwen2.5-coder"
models = ["qwen2.5-coder", "llama3.2", "deepseek-r1", "mistral"]
mainland_accessible = true
key_env = ""
key_url = "https://ollama.ai/"
```

### 2. New module `src/providers.rs` — loader

```rust
//! Static vendor preset catalog, embedded at compile time.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderPreset {
    pub id: String,
    pub name: String,
    pub provider_type: String,
    pub api_base: String,
    pub default_model: String,
    pub models: Vec<String>,
    pub mainland_accessible: bool,
    pub key_env: String,
    pub key_url: String,
}

#[derive(Deserialize)]
struct PresetsFile {
    providers: Vec<ProviderPreset>,
}

static PRESETS_TOML: &str = include_str!("../providers.toml");

pub fn all_presets() -> &'static [ProviderPreset] {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Vec<ProviderPreset>> = OnceLock::new();
    CACHE.get_or_init(|| {
        toml::from_str::<PresetsFile>(PRESETS_TOML)
            .expect("providers.toml is bundled at compile time and must be valid")
            .providers
    })
}

pub fn find_preset(id: &str) -> Option<&'static ProviderPreset> {
    all_presets().iter().find(|p| p.id == id)
}
```

Export from `src/lib.rs`:
```rust
pub mod providers;
pub use providers::{all_presets, find_preset, ProviderPreset};
```

### 3. Change default API base in `src/config.rs`

Line currently reads:
```rust
.unwrap_or_else(|| "https://api.openai.com/v1".into());
```

Change to:
```rust
.unwrap_or_else(|| "https://api.anthropic.com".into());
```

Also change the default `provider_type` fallback from `"openai"` to `"anthropic"`:
```rust
let provider_type = std::env::var("RECURSIVE_PROVIDER_TYPE")
    ...
    .unwrap_or_else(|| "anthropic".into());
```

And change the default `model` fallback:
```rust
.unwrap_or_else(|| "claude-sonnet-4-6".into());
```

### 4. Upgrade `recursive config init` in `src/main.rs`

Replace the current hardcoded 2-option list with a dynamic vendor picker using
`all_presets()`. The new flow:

```
Select a provider (or press Enter for Anthropic):

  International:
    1) Anthropic           claude-sonnet-4-6        [ANTHROPIC_API_KEY]
    2) OpenAI              gpt-4o                   [OPENAI_API_KEY]
    3) Google Gemini       gemini-2.5-pro            [GEMINI_API_KEY]
    4) Groq                llama-3.3-70b-versatile  [GROQ_API_KEY]
    5) Mistral AI          mistral-large-latest     [MISTRAL_API_KEY]
    6) xAI (Grok)          grok-3                   [XAI_API_KEY]

  Mainland China (直连):
    7) DeepSeek            deepseek-chat            [DEEPSEEK_API_KEY]
    8) MiniMax             MiniMax-M3               [MINIMAX_API_KEY]
    9) 智谱 AI (GLM)       glm-4-plus               [ZHIPU_API_KEY]
   10) 月之暗面 (Kimi)     moonshot-v1-128k         [MOONSHOT_API_KEY]
   11) 字节跳动 Doubao     doubao-seed-2-0-250615   [ARK_API_KEY]
   12) 阿里云通义千问      qwen-max                 [DASHSCOPE_API_KEY]
   13) 腾讯混元            hunyuan-turbos           [HUNYUAN_API_KEY]
   14) 阶跃星辰            step-3-5-flash           [STEPFUN_API_KEY]

  Local:
   15) Ollama (本地)       qwen2.5-coder            (no key needed)

  Other: enter 0 to specify custom API base manually

Choice [1]:
```

After vendor selection:
- Pre-fill `api_base` and `provider_type` from the preset
- Show `Model [<default_model>]:` — user can accept default or type another
- Show `API key (<key_env>):` — for Ollama, skip if `key_env` is empty
- Print the `key_url` as a hint: `  Get your key at: <key_url>`

If user enters `0`, fall through to the current manual flow (keep it as fallback).

Implementation: build the numbered list dynamically from `all_presets()`, grouped
by `mainland_accessible`. Print with `enumerate()`. Accept number input OR preset
`id` string.

### 5. Tests

In `src/providers.rs` tests:
- `all_presets_non_empty`: `all_presets().len() > 10`
- `find_preset_anthropic`: `find_preset("anthropic").unwrap().provider_type == "anthropic"`
- `find_preset_unknown_returns_none`: `find_preset("bogus").is_none()`
- `all_presets_have_valid_provider_type`: every preset's `provider_type` is "openai" or "anthropic"
- `default_preset_is_anthropic`: `all_presets()[0].id == "anthropic"`

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `recursive config init` shows the numbered vendor list
- Default provider resolves to Anthropic when no env/config is set
- `find_preset("deepseek").unwrap().api_base` == `"https://api.deepseek.com/v1"`

## Notes for the agent

- `providers.toml` goes in the **repo root** (same level as `Cargo.toml`), not inside `src/`.
  `include_str!("../providers.toml")` from `src/providers.rs` resolves correctly.
- `toml` crate is already a dependency (`Cargo.toml` has `toml = ...`). No new dep needed.
- `serde` with `derive` feature is already enabled. Use `#[derive(Deserialize)]`.
- The `config init` wizard is in `src/main.rs` — find it by searching for
  `"Select provider type"` or the `fn cmd_config_init` function name.
- Changing the default in `src/config.rs` may break the test
  `offline_mode_and_config_file_resolution` in `src/tui/runtime_builder.rs` —
  that test writes a config with `type = "openai"`; it should still pass since
  it sets an explicit type. Double-check.
- **DO NOT modify**: `src/tui/`, `src/agent.rs`, `src/llm/`, anything unrelated
  to config/providers/main CLI.
- **Files to touch**: `providers.toml` (new), `src/providers.rs` (new),
  `src/lib.rs`, `src/config.rs`, `src/main.rs`.
