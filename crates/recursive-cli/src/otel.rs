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
        // but without an active pipeline this is a no-op.
        let _guard = OtelGuard;
    }
}
