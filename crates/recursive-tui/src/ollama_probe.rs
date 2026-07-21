//! Probe the local Ollama instance for its real installed models, so the
//! `/model` picker lists what's actually on disk instead of the static
//! `providers.toml` placeholder list.
//!
//! # Network policy
//!
//! The only network touch is a blocking TCP connect to `127.0.0.1:11434`
//! (with a short timeout) when the cached probe is stale. No external hosts
//! are contacted. The probe is localhost-only by design; users running
//! Ollama on another host should configure a `providers.d` preset.
//!
//! # Test seam
//!
//! [`ollama_models_for_picker`] consults a `#[cfg(test)]` override before
//! the env var / network path, so unit tests can pin the picker behaviour
//! without a live Ollama. See [`set_probe_override_for_test`].

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use recursive::providers::{find_preset, ModelSpec};

/// Default Ollama listen address. Probed as-is (no DNS) so a down Ollama
/// fails fast with ECONNREFUSED instead of waiting on a timeout.
const OLLAMA_ADDR_V4: &str = "127.0.0.1:11434";
/// Fallback for installs that bind IPv6-only (`::1`).
const OLLAMA_ADDR_V6: &str = "[::1]:11434";
/// Hard cap on a single probe attempt (connect + read). Localhost responds
/// in <10ms when up; the timeout only matters if Ollama is hung mid-request.
const PROBE_TIMEOUT: Duration = Duration::from_millis(300);
/// How long a cached probe stays fresh before we re-probe.
const PROBE_TTL: Duration = Duration::from_secs(30);

/// Env var to disable probing and fall back to the bundled list. Values
/// `off` / `0` / `false` (case-insensitive) disable; anything else probes.
pub const PROBE_ENV: &str = "RECURSIVE_TUI_OLLAMA_PROBE";

/// Fallback context window for a probed model whose name isn't in the
/// bundled preset. Ollama doesn't report max context via `/api/tags`, so
/// this is a display hint only — it does not cap real requests.
const DEFAULT_CONTEXT_WINDOW: usize = 32_768;

/// What the picker should show for the `ollama` preset.
#[derive(Clone, Debug)]
pub enum OllamaPickerModels {
    /// Ollama not reachable → hide the preset (unless it's the active one).
    Unreachable,
    /// Ollama is up; list these (possibly empty) real local models.
    Local(Vec<ModelSpec>),
    /// Probing disabled via env → use the bundled `providers.toml` list.
    Bundled,
}

struct Cache {
    at: Instant,
    models: OllamaPickerModels,
}

// Thread-local cache. The picker only runs on the TUI's single event-loop
// thread, so per-thread storage is semantically identical to a global in
// production — and it keeps parallel tests deterministic (each test thread
// owns its own cache, so injected probes can't cross-contaminate).
thread_local! {
    static CACHE: std::cell::RefCell<Option<Cache>> = const { std::cell::RefCell::new(None) };
}

// Thread-local so parallel tests in the same binary don't clobber each
// other's pinned probe result. Each `cargo test` thread owns its own cell
// for the whole test lifetime (setup → body → guard drop).
#[cfg(test)]
thread_local! {
    static TEST_OVERRIDE: std::cell::RefCell<Option<OllamaPickerModels>> =
        const { std::cell::RefCell::new(None) };
}

/// Install a forced probe result for the duration of a test. Pass `None`
/// to clear it and resume the env/network path. Test-only: production
/// code never touches this, so the picker's behaviour in a real TUI run
/// is unaffected. Thread-local to stay isolated under parallel tests.
#[cfg(test)]
pub fn set_probe_override_for_test(models: Option<OllamaPickerModels>) {
    TEST_OVERRIDE.with(|c| *c.borrow_mut() = models);
}

// Thread-local override for the *network* step, so the cache + TTL logic
// (which wraps the probe) is unit-testable without a live socket. When
// unset, the real [`probe_ollama`] runs.
#[cfg(test)]
thread_local! {
    static PROBE_FN_OVERRIDE: std::cell::RefCell<Option<fn() -> OllamaPickerModels>> =
        const { std::cell::RefCell::new(None) };
}

