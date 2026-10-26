//! OpenTelemetry tracing exporter for the `recursive` CLI.
//!
//! ## Env vars
//!
//! | Var | Required | Default | Description |
//! |-----|----------|---------|-------------|
//! | `RECURSIVE_OTEL_ENDPOINT` | No (see note) | — | OTLP gRPC endpoint, e.g. `http://localhost:4317`. When set and non-empty, the OTEL exporter is activated (requires `--features otel` at compile time). |
//! | `RECURSIVE_OTEL_SERVICE_NAME` | No | `recursive` | `service.name` resource attribute sent with every span. |
//!
//! **Note**: The exporter is only initialised when both `--features otel` is
//! enabled at compile time **and** `RECURSIVE_OTEL_ENDPOINT` is set at
//! runtime. Without the endpoint, no network connection is attempted.

use opentelemetry::trace::TracerProvider as _;
use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::trace::TracerProvider;
use opentelemetry_sdk::Resource;

/// Read the OTLP endpoint from `RECURSIVE_OTEL_ENDPOINT`.
///
/// Returns `None` when the var is unset or empty, so callers can
/// short-circuit without attempting a network connect.
pub fn otel_endpoint() -> Option<String> {
    let v = std::env::var("RECURSIVE_OTEL_ENDPOINT").ok()?;
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// Read the service name from `RECURSIVE_OTEL_SERVICE_NAME`, defaulting
/// to `"recursive"`.
pub fn otel_service_name() -> String {
    std::env::var("RECURSIVE_OTEL_SERVICE_NAME")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "recursive".to_string())
}

// ---------------------------------------------------------------------------
// Pure helpers taking explicit values (testable without env mutation)
// ---------------------------------------------------------------------------

