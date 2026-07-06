//! Remote provider preset catalog — download, cache, and merge with the
//! bundled + `providers.d` preset sources.
//!
//! # Design
//!
//! The bundled `providers.toml` is the compile-time fallback. A JSON cache
//! file at `<user_data_dir>/providers_cache.json` (written by
//! `recursive providers update`) holds presets fetched from an upstream
//! catalog URL. When the kernel resolves the *effective* preset list (see
//! [`crate::providers::all_presets_effective`]), cached presets override
//! bundled entries with the same `id` (so stale bundled pricing gets
//! refreshed upstream), while bundled presets absent from the cache are
//! kept as-is. User overrides from `providers.d/` are layered on top by
//! [`crate::providers::all_presets_effective`].
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
//! The `providers` array uses the same structure as [`ProviderPreset`] plus
//! the same optional fields (`anthropic_api_base`, `pricing`, etc.).
//!
//! # Network policy
//!
//! The only network touch is [`fetch_and_save`], invoked explicitly by
//! `recursive providers update` (and, if enabled, the best-effort
//! [`spawn_background_refresh`]). Preset *lookup* never hits the network —
//! [`load_cache`] reads a local JSON file. URLs are validated by
//! [`validate_providers_url`] which mirrors the SSRF guard in
//! `tools::web_fetch` (scheme + loopback/private/metadata blocking).

use std::net::IpAddr;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::paths::user_data_dir;
use crate::providers::ProviderPreset;

/// How old the cache can be before [`needs_update`] returns true.
pub const CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Default upstream catalog. Users can override via the
/// `RECURSIVE_PROVIDERS_URL` env var or `recursive providers update --url`.
/// The repo `jeffkit/recursive-providers` publishes `providers.json` here.
pub const DEFAULT_PROVIDERS_URL: &str =
    "https://raw.githubusercontent.com/jeffkit/recursive-providers/main/providers.json";

/// Env var override for the upstream URL.
pub const PROVIDERS_URL_ENV: &str = "RECURSIVE_PROVIDERS_URL";

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

/// Return the configured upstream URL: `RECURSIVE_PROVIDERS_URL` if set,
/// otherwise [`DEFAULT_PROVIDERS_URL`].
pub fn configured_url() -> String {
    std::env::var(PROVIDERS_URL_ENV).unwrap_or_else(|_| DEFAULT_PROVIDERS_URL.to_string())
}

/// Return path to the cache file: `<user_data_dir>/providers_cache.json`.
pub fn cache_path() -> PathBuf {
    user_data_dir().join("providers_cache.json")
}

/// Load the cache from disk. Returns `None` if the file is absent or
/// unreadable — callers fall back to bundled presets in that case.
pub fn load_cache() -> Option<ProvidersCache> {
    let path = cache_path();
    if !path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

/// True if the cache is absent or older than [`CACHE_TTL`].
pub fn needs_update() -> bool {
    let path = cache_path();
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

/// Validate an upstream catalog URL. Mirrors the SSRF guard in
/// `tools::web_fetch`: requires an `http(s)` scheme and rejects
/// loopback / private / link-local / cloud-metadata hosts so a
/// misconfigured `RECURSIVE_PROVIDERS_URL` cannot be turned into an
/// internal-network probe.
pub fn validate_providers_url(url: &str) -> Result<()> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(Error::Config {
            message: "providers URL must start with http:// or https://".into(),
        });
    }

    let host = url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .and_then(|host_port| {
            if host_port.starts_with('[') {
                host_port.find(']').map(|i| &host_port[1..i])
            } else {
                host_port.split(':').next()
            }
        })
        .unwrap_or("");

    let host_lower = host.to_ascii_lowercase();
    if host_lower == "localhost"
        || host_lower.ends_with(".localhost")
        || host_lower == "metadata.google.internal"
    {
        return Err(Error::Config {
            message: format!("SSRF protection: host '{host}' is not allowed"),
        });
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_non_routable_ip(ip) {
            return Err(Error::Config {
                message: format!("SSRF protection: IP address '{ip}' is not routable"),
            });
        }
    }

    Ok(())
}

