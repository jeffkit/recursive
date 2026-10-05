//! Langfuse / OTLP exporter configuration resolved from the environment.

use base64::Engine as _;

/// Resolved exporter configuration.
///
/// Built by [`langfuse_config_from_env`]; the pure [`config_from`] form takes
/// an explicit env getter so it is testable without touching the process
/// environment.
#[derive(Debug, Clone, PartialEq)]
pub struct LangfuseConfig {
    /// Fully-qualified OTLP HTTP traces endpoint to POST to.
    pub endpoint: String,
    /// Extra HTTP headers (e.g. the Langfuse `Authorization: Basic …`).
    pub headers: Vec<(String, String)>,
    /// `service.name` resource attribute.
    pub service_name: String,
    /// Fraction of runs to export, `0.0..=1.0`.
    pub sample_ratio: f64,
    /// When `true`, payloads are reported as length+hash instead of text.
    pub redact: bool,
}

const DEFAULT_HOST: &str = "https://cloud.langfuse.com";
const OTEL_TRACES_PATH: &str = "/api/public/otel/v1/traces";
const DEFAULT_SERVICE_NAME: &str = "recursive";

/// Read the exporter configuration from the process environment.
///
/// Returns `None` when neither a Langfuse key pair nor a generic OTLP
/// endpoint is configured, so hosts can short-circuit without any network
/// setup.
pub fn langfuse_config_from_env() -> Option<LangfuseConfig> {
    config_from(|k| std::env::var(k).ok())
}

/// Pure configuration resolver.
pub fn config_from(get: impl Fn(&str) -> Option<String>) -> Option<LangfuseConfig> {
    let public = non_empty(get("LANGFUSE_PUBLIC_KEY"));
    let secret = non_empty(get("LANGFUSE_SECRET_KEY"));
    let explicit = non_empty(get("LANGFUSE_OTEL_ENDPOINT"));
    // Distinct from the CLI's `RECURSIVE_OTEL_ENDPOINT` (an OTLP gRPC base
    // endpoint for the pre-existing tracing exporter): this one is a full
    // HTTP/protobuf traces URL, used verbatim.
    let generic = non_empty(get("RECURSIVE_OTEL_TRACES_URL"));

    let mut headers = Vec::new();
    let endpoint = match (public.as_deref(), secret.as_deref()) {
        (Some(pk), Some(sk)) => {
            headers.push(("Authorization".to_string(), basic_auth(pk, sk)));
            explicit.unwrap_or_else(|| {
                format!(
                    "{}{OTEL_TRACES_PATH}",
                    normalise_host(non_empty(get("LANGFUSE_HOST")).as_deref())
                )
            })
        }
        _ => explicit.or(generic)?,
    };

    let service_name = non_empty(get("LANGFUSE_SERVICE_NAME"))
        .or_else(|| non_empty(get("RECURSIVE_OTEL_SERVICE_NAME")))
        .unwrap_or_else(|| DEFAULT_SERVICE_NAME.to_string());

    Some(LangfuseConfig {
        endpoint,
        headers,
        service_name,
        sample_ratio: parse_ratio(
            get("LANGFUSE_SAMPLE_RATIO")
                .or_else(|| get("RECURSIVE_OTEL_SAMPLE_RATIO"))
                .as_deref(),
        ),
        redact: parse_redact(get("LANGFUSE_REDACT").as_deref()),
    })
}

/// `Authorization: Basic <base64(public:secret)>`, per the Langfuse API.
pub fn basic_auth(public_key: &str, secret_key: &str) -> String {
    let token =
        base64::engine::general_purpose::STANDARD.encode(format!("{public_key}:{secret_key}"));
    format!("Basic {token}")
}

/// Trim a configured host and drop a trailing slash so path joins are clean.
pub fn normalise_host(host: Option<&str>) -> String {
    let host = host
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .unwrap_or(DEFAULT_HOST);
    host.trim_end_matches('/').to_string()
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty())
}

/// Parse a sampling ratio, clamping to `0.0..=1.0` and defaulting to `1.0`.
pub fn parse_ratio(raw: Option<&str>) -> f64 {
    match raw.and_then(|v| v.trim().parse::<f64>().ok()) {
        Some(r) if r.is_finite() => r.clamp(0.0, 1.0),
        _ => 1.0,
    }
}

