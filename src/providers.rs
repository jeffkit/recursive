//! Static vendor preset catalog, embedded at compile time.

use serde::Deserialize;

/// Per-million-token pricing embedded in a provider preset model entry. USD.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct ModelPricingSpec {
    pub input_per_million: f64,
    pub output_per_million: f64,
    /// Cache-hit input price per million tokens. Defaults to input_per_million when absent.
    pub cache_hit_input_per_million: Option<f64>,
}

/// A single model entry within a provider preset.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelSpec {
    pub name: String,
    /// Maximum input context window in tokens for this model.
    pub context_window: usize,
    /// Optional pricing. When present, used by `pricing_for()` instead of hard-coded values.
    pub pricing: Option<ModelPricingSpec>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderPreset {
    pub id: String,
    pub name: String,
    pub provider_type: String,
    pub api_base: String,
    /// Optional Anthropic-protocol base URL for providers that support both
    /// OpenAI-compatible and Anthropic Messages API formats (e.g. DeepSeek,
    /// MiniMax). When set and the user selects `provider_type = "anthropic"`,
    /// `Config::from_env` uses this URL instead of `api_base`.
    pub anthropic_api_base: Option<String>,
    pub default_model: String,
    pub models: Vec<ModelSpec>,
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

/// Look up a preset by its API base URL. Used to recover the preset id
/// from a config file that only stores `api_base` (e.g. `recursive config show`)
/// and to pick sensible defaults in the manual branch of `recursive init` instead
/// of guessing the model from URL substrings.
pub fn find_preset_by_api_base(url: &str) -> Option<&'static ProviderPreset> {
    all_presets().iter().find(|p| p.api_base == url)
}

/// Look up pricing for a model name across all presets.
/// Returns `None` if the model is not listed or has no pricing field.
pub fn find_model_pricing(model: &str) -> Option<&'static ModelPricingSpec> {
    for preset in all_presets() {
        for spec in &preset.models {
            if spec.name == model {
                return spec.pricing.as_ref();
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_presets_non_empty() {
        assert!(all_presets().len() > 10);
    }

    #[test]
    fn find_preset_anthropic() {
        let preset = find_preset("anthropic").unwrap();
        assert_eq!(preset.provider_type, "anthropic");
    }

    #[test]
    fn find_preset_unknown_returns_none() {
        assert!(find_preset("bogus").is_none());
    }

    #[test]
    fn all_presets_have_valid_provider_type() {
        for p in all_presets() {
            assert!(
                p.provider_type == "openai" || p.provider_type == "anthropic",
                "preset {} has invalid provider_type: {}",
                p.id,
                p.provider_type
            );
        }
    }

    #[test]
    fn default_preset_is_anthropic() {
        assert_eq!(all_presets()[0].id, "anthropic");
    }

    #[test]
    fn find_preset_deepseek_api_base() {
        let preset = find_preset("deepseek").unwrap();
        assert_eq!(preset.api_base, "https://api.deepseek.com/v1");
    }

    #[test]
    fn find_preset_by_api_base_known() {
        let preset =
            find_preset_by_api_base("https://api.deepseek.com/v1").expect("deepseek preset");
        assert_eq!(preset.id, "deepseek");
    }

    #[test]
    fn find_preset_by_api_base_unknown() {
        assert!(find_preset_by_api_base("https://example.com/v1").is_none());
    }
}
