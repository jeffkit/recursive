//! Remote provider preset catalog — download, cache, and merge with bundled presets.
//!
//! # Design
//!
//! The bundled `providers.toml` is always the fallback. A JSON cache file at
//! `~/.recursive/providers_cache.json` (written by `recursive providers update`)
//! takes precedence: any preset whose `id` appears in the cache replaces the
//! bundled entry; presets only in the bundle are kept as-is.
//!
//! # Cache file format
//!
//! ```json
//! {
//!   "schema_version": 1,
//!   "updated_at": "2026-06-06T00:00:00Z",
//!   "source_url": "https://...",
//!   "providers": [ ... ]
//! }
//! ```
//!
//! The `providers` array uses the same structure as `ProviderPreset` plus the
//! same optional fields (`anthropic_api_base`, `pricing`, etc.).

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::paths::user_data_dir;
use crate::providers::ProviderPreset;
#[cfg(test)]
use crate::providers::{ModelPricingSpec, ModelSpec};

/// How old the cache can be before `needs_update()` returns true.
pub const CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60); // 7 days

/// Default upstream URL. Users can override via `RECURSIVE_PROVIDERS_URL` or
/// `recursive providers update --url <URL>`.
pub const DEFAULT_PROVIDERS_URL: &str =
    "https://raw.githubusercontent.com/jeffkit/recursive-providers/main/providers.json";

/// On-disk cache envelope.
#[derive(Debug, Serialize, Deserialize)]
pub struct ProvidersCache {
    pub schema_version: u32,
    /// RFC 3339 timestamp of when this cache was written.
    pub updated_at: String,
    /// URL this cache was fetched from.
    pub source_url: String,
    pub providers: Vec<ProviderPreset>,
}

/// Return path to the cache file: `~/.recursive/providers_cache.json`.
pub fn cache_path() -> PathBuf {
    user_data_dir().join("providers_cache.json")
}

/// Load the cache from disk. Returns `None` if the file is absent or unreadable.
pub fn load_cache() -> Option<ProvidersCache> {
    let path = cache_path();
    if !path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

/// True if the cache is absent or older than `CACHE_TTL`.
pub fn needs_update() -> bool {
    let path = cache_path();
    if !path.exists() {
        return true;
    }
    let metadata = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(_) => return true,
    };
    let modified = match metadata.modified() {
        Ok(t) => t,
        Err(_) => return true,
    };
    SystemTime::now()
        .duration_since(modified)
        .map(|age| age > CACHE_TTL)
        .unwrap_or(true)
}

/// Download the providers JSON from `url`, parse it, write to the cache file,
/// and return the parsed `ProvidersCache`.
///
/// This is an async function and must be called from a tokio runtime.
pub async fn fetch_and_save(url: &str) -> Result<ProvidersCache> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(Error::Http)?;

    let body = client
        .get(url)
        .header(
            "User-Agent",
            concat!("recursive-agent/", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await
        .map_err(Error::Http)?
        .error_for_status()
        .map_err(Error::Http)?
        .text()
        .await
        .map_err(Error::Http)?;

    let cache: ProvidersCache = serde_json::from_str(&body).map_err(|e| Error::Config {
        message: format!("providers JSON parse error: {e}"),
    })?;

    if cache.schema_version != 1 {
        return Err(Error::Config {
            message: format!(
                "unsupported providers cache schema version: {}",
                cache.schema_version
            ),
        });
    }

    // Ensure parent directory exists.
    let path = cache_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(Error::Io)?;
    }
    let serialized = serde_json::to_string_pretty(&cache).map_err(|e| Error::Config {
        message: format!("failed to serialize cache: {e}"),
    })?;
    std::fs::write(&path, serialized).map_err(Error::Io)?;

    Ok(cache)
}

/// Merge cached presets over the bundled ones.
///
/// For each preset in `cache`, if a preset with the same `id` exists in
/// `bundled`, it is replaced. Presets only in `bundled` are appended at the
/// end, preserving order of the cache list first.
pub fn merge(bundled: &[ProviderPreset], cache: &[ProviderPreset]) -> Vec<ProviderPreset> {
    let mut result: Vec<ProviderPreset> = cache.to_vec();
    for bp in bundled {
        if !result.iter().any(|cp| cp.id == bp.id) {
            result.push(bp.clone());
        }
    }
    result
}

/// Spawn a background task that silently refreshes the cache if stale.
/// Errors are logged at debug level and never surface to the caller.
/// Returns immediately; the refresh happens concurrently.
pub fn spawn_background_refresh() {
    if !needs_update() {
        return;
    }
    let url = std::env::var("RECURSIVE_PROVIDERS_URL")
        .unwrap_or_else(|_| DEFAULT_PROVIDERS_URL.to_string());
    tokio::spawn(async move {
        match fetch_and_save(&url).await {
            Ok(cache) => {
                tracing::debug!(
                    "providers cache refreshed: {} presets from {}",
                    cache.providers.len(),
                    cache.source_url
                );
            }
            Err(e) => {
                tracing::debug!("providers cache refresh failed (non-fatal): {e}");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::all_presets;

    #[test]
    fn merge_cache_overrides_bundled() {
        let bundled = all_presets();
        // Build a fake cache with just one modified preset.
        let mut fake = bundled[0].clone();
        fake.default_model = "fake-model-override".to_string();
        let merged = merge(bundled, &[fake]);
        assert_eq!(merged[0].default_model, "fake-model-override");
        // All other bundled presets should still be present.
        assert_eq!(merged.len(), bundled.len());
    }

    #[test]
    fn merge_cache_adds_new_preset() {
        let bundled = all_presets();
        let new_preset = ProviderPreset {
            id: "test-only-provider".to_string(),
            name: "Test".to_string(),
            provider_type: "openai".to_string(),
            api_base: "https://test.example.com/v1".to_string(),
            anthropic_api_base: None,
            default_model: "test-model".to_string(),
            models: vec![ModelSpec {
                name: "test-model".to_string(),
                context_window: 4096,
                pricing: Some(ModelPricingSpec {
                    input_per_million: 0.10,
                    output_per_million: 0.20,
                    cache_hit_input_per_million: None,
                }),
            }],
            mainland_accessible: false,
            key_env: "TEST_API_KEY".to_string(),
            key_url: "https://test.example.com/keys".to_string(),
        };
        let merged = merge(bundled, &[new_preset]);
        assert_eq!(merged.len(), bundled.len() + 1);
        assert!(merged.iter().any(|p| p.id == "test-only-provider"));
    }

    #[test]
    fn merge_empty_cache_returns_bundled() {
        let bundled = all_presets();
        let merged = merge(bundled, &[]);
        assert_eq!(merged.len(), bundled.len());
        assert_eq!(merged[0].id, bundled[0].id);
    }

    #[test]
    fn needs_update_returns_true_when_no_cache() {
        // Use a temp RECURSIVE_HOME so we don't touch the real cache.
        let tmp = tempfile::tempdir().unwrap();
        let _pin = crate::test_util::PinnedRecursiveHome::new(tmp.path());
        assert!(needs_update());
    }
}
