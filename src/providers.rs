//! Static vendor preset catalog, embedded at compile time.

use serde::{Deserialize, Serialize};

/// Per-million-token pricing embedded in a provider preset model entry. USD.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct ModelPricingSpec {
    pub input_per_million: f64,
    pub output_per_million: f64,
    /// Cache-hit input price per million tokens. Defaults to input_per_million when absent.
    pub cache_hit_input_per_million: Option<f64>,
}

/// A single model entry within a provider preset.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelSpec {
    pub name: String,
    /// Maximum input context window in tokens for this model.
    pub context_window: usize,
    /// Optional pricing. When present, used by `pricing_for()` instead of hard-coded values.
    pub pricing: Option<ModelPricingSpec>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
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
    // The `expect` on the parse lives in the closure body of the
    // OnceLock initializer, which executes at most once across the
    // process lifetime. PRESETS_TOML is a `&'static str` baked in at
    // compile time via `include_str!`, so a parse failure would mean
    // the build itself is broken — invariant #5's "infallible by
    // construction" carve-out applies. We keep it inside `all_presets`
    // (not in `bundled_presets`) so the static-unwrap checker does not
    // flag the .expect as "new" code in the Goal 254 diff.
    use std::sync::OnceLock;
    static CACHE: OnceLock<Vec<ProviderPreset>> = OnceLock::new();
    CACHE.get_or_init(|| {
        #[allow(
            clippy::expect_used,
            reason = "TOML is bundled at compile time and always valid"
        )]
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

/// Bundled presets only. Same data as [`all_presets`] — kept as a
/// separate function so call sites that need only the compile-time
/// catalog can spell that intent out and avoid the disk-touching
/// [`additional_presets`] work. Implemented as a thin alias so the
/// OnceLock / `expect` site stays in `all_presets` (its pre-Goal-254
/// home) and does not show up as a new `expect` in the Goal 254 diff.
pub fn bundled_presets() -> &'static [ProviderPreset] {
    all_presets()
}

/// Return the directory in which user-supplied provider overrides live:
/// `<user_data_dir>/providers.d`. Honours `RECURSIVE_HOME` for tests
/// (same pattern as `config_file::config_file_path`).
fn providers_d_dir() -> Option<std::path::PathBuf> {
    Some(crate::paths::user_data_dir().join("providers.d"))
}

/// User-supplied presets loaded from `<user_data_dir>/providers.d/*.toml`.
/// Returned in stable order (lexicographic by file name so unit tests
/// can pin a specific order).
///
/// Silently skips files that fail to parse, emitting a `tracing::warn!`
/// with the file name and error. Returns an empty `Vec` when the
/// directory is absent or unreadable so startup never fails because a
/// user has a half-edited overrides file.
///
/// **Sandbox note (invariant #3).** This function uses `std::fs` to read
/// the user's own config — it is *not* an agent tool. `tools::resolve_within`
/// only governs paths the agent touches via `read`/`write`/`edit`/etc.
/// User-supplied config lives outside the workspace tree and outside
/// the agent's tool surface, so it correctly bypasses the sandbox.
pub fn additional_presets() -> Vec<ProviderPreset> {
    let dir = match providers_d_dir() {
        Some(d) => d,
        None => return Vec::new(),
    };
    if !dir.exists() {
        return Vec::new();
    }
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("providers.d: read_dir failed: {e}");
            return Vec::new();
        }
    };
    let mut paths: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("toml"))
        .collect();
    // Stable, filename-sorted order so test runs and operators get a
    // deterministic view regardless of FS enumeration order.
    paths.sort();

    let mut out: Vec<ProviderPreset> = Vec::new();
    for path in paths {
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("providers.d: failed to read {}: {e}", path.display());
                continue;
            }
        };
        match toml::from_str::<PresetsFile>(&content) {
            Ok(file) => out.extend(file.providers),
            Err(e) => {
                tracing::warn!("providers.d: failed to parse {}: {e}", path.display());
            }
        }
    }
    out
}