/// Returns true for IP addresses that must not be reached
/// (loopback, private RFC-1918, link-local, cloud metadata, unspecified).
/// Duplicated from `tools::web_fetch` rather than shared, because the
/// fetch tool's helper is private and lifting it into a common util would
/// widen this change's blast radius.
fn is_non_routable_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Download the providers JSON from `url`, parse it, write to the cache
/// file, and return the parsed [`ProvidersCache`]. The URL is validated by
/// [`validate_providers_url`] before any request is made.
///
/// Async — must be called from a tokio runtime.
pub async fn fetch_and_save(url: &str) -> Result<ProvidersCache> {
    validate_providers_url(url)?;

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

/// Merge cached presets over bundled ones. For each preset in `cache`, the
/// cached copy wins (placed first); bundled presets whose `id` is absent
/// from the cache are appended afterwards. Used by
/// [`crate::providers::all_presets_effective`].
pub fn merge_over_bundled(
    bundled: &[ProviderPreset],
    cache: &[ProviderPreset],
) -> Vec<ProviderPreset> {
    let mut result: Vec<ProviderPreset> = cache.to_vec();
    for bp in bundled {
        if !result.iter().any(|cp| cp.id == bp.id) {
            result.push(bp.clone());
        }
    }
    result
}

/// Spawn a best-effort background task that silently refreshes the cache if
/// it is stale. Errors are logged at `debug` level and never surface to the
/// caller. Returns immediately; the refresh happens concurrently. No-op if
/// the cache is fresh.
///
/// Only spawned by long-running command paths in the CLI, and only when
/// `RECURSIVE_PROVIDERS_AUTO_REFRESH=1` is set — one-shot commands must not
/// make surprise network requests.
pub fn spawn_background_refresh() {
    if !needs_update() {
        return;
    }
    if std::env::var("RECURSIVE_PROVIDERS_AUTO_REFRESH")
        .map(|v| v != "1")
        .unwrap_or(true)
    {
        return;
    }
    let url = configured_url();
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
    use crate::providers::{all_presets, ModelPricingSpec, ModelSpec};
    use crate::test_util::PinnedRecursiveHome;

    #[test]
    fn merge_overrides_bundled() {
        let bundled = all_presets();
        let mut fake = bundled[0].clone();
        fake.default_model = "fake-model-override".to_string();
        let merged = merge_over_bundled(bundled, &[fake]);
        assert_eq!(merged[0].default_model, "fake-model-override");
        assert_eq!(merged.len(), bundled.len());
    }

    #[test]
    fn merge_adds_new_preset() {
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
        let merged = merge_over_bundled(bundled, &[new_preset]);
        assert_eq!(merged.len(), bundled.len() + 1);
        assert!(merged.iter().any(|p| p.id == "test-only-provider"));
    }

    #[test]
    fn merge_empty_cache_returns_bundled() {
        let bundled = all_presets();
        let merged = merge_over_bundled(bundled, &[]);
        assert_eq!(merged.len(), bundled.len());
        assert_eq!(merged[0].id, bundled[0].id);
    }

    #[test]
    fn needs_update_true_when_no_cache() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let _pin = PinnedRecursiveHome::new(tmp.path());
        assert!(needs_update());
        Ok(())
    }

    #[test]
    fn validate_url_accepts_https_public_host() {
        assert!(validate_providers_url(DEFAULT_PROVIDERS_URL).is_ok());
        assert!(validate_providers_url("https://example.com/x.json").is_ok());
    }

    #[test]
    fn validate_url_rejects_non_http_scheme() {
        assert!(validate_providers_url("ftp://example.com/x").is_err());
        assert!(validate_providers_url("example.com/x").is_err());
    }

    #[test]
    fn validate_url_rejects_ssrf_targets() {
        assert!(validate_providers_url("http://127.0.0.1/x").is_err());
        assert!(validate_providers_url("http://localhost/x").is_err());
        assert!(validate_providers_url("http://10.0.0.1/x").is_err());
        assert!(validate_providers_url("http://169.254.169.254/x").is_err());
        assert!(validate_providers_url("http://[::1]/x").is_err());
        assert!(validate_providers_url("http://metadata.google.internal/x").is_err());
    }

    #[test]
    fn load_cache_returns_none_when_absent() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let _pin = PinnedRecursiveHome::new(tmp.path());
        assert!(load_cache().is_none());
        Ok(())
    }

    #[test]
    fn fetch_roundtrips_through_cache_file() -> Result<(), Box<dyn std::error::Error>> {
        // No network: write a cache envelope directly and confirm load_cache
        // parses it back. PinnedRecursiveHome points user_data_dir() at the
        // temp dir on every platform (incl. Windows, where HOME/USERPROFILE
        // pinning alone is unreliable).
        let tmp = tempfile::tempdir()?;
        let _pin = PinnedRecursiveHome::new(tmp.path());

        let cache = ProvidersCache {
            schema_version: 1,
            updated_at: "2026-06-06T00:00:00Z".to_string(),
            source_url: "https://example.com/providers.json".to_string(),
            providers: vec![ProviderPreset {
                id: "roundtrip".to_string(),
                name: "Roundtrip".to_string(),
                provider_type: "openai".to_string(),
                api_base: "https://rt.example.com/v1".to_string(),
                anthropic_api_base: None,
                default_model: "rt-1".to_string(),
                models: vec![],
                mainland_accessible: false,
                key_env: "RT_KEY".to_string(),
                key_url: "https://rt.example.com/keys".to_string(),
            }],
        };
        let path = cache_path();
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, serde_json::to_string_pretty(&cache)?)?;

        let loaded = load_cache().expect("cache should load");
        assert_eq!(loaded.schema_version, 1);
        assert_eq!(loaded.providers.len(), 1);
        assert_eq!(loaded.providers[0].id, "roundtrip");
        Ok(())
    }

    // ── Additional gap-filling tests ─────────────────────────────────────────

    #[test]
    fn cache_ttl_is_seven_days() {
        let seven_days = Duration::from_secs(7 * 24 * 60 * 60);
        assert_eq!(CACHE_TTL, seven_days, "CACHE_TTL must be exactly 7 days");
    }

    #[test]
    fn configured_url_returns_default_when_env_unset() {
        let _lock = crate::test_util::env_lock();
        std::env::remove_var(PROVIDERS_URL_ENV);
        let url = configured_url();
        assert_eq!(url, DEFAULT_PROVIDERS_URL, "must return default when env var unset");
    }

    #[test]
    fn configured_url_honors_env_override() {
        let _lock = crate::test_util::env_lock();
        let custom = "https://custom.example.com/providers.json";
        std::env::set_var(PROVIDERS_URL_ENV, custom);
        let url = configured_url();
        std::env::remove_var(PROVIDERS_URL_ENV);
        assert_eq!(url, custom);
    }

    #[test]
    fn validate_url_rejects_192_168_private() {
        assert!(validate_providers_url("http://192.168.1.1/x").is_err());
    }

    #[test]
    fn validate_url_rejects_172_16_private() {
        assert!(validate_providers_url("http://172.16.0.1/x").is_err());
    }

    #[test]
    fn validate_url_rejects_ipv6_loopback() {
        assert!(validate_providers_url("http://[::1]/x").is_err());
    }

    #[test]
    fn validate_url_accepts_http_public() {
        assert!(validate_providers_url("http://example.com/providers.json").is_ok());
    }

    #[test]
    fn is_non_routable_ip_loopback_true() {
        use std::net::IpAddr;
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        assert!(is_non_routable_ip(ip));
    }

    #[test]
    fn is_non_routable_ip_private_true() {
        use std::net::IpAddr;
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        assert!(is_non_routable_ip(ip));
        let ip2: IpAddr = "192.168.0.1".parse().unwrap();
        assert!(is_non_routable_ip(ip2));
    }

    #[test]
    fn is_non_routable_ip_public_false() {
        use std::net::IpAddr;
        let ip: IpAddr = "8.8.8.8".parse().unwrap();
        assert!(!is_non_routable_ip(ip));
    }

    #[test]
    fn needs_update_false_when_fresh_cache() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let _pin = PinnedRecursiveHome::new(tmp.path());

        // Write a fresh cache file
        let path = cache_path();
        std::fs::create_dir_all(path.parent().unwrap())?;
        let cache = ProvidersCache {
            schema_version: 1,
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            source_url: "https://example.com/providers.json".to_string(),
            providers: vec![],
        };
        std::fs::write(&path, serde_json::to_string_pretty(&cache)?)?;

        // File was just written — modification time is NOW → not stale
        assert!(!needs_update(), "freshly written cache must not need update");
        Ok(())
    }
}