/// Install a fake probe function for the duration of a test, so the cache
/// hit / TTL / invalidation paths can be asserted on deterministically.
/// Pass `None` to restore the real network probe.
#[cfg(test)]
pub fn set_probe_fn_for_test(f: Option<fn() -> OllamaPickerModels>) {
    PROBE_FN_OVERRIDE.with(|c| *c.borrow_mut() = f);
}

// Thread-local override for the env-var opt-out check. `std::env::var` is
// process-global, so a test that toggles `RECURSIVE_TUI_OLLAMA_PROBE` races
// with any other test reading it. This override lets a cache-path test pin
// "probe enabled" regardless of what the global env says.
#[cfg(test)]
thread_local! {
    static ENV_DISABLED_OVERRIDE: std::cell::RefCell<Option<bool>> =
        const { std::cell::RefCell::new(None) };
}

/// Force the env-var opt-out check result for a test. Pass `None` to
/// restore real `std::env::var` reads.
#[cfg(test)]
pub fn set_env_disabled_for_test(v: Option<bool>) {
    ENV_DISABLED_OVERRIDE.with(|c| *c.borrow_mut() = v);
}

/// Run the probe step. In production this is the real localhost probe; in
/// tests a thread-local fake can stand in so the cache wrapper is exercised
/// without touching the network.
fn run_probe() -> OllamaPickerModels {
    #[cfg(test)]
    if let Some(f) = PROBE_FN_OVERRIDE.with(|c| *c.borrow()) {
        return f();
    }
    probe_ollama()
}

fn env_probe_disabled() -> bool {
    #[cfg(test)]
    if let Some(v) = ENV_DISABLED_OVERRIDE.with(|c| *c.borrow()) {
        return v;
    }
    std::env::var(PROBE_ENV)
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "off" | "0" | "false"))
        .unwrap_or(false)
}

/// Resolve the models the `/model` picker should show for the `ollama`
/// preset. Consults (in order) the test override, the env-var opt-out,
/// then a TTL-cached localhost probe.
pub fn ollama_models_for_picker() -> OllamaPickerModels {
    #[cfg(test)]
    if let Some(forced) = TEST_OVERRIDE.with(|c| c.borrow().clone()) {
        return forced;
    }

    if env_probe_disabled() {
        return OllamaPickerModels::Bundled;
    }

    CACHE.with(|c| {
        if let Some(guard) = c.borrow().as_ref() {
            if guard.at.elapsed() < PROBE_TTL {
                return guard.models.clone();
            }
        }
        let models = run_probe();
        *c.borrow_mut() = Some(Cache {
            at: Instant::now(),
            models: models.clone(),
        });
        models
    })
}

/// Force a re-probe on the next call regardless of TTL. Used by tests that
/// need to observe a fresh probe after mutating env state.
pub fn invalidate_cache() {
    CACHE.with(|c| *c.borrow_mut() = None);
}

fn probe_ollama() -> OllamaPickerModels {
    let raw = match probe_once(OLLAMA_ADDR_V4).or_else(|| probe_once(OLLAMA_ADDR_V6)) {
        Some(raw) => raw,
        None => return OllamaPickerModels::Unreachable,
    };
    let body = match http_body(&raw) {
        Some(body) => body,
        None => return OllamaPickerModels::Unreachable,
    };
    OllamaPickerModels::Local(parse_tags(body))
}