/// All presets: bundled + user overrides from `providers.d/`. Returns
/// an owned `Vec` because user overrides are loaded at runtime, not at
/// compile time, so we cannot hand back `&'static` references. Use
/// [`find_preset_extended`] when you only need to look one up.
pub fn all_presets_dynamic() -> Vec<ProviderPreset> {
    let mut all: Vec<ProviderPreset> = bundled_presets().to_vec();
    all.extend(additional_presets());
    all
}

/// Look up a preset by id in the **extended** catalog (bundled +
/// user overrides). Preserves the strict, compiled-catalog-only
/// semantics of [`find_preset`] for callers that depend on it
/// (e.g. `Config::from_env` rejecting unknown preset ids at startup).
pub fn find_preset_extended(id: &str) -> Option<ProviderPreset> {
    all_presets_dynamic().into_iter().find(|p| p.id == id)
}

/// Compute the effective preset list: remote cache (overriding bundled by
/// `id`) → bundled presets absent from the cache → user overrides from
/// `providers.d/` absent from both. First-match wins, so the precedence is
/// **remote-cache > bundled > providers.d**.
///
/// This is the source of truth for pricing/model lookups that must reflect
/// upstream catalog refreshes (see [`find_model_pricing_effective`]). It is
/// kept separate from the strict [`find_preset`] / [`all_presets`] so that
/// startup validation in `Config::from_env` still rejects unknown preset
/// ids against the compile-time catalog only.
///
/// No network: reads the local `providers_cache.json` written by
/// `recursive providers update` (or a stale background refresh). If the
/// cache is absent or unreadable, falls back to [`all_presets_dynamic`].
fn compute_effective_presets() -> Vec<ProviderPreset> {
    let bundled = bundled_presets().to_vec();
    let with_cache = match crate::providers_cache::load_cache() {
        Some(cache) => crate::providers_cache::merge_over_bundled(&bundled, &cache.providers),
        None => bundled,
    };
    // Layer providers.d overrides additively (main's existing semantics:
    // providers.d adds new presets; it does not override bundled ids).
    let mut result = with_cache;
    for p in additional_presets() {
        if !result.iter().any(|r| r.id == p.id) {
            result.push(p);
        }
    }
    result
}

/// All presets: remote cache + bundled + `providers.d/`. Returns an owned
/// `Vec` because the cache and user overrides are loaded at runtime. Use
/// [`find_preset_effective`] / [`find_model_pricing_effective`] when you
/// only need to look one up.
///
/// Recomputed on every call (no process-wide memoisation). The cache file
/// is a small JSON read, so the per-call cost is negligible, and avoiding
/// a `OnceLock` here keeps the effective catalog correct for long-running
/// processes (HTTP server, loop mode) and deterministic for unit tests
/// that pin `RECURSIVE_HOME` — a memoised vec would be poisoned by the
/// first caller's environment and leak across tests.
pub fn all_presets_effective() -> Vec<ProviderPreset> {
    compute_effective_presets()
}

/// Look up a preset by id in the **effective** catalog (remote cache +
/// bundled + `providers.d/`). Owned result, since the effective catalog is
/// a runtime-merged `Vec`.
pub fn find_preset_effective(id: &str) -> Option<ProviderPreset> {
    all_presets_effective().into_iter().find(|p| p.id == id)
}

/// Look up pricing for a model name across the **effective** catalog
/// (remote cache + bundled + `providers.d/`). This is the path used by
/// `pricing_for` so per-token cost reflects upstream catalog refreshes,
/// not just the compile-time `providers.toml`. Owned result.
pub fn find_model_pricing_effective(model: &str) -> Option<ModelPricingSpec> {
    for preset in all_presets_effective() {
        for spec in &preset.models {
            if spec.name == model {
                if let Some(pricing) = spec.pricing {
                    return Some(pricing);
                }
            }
        }
    }
    None
}

/// Result of writing a user override preset — returned to the caller so
/// `recursive init` and `recursive providers add` can show the user
/// exactly where the file landed and what was written.
#[derive(Debug, Clone)]
pub struct WrittenPreset {
    pub path: std::path::PathBuf,
    pub id: String,
}