/// Returns `Some(endpoint)` when the endpoint is non-empty, `None` when
/// unset or empty. Pure version for testing.
#[allow(dead_code)]
pub fn otel_endpoint_from_str(endpoint: Option<&str>) -> Option<String> {
    let s = endpoint?;
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Returns the service name from an explicit value, defaulting to
/// `"recursive"`. Pure version for testing.
#[allow(dead_code)]
pub fn otel_service_name_from_str(name: Option<&str>) -> String {
    name.filter(|v| !v.is_empty())
        .unwrap_or("recursive")
        .to_string()
}

// ---------------------------------------------------------------------------
// Layer construction
// ---------------------------------------------------------------------------

/// RAII guard that shuts down the global tracer provider on drop,
/// flushing any pending spans to the collector.
///
/// Hold this for the lifetime of `main`. The drop runs inside the
/// tokio runtime (before `#[tokio::main]` shuts it down), so the
/// batch span processor can flush its queue.
pub struct OtelGuard;

impl Drop for OtelGuard {
    fn drop(&mut self) {
        opentelemetry::global::shutdown_tracer_provider();
    }
}

/// Initialise the OTLP tracer provider and return the SDK tracer
/// alongside a guard that flushes on drop.
///
/// After calling this, the global tracer provider is also set for
/// any code that uses `opentelemetry::global::tracer()`.
///
/// # Errors
///
/// Returns an error if the OTLP pipeline cannot be constructed (e.g.
/// invalid endpoint or tonic connection failure).
pub fn init_global_tracer() -> anyhow::Result<(opentelemetry_sdk::trace::Tracer, OtelGuard)> {
    let endpoint =
        otel_endpoint().ok_or_else(|| anyhow::anyhow!("RECURSIVE_OTEL_ENDPOINT not set"))?;
    let service_name = otel_service_name();
    let service_version = env!("CARGO_PKG_VERSION");

    let resource = Resource::new(vec![
        KeyValue::new("service.name", service_name),
        KeyValue::new("service.version", service_version.to_string()),
    ]);

    let tracer_provider = TracerProvider::builder()
        .with_batch_exporter(
            opentelemetry_otlp::SpanExporter::builder()
                .with_tonic()
                .with_endpoint(&endpoint)
                .build()?,
            opentelemetry_sdk::runtime::Tokio,
        )
        .with_resource(resource)
        .build();

    let tracer = tracer_provider.tracer("recursive-cli");
    opentelemetry::global::set_tracer_provider(tracer_provider);

    Ok((tracer, OtelGuard))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::EnvGuard;

    /// The global tracer provider is process-wide state: every test that
    /// installs or shuts it down holds this lock so a sibling's shutdown
    /// cannot land between another test's install and its observation.
    static GLOBAL_TRACER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_global_tracer() -> std::sync::MutexGuard<'static, ()> {
        GLOBAL_TRACER_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ── env shells (issue #245 batch 2) ───────────────────────────────────
    //
    // `otel_endpoint` / `otel_service_name` are the env-reading shells over the
    // pure `_from_str` helpers below. The cli-mutants gate compiles this module
    // out (`--features weixin` only; `otel` is opt-in), so these pins only run
    // under `--features otel` — see the matching exclusions in
    // `.cargo/mutants.toml`.

    /// Unset and empty both mean "no exporter" (`None`); a set value comes
    /// back verbatim, not as a placeholder.
    #[test]
    fn otel_endpoint_env_shell_reads_unset_empty_and_set() {
        let mut env = EnvGuard::lock();
        env.remove_var("RECURSIVE_OTEL_ENDPOINT");
        assert_eq!(otel_endpoint(), None, "unset must short-circuit to None");
        env.set_var("RECURSIVE_OTEL_ENDPOINT", "");
        assert_eq!(
            otel_endpoint(),
            None,
            "an empty endpoint must not attempt a connect"
        );
        env.set_var("RECURSIVE_OTEL_ENDPOINT", "http://127.0.0.1:4317");
        assert_eq!(
            otel_endpoint().as_deref(),
            Some("http://127.0.0.1:4317"),
            "a set endpoint must be returned verbatim"
        );
    }

    /// Unset and empty both fall back to the documented default; a set value
    /// overrides it.
    #[test]
    fn otel_service_name_env_shell_defaults_and_overrides() {
        let mut env = EnvGuard::lock();
        env.remove_var("RECURSIVE_OTEL_SERVICE_NAME");
        assert_eq!(otel_service_name(), "recursive", "unset → default");
        env.set_var("RECURSIVE_OTEL_SERVICE_NAME", "");
        assert_eq!(
            otel_service_name(),
            "recursive",
            "an empty name must fall back to the default, not become \"\""
        );
        env.set_var("RECURSIVE_OTEL_SERVICE_NAME", "my-agent");
        assert_eq!(otel_service_name(), "my-agent", "a set name must win");
    }

    /// Without an endpoint the pipeline is never built: the error names the
    /// variable the operator has to set.
    #[test]
    fn init_global_tracer_refuses_without_an_endpoint() {
        let mut env = EnvGuard::lock();
        env.remove_var("RECURSIVE_OTEL_ENDPOINT");
        let _global = lock_global_tracer();
        let err = init_global_tracer()
            .err()
            .expect("no endpoint must be an error, not a silently inert tracer");
        assert!(
            err.to_string().contains("RECURSIVE_OTEL_ENDPOINT"),
            "the error must name the missing variable: {err}"
        );
    }

    /// Dropping the guard shuts the *global* provider down: a tracer fetched
    /// afterwards is the no-op one (its spans carry an invalid context),
    /// whereas one fetched before the drop came from the installed SDK
    /// provider. A guard whose `drop` does nothing leaves the SDK provider
    /// installed and the second span valid.
    #[test]
    fn otel_guard_drop_shuts_down_the_global_provider() {
        use opentelemetry::trace::{Span, Tracer};
        let _global = lock_global_tracer();
        opentelemetry::global::set_tracer_provider(TracerProvider::builder().build());
        let before = opentelemetry::global::tracer("otel-guard-probe").start("probe");
        assert!(
            before.span_context().is_valid(),
            "fixture: the SDK provider must be the global one before the drop"
        );

        drop(OtelGuard);

        let after = opentelemetry::global::tracer("otel-guard-probe").start("probe");
        assert!(
            !after.span_context().is_valid(),
            "dropping the guard must replace the global provider with the no-op one"
        );
    }

    #[test]
    fn otel_endpoint_unset_is_none() {
        assert_eq!(otel_endpoint_from_str(None), None);
    }

    #[test]
    fn otel_endpoint_empty_is_none() {
        assert_eq!(otel_endpoint_from_str(Some("")), None);
    }

    #[test]
    fn otel_endpoint_set_returns_value() {
        assert_eq!(
            otel_endpoint_from_str(Some("http://localhost:4317")),
            Some("http://localhost:4317".to_string())
        );
    }

    #[test]
    fn otel_service_name_default() {
        assert_eq!(otel_service_name_from_str(None), "recursive");
    }

    #[test]
    fn otel_service_name_empty_defaults() {
        assert_eq!(otel_service_name_from_str(Some("")), "recursive");
    }

    #[test]
    fn otel_service_name_override() {
        assert_eq!(otel_service_name_from_str(Some("my-agent")), "my-agent");
    }

    #[test]
    fn otel_guard_drop_does_not_panic() {
        // Just verify the guard can be created and dropped without panic.
        // In a real scenario it would shut down the global tracer provider,
        // but without an active pipeline this is a no-op. The drop still
        // swaps the global provider, so it serialises with the probe above.
        let _global = lock_global_tracer();
        let _guard = OtelGuard;
    }
}