fn probe_once(addr: &str) -> Option<Vec<u8>> {
    let socket: SocketAddr = addr.parse().ok()?;
    let mut stream = TcpStream::connect_timeout(&socket, PROBE_TIMEOUT).ok()?;
    stream.set_read_timeout(Some(PROBE_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(PROBE_TIMEOUT)).ok()?;
    let req = b"GET /api/tags HTTP/1.0\r\nHost: localhost\r\nAccept: application/json\r\nConnection: close\r\n\r\n";
    stream.write_all(req).ok()?;
    let mut buf = Vec::with_capacity(4096);
    stream.read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// Split a raw HTTP response into its body. Returns the bytes after the
/// first `\r\n\r\n` separator, or `None` if no separator is present.
fn http_body(raw: &[u8]) -> Option<&[u8]> {
    let sep = b"\r\n\r\n";
    let idx = raw.windows(sep.len()).position(|w| w == sep)?;
    Some(&raw[idx + sep.len()..])
}

/// Parse an Ollama `/api/tags` body into a sorted, de-duplicated
/// `Vec<ModelSpec>`. Tolerates either the `name` or `model` field per
/// entry (Ollama has used both across versions). Context windows are
/// looked up from the bundled `ollama` preset by base name (tag stripped)
/// so a known model keeps its bundled hint; unknown models fall back to
/// [`DEFAULT_CONTEXT_WINDOW`].
pub fn parse_tags(body: &[u8]) -> Vec<ModelSpec> {
    let value: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut names: Vec<String> = Vec::new();
    if let Some(arr) = value.get("models").and_then(|m| m.as_array()) {
        for entry in arr {
            let name = entry
                .get("name")
                .and_then(|n| n.as_str())
                .or_else(|| entry.get("model").and_then(|n| n.as_str()));
            if let Some(n) = name {
                if !n.is_empty() {
                    names.push(n.to_string());
                }
            }
        }
    }
    names.sort();
    names.dedup();

    let bundled = find_preset("ollama").map(|p| p.models.clone());
    names
        .into_iter()
        .map(|name| ModelSpec {
            context_window: bundled
                .as_ref()
                .and_then(|ms| {
                    ms.iter()
                        .find(|m| m.name == base_name(&name))
                        .map(|m| m.context_window)
                })
                .unwrap_or(DEFAULT_CONTEXT_WINDOW),
            name,
            pricing: None,
        })
        .collect()
}

/// Strip the `:tag` suffix Ollama appends (e.g. `llama3.2:latest` →
/// `llama3.2`) so the name can be matched against the bundled preset.
fn base_name(full: &str) -> &str {
    full.split(':').next().unwrap_or(full)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn override_guard(models: Option<OllamaPickerModels>) -> impl Drop {
        set_probe_override_for_test(models);
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                set_probe_override_for_test(None);
            }
        }
        Reset
    }

    #[test]
    fn parse_tags_extracts_names_from_name_field() {
        let body = br#"{"models":[{"name":"llama3.2:latest"},{"name":"qwen2.5-coder:latest"}]}"#;
        let models = parse_tags(body);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].name, "llama3.2:latest");
        assert_eq!(models[1].name, "qwen2.5-coder:latest");
    }

    #[test]
    fn parse_tags_falls_back_to_model_field() {
        let body = br#"{"models":[{"model":"mistral:7b"}]}"#;
        let models = parse_tags(body);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].name, "mistral:7b");
    }

    #[test]
    fn parse_tags_dedupes_and_sorts() {
        let body = br#"{"models":[{"name":"b:latest"},{"name":"a:latest"},{"name":"b:latest"}]}"#;
        let models = parse_tags(body);
        assert_eq!(
            models.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            vec!["a:latest", "b:latest"]
        );
    }

    #[test]
    fn parse_tags_reuses_bundled_context_window_for_known_model() {
        let body = br#"{"models":[{"name":"llama3.2:latest"}]}"#;
        let models = parse_tags(body);
        let bundled = find_preset("ollama")
            .expect("bundled ollama preset")
            .models
            .iter()
            .find(|m| m.name == "llama3.2")
            .expect("bundled llama3.2");
        assert_eq!(models[0].context_window, bundled.context_window);
    }

    #[test]
    fn parse_tags_uses_default_context_window_for_unknown_model() {
        let body = br#"{"models":[{"name":"totally-custom:latest"}]}"#;
        let models = parse_tags(body);
        assert_eq!(models[0].context_window, DEFAULT_CONTEXT_WINDOW);
    }

    #[test]
    fn parse_tags_empty_models_array_yields_empty_vec() {
        let body = br#"{"models":[]}"#;
        assert!(parse_tags(body).is_empty());
    }

    #[test]
    fn parse_tags_malformed_json_yields_empty_vec() {
        assert!(parse_tags(b"not json").is_empty());
        assert!(parse_tags(b"{}").is_empty());
    }

    #[test]
    fn http_body_splits_headers_and_body() {
        let raw = b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\n{}";
        assert_eq!(http_body(raw), Some(b"{}".as_slice()));
    }

    #[test]
    fn http_body_returns_none_without_separator() {
        assert_eq!(http_body(b"no separator here"), None);
    }

    #[test]
    fn base_name_strips_tag_suffix() {
        assert_eq!(base_name("llama3.2:latest"), "llama3.2");
        assert_eq!(base_name("qwen2.5-coder"), "qwen2.5-coder");
    }

    #[test]
    fn env_probe_disabled_recognises_off_variants() {
        let cases = [
            ("off", true),
            ("OFF", true),
            ("0", true),
            ("false", true),
            ("on", false),
            ("1", false),
            ("yes", false),
        ];
        for (val, expected) in cases {
            std::env::set_var(PROBE_ENV, val);
            assert_eq!(env_probe_disabled(), expected, "env={val}");
        }
        std::env::remove_var(PROBE_ENV);
        assert!(!env_probe_disabled());
    }

    #[test]
    fn picker_returns_test_override_when_set() {
        let _g = override_guard(Some(OllamaPickerModels::Unreachable));
        invalidate_cache();
        assert!(matches!(
            ollama_models_for_picker(),
            OllamaPickerModels::Unreachable
        ));
    }

    #[test]
    fn picker_returns_bundled_when_env_disables_probe() {
        // Use the thread-local env override rather than std::env::set_var so
        // this can't race with `env_probe_disabled_recognises_off_variants`
        // (which toggles the same process-global env var).
        let _g = override_guard(None);
        set_env_disabled_for_test(Some(true));
        invalidate_cache();
        assert!(matches!(
            ollama_models_for_picker(),
            OllamaPickerModels::Bundled
        ));
        set_env_disabled_for_test(None);
    }

    // ── cache + TTL + invalidation (probe fn injected) ───────────────

    thread_local! {
        static PROBE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    fn counting_probe_returning_local() -> OllamaPickerModels {
        PROBE_CALLS.with(|c| c.set(c.get() + 1));
        OllamaPickerModels::Local(vec![ModelSpec {
            name: "cached-model:latest".into(),
            context_window: DEFAULT_CONTEXT_WINDOW,
            pricing: None,
        }])
    }

    /// Pin the picker to the cache path (no override, no env opt-out) with
    /// an injected probe, so the cache hit / TTL logic is exercised without
    /// a live socket.
    fn cache_path_guard() -> impl Drop {
        set_probe_override_for_test(None);
        set_probe_fn_for_test(Some(counting_probe_returning_local));
        set_env_disabled_for_test(Some(false));
        PROBE_CALLS.with(|c| c.set(0));
        invalidate_cache();
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                set_probe_override_for_test(None);
                set_probe_fn_for_test(None);
                set_env_disabled_for_test(None);
                PROBE_CALLS.with(|c| c.set(0));
                invalidate_cache();
            }
        }
        Reset
    }

    #[test]
    fn cache_serves_repeated_calls_within_ttl() {
        // First call probes (count=1); the second call within the TTL must
        // hit the cache and NOT re-probe (count stays 1). Catches the
        // TTL `<` → `==`/`>` mutants and the `cache_cell` Box::leak mutant,
        // all of which would re-probe on the second call.
        let _g = cache_path_guard();
        let first = ollama_models_for_picker();
        let calls_after_first = PROBE_CALLS.with(|c| c.get());
        assert_eq!(calls_after_first, 1, "first call should probe once");

        let second = ollama_models_for_picker();
        assert_eq!(
            PROBE_CALLS.with(|c| c.get()),
            1,
            "second call within TTL should hit the cache, not re-probe"
        );
        assert!(
            matches!(second, OllamaPickerModels::Local(ref m) if m.first().is_some_and(|m| m.name == "cached-model:latest")),
            "cached call should return the probed model: {second:?}"
        );
        let _ = first;
    }

    #[test]
    fn invalidate_cache_forces_reprobe() {
        // After invalidate_cache(), the next call must re-probe (count=2),
        // not return the stale cached value. Catches the
        // `invalidate_cache -> ()` mutant.
        let _g = cache_path_guard();
        let _ = ollama_models_for_picker();
        assert_eq!(PROBE_CALLS.with(|c| c.get()), 1);

        invalidate_cache();
        let _ = ollama_models_for_picker();
        assert_eq!(
            PROBE_CALLS.with(|c| c.get()),
            2,
            "invalidate_cache must force a re-probe on the next call"
        );
    }
}