/// Parse the redaction switch; default `true` (least-privilege).
pub fn parse_redact(raw: Option<&str>) -> bool {
    !matches!(raw.map(str::trim), Some("0") | Some("false") | Some("no"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn getter(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + 'static {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn no_env_is_disabled() {
        assert!(config_from(|_| None).is_none());
    }

    #[test]
    fn langfuse_keys_build_cloud_endpoint_and_header() {
        let cfg = config_from(getter(&[
            ("LANGFUSE_PUBLIC_KEY", "pk-lf-abc"),
            ("LANGFUSE_SECRET_KEY", "sk-lf-def"),
        ]))
        .expect("keys enable the exporter");
        assert_eq!(
            cfg.endpoint,
            "https://cloud.langfuse.com/api/public/otel/v1/traces"
        );
        assert_eq!(cfg.headers.len(), 1);
        assert_eq!(cfg.headers[0].0, "Authorization");
        // base64("pk-lf-abc:sk-lf-def")
        assert_eq!(cfg.headers[0].1, basic_auth("pk-lf-abc", "sk-lf-def"));
        assert!(cfg.headers[0].1.starts_with("Basic "));
        assert_eq!(cfg.service_name, "recursive");
        assert_eq!(cfg.sample_ratio, 1.0);
        assert!(cfg.redact);
    }

    #[test]
    fn custom_host_and_service_and_trailing_slash() {
        let cfg = config_from(getter(&[
            ("LANGFUSE_PUBLIC_KEY", "pk"),
            ("LANGFUSE_SECRET_KEY", "sk"),
            ("LANGFUSE_HOST", "https://lf.internal/"),
            ("LANGFUSE_SERVICE_NAME", "recursive-e2e"),
        ]))
        .expect("configured");
        assert_eq!(
            cfg.endpoint,
            "https://lf.internal/api/public/otel/v1/traces"
        );
        assert_eq!(cfg.service_name, "recursive-e2e");
    }

    #[test]
    fn explicit_endpoint_overrides_derivation() {
        let cfg = config_from(getter(&[
            ("LANGFUSE_PUBLIC_KEY", "pk"),
            ("LANGFUSE_SECRET_KEY", "sk"),
            ("LANGFUSE_OTEL_ENDPOINT", "http://collector:4318/v1/traces"),
        ]))
        .expect("configured");
        assert_eq!(cfg.endpoint, "http://collector:4318/v1/traces");
    }

    #[test]
    fn generic_otlp_endpoint_without_keys_has_no_auth() {
        let cfg = config_from(getter(&[(
            "RECURSIVE_OTEL_TRACES_URL",
            "http://localhost:4318/v1/traces",
        )]))
        .expect("configured");
        assert_eq!(cfg.endpoint, "http://localhost:4318/v1/traces");
        assert!(cfg.headers.is_empty());
    }

    #[test]
    fn sample_ratio_and_redact_parsing() {
        assert_eq!(parse_ratio(None), 1.0);
        assert_eq!(parse_ratio(Some("0.25")), 0.25);
        assert_eq!(parse_ratio(Some("2")), 1.0);
        assert_eq!(parse_ratio(Some("-1")), 0.0);
        assert_eq!(parse_ratio(Some("nonsense")), 1.0);
        assert_eq!(parse_ratio(Some("NaN")), 1.0);

        let cfg = config_from(getter(&[
            ("RECURSIVE_OTEL_TRACES_URL", "http://x/v1/traces"),
            ("RECURSIVE_OTEL_SAMPLE_RATIO", "0.5"),
            ("LANGFUSE_REDACT", "0"),
        ]))
        .expect("configured");
        assert_eq!(cfg.sample_ratio, 0.5);
        assert!(!cfg.redact);
    }

    #[test]
    fn redact_defaults_on_and_accepts_off_spellings() {
        assert!(parse_redact(None));
        assert!(parse_redact(Some("1")));
        assert!(!parse_redact(Some("0")));
        assert!(!parse_redact(Some("false")));
        assert!(!parse_redact(Some("no")));
    }

    #[test]
    fn normalise_host_defaults_and_trims() {
        assert_eq!(normalise_host(None), DEFAULT_HOST);
        assert_eq!(normalise_host(Some("")), DEFAULT_HOST);
        assert_eq!(normalise_host(Some("  ")), DEFAULT_HOST);
        assert_eq!(normalise_host(Some("https://a/b/")), "https://a/b");
    }

    #[test]
    fn empty_strings_do_not_enable_the_exporter() {
        assert!(config_from(getter(&[("LANGFUSE_PUBLIC_KEY", "")])).is_none());
        assert!(config_from(getter(&[("RECURSIVE_OTEL_TRACES_URL", "")])).is_none());
    }
}
