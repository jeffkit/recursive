//! OTLP HTTP/protobuf exporter for Langfuse.
//!
//! Thin adapter between the pure [`Observation`] records produced by the
//! [`RunCollector`] and OpenTelemetry spans. All span-shaping decisions live
//! in the collector; this module only turns records into SDK calls and ships
//! them over HTTP.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use opentelemetry::trace::{
    Event as OtelEvent, Span, SpanKind, Status as OtelStatus, TraceContextExt, Tracer,
    TracerProvider as _,
};
use opentelemetry::{Array, Context, KeyValue, Value};
use opentelemetry_otlp::{SpanExporter, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::trace::{Tracer as SdkTracer, TracerProvider};
use opentelemetry_sdk::Resource;

use super::collector::{AttrValue, ObsEvent, ObsKind, ObsStatus, Observation, RunCollector};
use super::config::LangfuseConfig;
use super::RunMeta;
use crate::event::{AgentEvent, EventSink};

/// Export timeout for one OTLP request.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);

/// A live run trace: owns the tracer provider and the accumulating collector.
pub struct ActiveRun {
    tracer: SdkTracer,
    provider: TracerProvider,
    collector: Arc<Mutex<RunCollector>>,
    flushed: AtomicBool,
}

impl ActiveRun {
    /// Build the OTLP pipeline and start a run trace, or return `None` when
    /// the run is dropped by sampling or the exporter cannot be constructed.
    pub fn start(meta: &RunMeta, cfg: &LangfuseConfig) -> Option<Self> {
        if !should_sample(cfg.sample_ratio, sample_draw()) {
            return None;
        }
        // The batch span processor spawns its export task onto the Tokio
        // runtime; fail open instead of panicking when called outside one.
        if tokio::runtime::Handle::try_current().is_err() {
            tracing::warn!("no Tokio runtime; Langfuse tracing disabled");
            return None;
        }

        let headers: HashMap<String, String> = cfg.headers.iter().cloned().collect();
        let exporter = match SpanExporter::builder()
            .with_http()
            .with_endpoint(cfg.endpoint.clone())
            .with_headers(headers)
            .with_timeout(EXPORT_TIMEOUT)
            .build()
        {
            Ok(exporter) => exporter,
            Err(err) => {
                // Fail open, but never swallow the reason: a selection/build
                // problem (e.g. no HTTP client feature) must be visible.
                tracing::warn!(
                    error = %err,
                    "failed to build OTLP span exporter; Langfuse tracing disabled"
                );
                return None;
            }
        };

        let provider = TracerProvider::builder()
            .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
            .with_resource(Resource::new(vec![
                KeyValue::new("service.name", cfg.service_name.clone()),
                KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
            ]))
            .build();
        let tracer = provider.tracer("recursive");

        Some(Self {
            tracer,
            provider,
            collector: Arc::new(Mutex::new(RunCollector::new(
                meta.clone(),
                cfg.redact,
                SystemTime::now(),
            ))),
            flushed: AtomicBool::new(false),
        })
    }

    /// The event sink recording into this run's collector.
    pub fn sink(&self) -> Box<dyn EventSink> {
        Box::new(CollectorSink {
            collector: self.collector.clone(),
        })
    }

    /// Record the terminal state and flush the trace to Langfuse.
    ///
    /// Awaits the export: the OTLP request has left the process by the time
    /// this returns, even when the host is about to shut its runtime down.
    pub async fn finish(&self, finish_reason: Option<&str>, error: Option<&str>) {
        if let Ok(mut collector) = self.collector.lock() {
            collector.finish(finish_reason, error, SystemTime::now());
        }
        self.flush().await;
    }

    /// Queue every collected span into the batch processor. Idempotent —
    /// returns `false` when this run was already queued.
    fn queue(&self) -> bool {
        if self.flushed.swap(true, Ordering::SeqCst) {
            return false;
        }
        let records = match self.collector.lock() {
            Ok(collector) => collector.records(),
            Err(_) => return true,
        };
        export(&self.tracer, &records);
        true
    }

    /// Queue the spans and wait for the batch processor to ship them.
    ///
    /// The processor that performs the OTLP POST is a task on the *ambient*
    /// Tokio runtime (`opentelemetry_sdk::runtime::Tokio`), so this wait has
    /// to finish while that runtime is still alive. A detached
    /// `spawn_blocking` cannot guarantee that: the runtime shutdown drops its
    /// owned tasks — the processor among them, **before** the blocking pool is
    /// joined — so the request is never written (observed as a bare TCP
    /// connect at process exit). Await the handle instead.
    async fn flush(&self) {
        self.queue();
        drain(&self.provider).await;
    }
}

/// Flush the queued spans and stop the processor.
///
/// Both calls block on the processor task, so this must run where waiting is
/// safe: on a blocking thread, or with no runtime at all. The explicit
/// `shutdown` also defuses the SDK's `Drop for TracerProviderInner`, which
/// calls `shutdown` itself — waiting for the processor from an executor
/// thread without yielding would deadlock a current-thread runtime.
fn provider_flush(provider: &TracerProvider) {
    let _ = provider.force_flush();
    let _ = provider.shutdown();
}

/// Await a blocking flush so it completes before the caller returns (and
/// before the runtime the processor runs on can shut down).
async fn drain(provider: &TracerProvider) {
    let provider = provider.clone();
    if tokio::runtime::Handle::try_current().is_ok() {
        let _ = tokio::task::spawn_blocking(move || provider_flush(&provider)).await;
    } else {
        provider_flush(&provider);
    }
}

impl Drop for ActiveRun {
    fn drop(&mut self) {
        if let Ok(mut collector) = self.collector.lock() {
            collector.finish(None, None, SystemTime::now());
        }
        // Safety net for a host that drops the run without calling `finish`:
        // queue the spans and drain the processor off the executor thread.
        // Dropping the provider on this thread would block on the processor
        // task (see `provider_flush`), stalling the async worker — and
        // deadlocking outright on a current-thread runtime. Best-effort by
        // design: the runtime may shut down first, which is exactly why a
        // host that needs the export delivered before exit must await
        // `finish`. After a `finish` there is nothing left to do.
        if !self.queue() {
            return;
        }
        let provider = self.provider.clone();
        if tokio::runtime::Handle::try_current().is_ok() {
            std::mem::drop(tokio::task::spawn_blocking(move || {
                provider_flush(&provider)
            }));
        } else {
            provider_flush(&provider);
        }
    }
}

/// The sink handed to the host's composite sink.
struct CollectorSink {
    collector: Arc<Mutex<RunCollector>>,
}

#[async_trait::async_trait]
impl EventSink for CollectorSink {
    async fn emit(&self, event: AgentEvent) {
        if let Ok(mut collector) = self.collector.lock() {
            collector.ingest(&event, SystemTime::now());
        }
    }
}

/// Emit every observation as an OTel span, parenting children on their
/// recorded parent. Records are ordered parent-before-child by the collector.
fn export(tracer: &SdkTracer, records: &[Observation]) {
    let mut contexts: Vec<Option<Context>> = Vec::with_capacity(records.len());
    for obs in records {
        let parent_cx = obs
            .parent
            .and_then(|p| contexts.get(p).cloned().flatten())
            .unwrap_or_else(Context::new);
        let builder = tracer
            .span_builder(obs.name.clone())
            .with_kind(span_kind(obs.kind))
            .with_start_time(obs.start)
            .with_attributes(obs.attrs.iter().map(to_key_value))
            .with_events(obs.events.iter().map(to_otel_event).collect())
            .with_status(to_status(&obs.status));
        let mut span = builder.start_with_context(tracer, &parent_cx);
        contexts.push(Some(
            Context::new().with_remote_span_context(span.span_context().clone()),
        ));
        span.end_with_timestamp(obs.end);
    }
}

fn span_kind(_kind: ObsKind) -> SpanKind {
    SpanKind::Internal
}

fn to_key_value((key, value): &(String, AttrValue)) -> KeyValue {
    KeyValue::new(key.clone(), to_value(value))
}

fn to_value(value: &AttrValue) -> Value {
    match value {
        AttrValue::Str(s) => Value::String(s.clone().into()),
        AttrValue::Int(i) => Value::I64(*i),
        AttrValue::Float(f) => Value::F64(*f),
        AttrValue::Bool(b) => Value::Bool(*b),
        AttrValue::StrArray(items) => Value::Array(Array::String(
            items.iter().cloned().map(Into::into).collect(),
        )),
    }
}

fn to_otel_event(event: &ObsEvent) -> OtelEvent {
    OtelEvent::new(
        event.name.clone(),
        event.time,
        event.attrs.iter().map(to_key_value).collect(),
        0,
    )
}

fn to_status(status: &ObsStatus) -> OtelStatus {
    match status {
        ObsStatus::Unset => OtelStatus::Unset,
        ObsStatus::Ok => OtelStatus::Ok,
        ObsStatus::Error(message) => OtelStatus::Error {
            description: message.clone().into(),
        },
    }
}

/// Keep a run when `draw` (uniform in `[0, 1)`) is below `ratio`.
fn should_sample(ratio: f64, draw: f64) -> bool {
    draw < ratio
}

/// A cheap uniform draw in `[0, 1)` derived from the wall clock.
fn sample_draw() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    f64::from(nanos) / 1_000_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(endpoint: &str) -> LangfuseConfig {
        LangfuseConfig {
            endpoint: endpoint.to_string(),
            headers: Vec::new(),
            service_name: "recursive-test".to_string(),
            sample_ratio: 1.0,
            redact: true,
        }
    }

    // Regression for #124: without the `reqwest-client` feature the OTLP HTTP
    // exporter fails to build ("no http client") and `start` returns `None`,
    // silently disabling the whole pipeline in every build configuration.
    #[tokio::test]
    async fn pipeline_builds_with_an_http_client() {
        let run = ActiveRun::start(
            &RunMeta::new("sess", "model", "provider"),
            &test_config("http://127.0.0.1:1/v1/traces"),
        );
        assert!(run.is_some(), "OTLP HTTP exporter must build");
    }

    // Regression for #124: the batch span processor that performs the OTLP
    // POST is a task on the host's Tokio runtime. A host that finishes a run
    // and then lets its runtime shut down must still see the request on the
    // wire, so `finish` has to await the flush instead of detaching it (a
    // detached flush is dropped with the runtime — observed as a bare TCP
    // connect with no request written).
    #[test]
    fn finish_delivers_the_export_before_the_runtime_is_dropped() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let (tx, rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            listener
                .set_nonblocking(true)
                .expect("nonblocking listener");
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(std::time::Instant::now() < deadline, "no export arrived");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("accept failed: {e}"),
                }
            };
            stream
                .set_nonblocking(false)
                .expect("blocking client stream");
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).expect("read request");
            tx.send(()).expect("signal the export was received");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
                .expect("write response");
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        runtime.block_on(async {
            let run = ActiveRun::start(
                &RunMeta::new("sess", "model", "provider"),
                &test_config(&format!("http://{addr}/v1/traces")),
            )
            .expect("OTLP HTTP exporter must build");
            let mut span = run.tracer.start("probe");
            span.end();
            run.finish(None, None).await;
        });
        // Simulate the host returning from `main` (runtime shutdown).
        drop(runtime);

        rx.recv_timeout(Duration::from_secs(2))
            .expect("export must be on the wire before the runtime shuts down");
        let request = server.join().expect("server thread");
        assert!(request.starts_with("POST "), "got: {request}");
    }

    #[tokio::test]
    async fn zero_sample_ratio_never_starts() {
        let cfg = LangfuseConfig {
            sample_ratio: 0.0,
            ..test_config("http://127.0.0.1:1/v1/traces")
        };
        assert!(ActiveRun::start(&RunMeta::new("s", "m", "p"), &cfg).is_none());
    }

    #[test]
    fn sampling_ratio_endpoints() {
        assert!(should_sample(1.0, 0.0));
        assert!(!should_sample(0.0, 0.0));
        assert!(should_sample(0.5, 0.25));
        assert!(!should_sample(0.5, 0.75));
    }

    #[test]
    fn sample_draw_is_in_unit_interval() {
        let draw = sample_draw();
        assert!((0.0..1.0).contains(&draw), "draw out of range: {draw}");
    }

    #[test]
    fn attribute_values_map_to_otel_values() {
        assert_eq!(
            to_value(&AttrValue::Str("x".into())),
            Value::String("x".into())
        );
        assert_eq!(to_value(&AttrValue::Int(7)), Value::I64(7));
        assert_eq!(to_value(&AttrValue::Float(1.5)), Value::F64(1.5));
        assert_eq!(to_value(&AttrValue::Bool(true)), Value::Bool(true));
        assert_eq!(
            to_value(&AttrValue::StrArray(vec!["a".into()])),
            Value::Array(Array::String(vec!["a".into()]))
        );
    }

    #[test]
    fn statuses_map_to_otel_statuses() {
        assert_eq!(to_status(&ObsStatus::Unset), OtelStatus::Unset);
        assert_eq!(to_status(&ObsStatus::Ok), OtelStatus::Ok);
        assert_eq!(
            to_status(&ObsStatus::Error("boom".into())),
            OtelStatus::Error {
                description: "boom".into()
            }
        );
    }

    #[test]
    fn span_kind_is_internal_for_both_observation_kinds() {
        assert_eq!(span_kind(ObsKind::Span), SpanKind::Internal);
        assert_eq!(span_kind(ObsKind::Generation), SpanKind::Internal);
    }

    #[test]
    fn event_conversion_preserves_name_and_time() {
        let time = SystemTime::UNIX_EPOCH + Duration::from_secs(42);
        let otel = to_otel_event(&ObsEvent {
            name: "llm.retry".into(),
            time,
            attrs: vec![("k".into(), AttrValue::Int(1))],
        });
        assert_eq!(otel.name, "llm.retry");
        assert_eq!(otel.timestamp, time);
        assert_eq!(otel.attributes.len(), 1);
    }
}
