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
    #[cfg(test)]
    if let Some(nanos) = SEEDED_SAMPLE_NANOS.with(|seed| seed.get()) {
        return nanos_to_unit_interval(nanos);
    }
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    nanos_to_unit_interval(nanos)
}

/// The sampler's mapping core: subsecond nanos to a draw in `[0, 1)`. Pure so
/// the division is pinned deterministically — the wall-clock path behind
/// `sample_draw` cannot be asserted against on hosts with a coarse clock tick
/// (issue #245: the batch-38 100-draw statistical assertion false-reds there).
fn nanos_to_unit_interval(nanos: u32) -> f64 {
    f64::from(nanos) / 1_000_000_000.0
}

// Test seam for `sample_draw`: when seeded, the draw comes from here instead
// of the wall clock, so the `-> 0.0` body replacement stays killable at any
// host clock granularity. Never compiled outside tests. (Plain comments, not
// `///`: rustdoc does not document macro invocations, so `-D warnings` flags
// doc comments here.)
#[cfg(test)]
thread_local! {
    static SEEDED_SAMPLE_NANOS: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
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

    /// The collector sink must actually ingest (issue #245 batch 38):
    /// `emit -> ()` empties every run's observations while the wire test
    /// below still passes — `records()` always contains the run's root
    /// record, so the pin asserts the exact record count for one step.
    #[tokio::test]
    async fn collector_sink_ingests_one_event_into_one_record() {
        let run = ActiveRun::start(
            &RunMeta::new("sess", "model", "provider"),
            &test_config("http://127.0.0.1:1/v1/traces"),
        )
        .expect("OTLP HTTP exporter must build");
        run.sink()
            .emit(AgentEvent::Latency {
                step: 0,
                llm_ms: 42,
            })
            .await;
        assert_eq!(
            run.collector.lock().expect("collector").records().len(),
            2,
            "root record + one ingested step, nothing more"
        );
    }

    /// `queue` is idempotent by contract (returns `false` when the run
    /// was already queued) — issue #245 batch 38: a `-> true` replacement
    /// re-exports on every call and Drop, `-> false` never exports.
    #[test]
    fn queue_is_idempotent() {
        // A real `queue()` leaves spans in the batch processor; on a
        // current-thread runtime its shutdown waits for a task that can
        // never be polled again (see `provider_flush`'s doc comment), so
        // this test needs worker threads like the wire test below.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        runtime.block_on(async {
            let run = ActiveRun::start(
                &RunMeta::new("sess", "model", "provider"),
                &test_config("http://127.0.0.1:1/v1/traces"),
            )
            .expect("OTLP HTTP exporter must build");
            assert!(run.queue(), "the first queue call must proceed");
            assert!(!run.queue(), "a second queue call must be a no-op");
        });
    }

    /// `nanos_to_unit_interval` mapping pins (issue #245). The batch-38
    /// `sample_draw_spans_the_unit_interval` statistical assertion (max of
    /// 100 wall-clock draws > 0.5) was deleted instead: on runners whose
    /// clock tick is coarse (e.g. 4 ms), 100 tight-loop draws land in one
    /// tick and every draw sits near 0.0 — the deterministic CI red behind
    /// main b99fb13d, third clock-quantum casualty (after the create_
    /// scratch_dir collision family). These pins kill the same mutation
    /// family (fn-body constantification, `/` -> `*`/`%`) deterministically.
    #[test]
    fn nanos_to_unit_interval_maps_deterministically() {
        assert_eq!(nanos_to_unit_interval(0), 0.0);
        assert_eq!(nanos_to_unit_interval(500_000_000), 0.5);
        assert!(nanos_to_unit_interval(999_999_999) < 1.0);
        // Out of the caller's domain (subsec_nanos is always < 1e9): the
        // mapping is pure division, no clamp — u32::MAX overshoots past 1.0.
        assert!(nanos_to_unit_interval(u32::MAX) > 1.0);
    }

    /// Seamed wiring pin: a seeded draw must route through
    /// `nanos_to_unit_interval` (seed 0.5 s -> draw 0.5). Kills the
    /// `sample_draw -> 0.0` body replacement — the mutant the deleted
    /// statistical test was the only killer of — at any clock granularity.
    #[test]
    fn sample_draw_routes_seeded_nanos_through_the_mapping() {
        SEEDED_SAMPLE_NANOS.with(|seed| seed.set(Some(500_000_000)));
        assert_eq!(sample_draw(), 0.5);
        SEEDED_SAMPLE_NANOS.with(|seed| seed.set(None));
    }

    /// A run fed ONLY through its event sink must put its spans on the
    /// wire by `finish` (issue #245 batch 38). `finish_delivers_…` cannot
    /// see the queue/export/flush chain — it starts its probe span on the
    /// tracer directly — so `queue -> true`, `export -> ()`,
    /// `provider_flush -> ()` and `drain -> ()` all survived it.
    #[test]
    fn sink_fed_run_reaches_the_wire_before_finish_returns() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("addr");
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
            run.sink()
                .emit(AgentEvent::Latency {
                    step: 0,
                    llm_ms: 42,
                })
                .await;
            run.finish(None, None).await;
            // The POST must be on the wire while `run` is STILL ALIVE:
            // `finish` has to force the processor via drain →
            // provider_flush. A `drain -> ()` or `provider_flush -> ()`
            // mutation defers the export to the provider's own
            // Drop-shutdown, which only fires after this scope ends — and
            // the batch processor's own 5 s schedule cannot beat the 2 s
            // recv — so this wait goes red under either mutation. Blocking
            // the block_on thread here is fine: the multi-thread runtime's
            // workers drive the export POST on their own threads.
            rx.recv_timeout(Duration::from_secs(2))
                .expect("sink-fed export must be forced onto the wire by finish itself");
        });
        // Simulate the host returning from `main` (runtime shutdown).
        drop(runtime);

        let request = server.join().expect("server thread");
        assert!(request.starts_with("POST "), "got: {request}");
    }
}