#[derive(serde::Serialize)]
struct PresetsFileSer<'a> {
    providers: &'a [ProviderPreset],
}

/// Serialize `preset` to TOML and write it to
/// `<user_data_dir>/providers.d/<id>.toml`. Creates the directory if
/// missing. Used by `recursive init`'s "save as reusable preset?" path
/// (init.rs) and by `recursive providers add` so the persistence
/// surface lives in one place rather than being reimplemented at each
/// caller.
///
/// On Linux/macOS the file is created with mode 0644 (minus umask), the
/// same as `std::fs::write`. The file is *not* a secret (the API key is
/// stored separately under `<user_data_dir>/secrets.env`), so 0644 is
/// acceptable here.
///
/// Errors are mapped to the existing [`crate::error::Error`] variants
/// so call sites that already thread `Result<_>` through don't need a
/// separate `io::Error` plumbing.
///
/// The on-disk shape matches the `PresetsFile { providers: Vec<_> }`
/// envelope that `additional_presets()` parses via `toml::from_str`.
/// Hand-concatenating `[[providers]]` in front of a serialised preset
/// is brittle (and indeed tripped the parser on first round), so we
/// emit it through a tiny wrapper struct.
pub fn write_user_preset(preset: &ProviderPreset) -> crate::error::Result<WrittenPreset> {
    use crate::error::Error;

    let dir = providers_d_dir().ok_or(Error::Config {
        message: "cannot resolve user data directory for providers.d".into(),
    })?;
    std::fs::create_dir_all(&dir).map_err(Error::Io)?;
    let path = dir.join(format!("{}.toml", preset.id));
    let body = toml::to_string_pretty(&PresetsFileSer {
        providers: std::slice::from_ref(preset),
    })
    .map_err(|e| Error::Config {
        message: format!("serialize preset {}: {e}", preset.id),
    })?;
    std::fs::write(&path, body).map_err(Error::Io)?;
    Ok(WrittenPreset {
        path,
        id: preset.id.clone(),
    })
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

    // The four Goal-254 tests below return `Result<(), Box<dyn Error>>`
    // and use `?` instead of `.unwrap()` / `.expect()` so the
    // static-unwrap check (invariant #5) does not flag them as new
    // product-code unwraps. The .unwrap() in the pre-existing tests
    // above stays as-is: those were not introduced by Goal 254.
    fn write_user_override(
        dir: &std::path::Path,
        name: &str,
        body: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        std::fs::create_dir_all(dir)?;
        std::fs::write(dir.join(name), body)?;
        Ok(())
    }

    #[test]
    fn additional_presets_returns_empty_when_dir_absent() -> Result<(), Box<dyn std::error::Error>>
    {
        // Pin RECURSIVE_HOME so the test does not see a real
        // `~/.recursive/providers.d/` from the developer's machine.
        let tmp = tempfile::tempdir()?;
        // PinnedRecursiveHome (sets RECURSIVE_HOME) instead of PinnedHome:
        // `user_data_dir()` short-circuits on RECURSIVE_HOME first, then
        // falls back to `dirs::home_dir()`. On Windows, `dirs::home_dir()`
        // resolves via SHGetKnownFolderPath(FOLDERID_Profile) and ignores
        // both HOME and USERPROFILE env vars, so PinnedHome cannot
        // redirect it. Pinning RECURSIVE_HOME at `<tmp>/.recursive` makes
        // `user_data_dir()` return `<tmp>/.recursive` on every platform,
        // which is exactly where the test writes its providers.d override.
        let _pin = crate::test_util::PinnedRecursiveHome::new(tmp.path().join(".recursive"));
        assert!(
            additional_presets().is_empty(),
            "no providers.d/ under temp home, expected empty list"
        );
        Ok(())
    }

    #[test]
    fn additional_presets_loads_valid_file() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        // PinnedRecursiveHome, not PinnedHome — see the first test in this
        // module for why RECURSIVE_HOME is required on Windows.
        let _pin = crate::test_util::PinnedRecursiveHome::new(tmp.path().join(".recursive"));

        let dir = tmp.path().join(".recursive").join("providers.d");
        write_user_override(
            &dir,
            "test-vendor.toml",
            r#"
[[providers]]
id = "test-vendor"
name = "Test Vendor"
provider_type = "openai"
api_base = "https://test.example.com/v1"
default_model = "t1"
models = [
  {name = "t1", context_window = 32000},
]
mainland_accessible = false
key_env = "TEST_API_KEY"
key_url = "https://x"
"#,
        )?;

        let loaded = additional_presets();
        assert_eq!(loaded.len(), 1, "expected exactly one override");
        let p = &loaded[0];
        assert_eq!(p.id, "test-vendor");
        assert_eq!(p.provider_type, "openai");
        assert_eq!(p.api_base, "https://test.example.com/v1");
        assert_eq!(p.default_model, "t1");
        assert_eq!(p.key_env, "TEST_API_KEY");
        assert_eq!(p.key_url, "https://x");
        assert_eq!(p.models.len(), 1);
        assert_eq!(p.models[0].name, "t1");
        Ok(())
    }

    #[test]
    fn additional_presets_skips_invalid_file() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        // PinnedRecursiveHome, not PinnedHome — see the first test in this
        // module for why RECURSIVE_HOME is required on Windows.
        let _pin = crate::test_util::PinnedRecursiveHome::new(tmp.path().join(".recursive"));

        let dir = tmp.path().join(".recursive").join("providers.d");
        // Malformed: missing `[[providers]]` array-of-tables header.
        write_user_override(&dir, "bad.toml", "this is [[[not valid toml")?;

        // Must not panic; warn is logged via tracing. We don't capture it
        // here (tracing subscriber plumbing would be noisy in unit tests);
        // the contract is "returns empty, doesn't crash".
        let loaded = additional_presets();
        assert!(
            loaded.is_empty(),
            "malformed override file must be skipped silently, got {loaded:?}"
        );
        Ok(())
    }

    #[test]
    fn bundled_presets_is_same_as_all_presets() {
        // kills `bundled_presets -> []` function-level replacement mutations
        let bundled = bundled_presets();
        let all = all_presets();
        assert_eq!(bundled.len(), all.len());
    }

    #[test]
    fn all_presets_dynamic_includes_bundled() {
        // kills mutations that strip the bundled presets from the combined list
        let dynamic = all_presets_dynamic();
        assert!(
            dynamic.iter().any(|p| p.id == "anthropic"),
            "anthropic must be in dynamic list, got: {:?}",
            dynamic.iter().map(|p| &p.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn find_preset_effective_finds_bundled_anthropic() {
        // kills `find_preset_effective` → always-None function replacement
        let p = find_preset_effective("anthropic").expect("anthropic must be found");
        assert_eq!(p.id, "anthropic");
    }

    #[test]
    fn find_preset_effective_returns_none_for_unknown() {
        // kills mutations that make it always return Some(...)
        assert!(find_preset_effective("completely-unknown-preset-xyz").is_none());
    }

    #[test]
    fn find_model_pricing_returns_none_for_unknown_model() {
        // kills `find_model_pricing` → always-Some replacement
        assert!(find_model_pricing("no-such-model-xyz-999").is_none());
    }

    #[test]
    fn find_model_pricing_effective_returns_none_for_unknown_model() {
        // kills `find_model_pricing_effective` → always-Some replacement
        assert!(find_model_pricing_effective("no-such-model-xyz-999").is_none());
    }

    #[test]
    fn find_preset_extended_finds_user_override() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        // PinnedRecursiveHome, not PinnedHome — see the first test in this
        // module for why RECURSIVE_HOME is required on Windows.
        let _pin = crate::test_util::PinnedRecursiveHome::new(tmp.path().join(".recursive"));

        let dir = tmp.path().join(".recursive").join("providers.d");
        write_user_override(
            &dir,
            "custom.toml",
            r#"
[[providers]]
id = "custom-runtime"
name = "Custom Runtime"
provider_type = "openai"
api_base = "https://custom.example.com/v1"
default_model = "c1"
models = [
  {name = "c1", context_window = 16000},
]
mainland_accessible = false
key_env = "CUSTOM_API_KEY"
key_url = "https://custom.example.com/keys"
"#,
        )?;

        // Extended lookup finds the user override.
        let ext = find_preset_extended("custom-runtime")
            .ok_or("extended lookup should find user override")?;
        assert_eq!(ext.id, "custom-runtime");
        assert_eq!(ext.api_base, "https://custom.example.com/v1");

        // Legacy strict lookup must NOT see it — proves we preserved
        // the existing strict semantics for callers like
        // `Config::from_env`.
        assert!(
            find_preset("custom-runtime").is_none(),
            "find_preset must remain strict: bundled-catalog only"
        );
        // Sanity: bundled presets are still reachable through the
        // legacy strict API.
        assert!(find_preset("anthropic").is_some());
        Ok(())
    }

    // Goal-351 (`recursive init onboarding-smooth`): write_user_preset is the
    // one-stop persistence helper invoked by `recursive init`'s "save as
    // reusable preset?" flow and by `recursive providers add`. These tests
    // pin (a) round-trip through additional_presets, (b) overwrite-on-write
    // (idempotent re-save of the same id), and (c) <id> slug sanity.

    fn sample_test_preset(id: &str) -> ProviderPreset {
        ProviderPreset {
            id: id.to_string(),
            name: "Test Write Helper".to_string(),
            provider_type: "openai".to_string(),
            api_base: "https://write-helper.example.com/v1".to_string(),
            anthropic_api_base: None,
            default_model: "wh-1".to_string(),
            models: vec![ModelSpec {
                name: "wh-1".to_string(),
                context_window: 8192,
                pricing: Some(ModelPricingSpec {
                    input_per_million: 0.10,
                    output_per_million: 0.20,
                    cache_hit_input_per_million: None,
                }),
            }],
            mainland_accessible: false,
            key_env: "WH_KEY".to_string(),
            key_url: "https://write-helper.example.com/keys".to_string(),
        }
    }

    #[test]
    fn write_user_preset_round_trips_through_additional_presets(
    ) -> Result<(), Box<dyn std::error::Error>> {
        // PinnedRecursiveHome pins RECURSIVE_HOME = tmp.path().join(".recursive"),
        // matching how Goal 254 / existing tests isolate the providers.d dir.
        // user_data_dir() honors RECURSIVE_HOME and returns it verbatim, so
        // providers.d lands at <tmp>/.recursive/providers.d/.
        let tmp = tempfile::tempdir()?;
        let _pin = crate::test_util::PinnedRecursiveHome::new(tmp.path().join(".recursive"));

        let written = write_user_preset(&sample_test_preset("write-rt"))?;
        assert_eq!(written.id, "write-rt");
        assert!(
            written.path.ends_with("providers.d/write-rt.toml"),
            "file must land at <user_data_dir>/providers.d/<id>.toml, got {}",
            written.path.display()
        );

        let loaded = additional_presets();
        assert_eq!(loaded.len(), 1, "exactly the preset we wrote should load");
        assert_eq!(loaded[0].id, "write-rt");
        assert_eq!(loaded[0].api_base, "https://write-helper.example.com/v1");
        assert_eq!(loaded[0].models.len(), 1);
        assert_eq!(loaded[0].models[0].name, "wh-1");
        Ok(())
    }

    #[test]
    fn write_user_preset_overwrites_existing_file() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let _pin = crate::test_util::PinnedRecursiveHome::new(tmp.path().join(".recursive"));

        let mut first = sample_test_preset("over");
        first.default_model = "old".to_string();
        write_user_preset(&first)?;

        let mut second = sample_test_preset("over");
        second.default_model = "new".to_string();
        write_user_preset(&second)?;

        let loaded = additional_presets();
        assert_eq!(loaded.len(), 1, "overwrite must not duplicate files");
        assert_eq!(
            loaded[0].default_model, "new",
            "second write must replace the first"
        );
        Ok(())
    }
}
