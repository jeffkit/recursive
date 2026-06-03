//! Static vendor preset catalog, embedded at compile time.

use serde::Deserialize;

/// A single model entry within a provider preset.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelSpec {
    pub name: String,
    /// Maximum input context window in tokens for this model.
    pub context_window: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderPreset {
    pub id: String,
    pub name: String,
    pub provider_type: String,
    pub api_base: String,
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
}
