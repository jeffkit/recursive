//! Optional Langfuse / OTLP observability export (issue #124).
//!
//! When the crate is built with the `otel` feature **and** the Langfuse env
//! vars are set at runtime, an [`AgentEvent`](crate::event::AgentEvent) sink
//! records one trace per agent run and exports it to Langfuse over OTLP
//! HTTP/protobuf (`/api/public/otel/v1/traces`).
//!
//! ## Env contract
//!
//! | Var | Required | Meaning |
//! |-----|----------|---------|
//! | `LANGFUSE_PUBLIC_KEY` | yes (Langfuse path) | `pk-lf-…` public key |
//! | `LANGFUSE_SECRET_KEY` | yes (Langfuse path) | `sk-lf-…` secret key |
//! | `LANGFUSE_HOST` | no | default `https://cloud.langfuse.com` |
//! | `LANGFUSE_OTEL_ENDPOINT` | no | override the full traces URL |
//! | `RECURSIVE_OTEL_TRACES_URL` | no | generic OTLP HTTP traces URL (no auth) |
//! | `LANGFUSE_SAMPLE_RATIO` | no | `0.0..=1.0`, default `1.0` |
//! | `LANGFUSE_REDACT` | no | `1` (default) reports length+hash, `0` reports text |
//!
//! Hosts attach the sink with [`with_sink`] and report the terminal state
//! with [`LangfuseRun::finish`]. Everything is fail-open: with the feature
//! off, or env unset, the handle is a no-op.
//!
//! The attribute names follow the `langfuse.*` convention (see the
//! `collector` submodule) so traces produced here and by the plaita
//! orchestration stack land in the same Langfuse project and are comparable.

use crate::event::EventSink;

#[cfg(feature = "otel")]
mod collector;
#[cfg(feature = "otel")]
mod config;
#[cfg(feature = "otel")]
mod exporter;

#[cfg(feature = "otel")]
pub use collector::{fingerprint, redact, ObsKind, Observation, RunCollector};
#[cfg(feature = "otel")]
pub use config::LangfuseConfig;

/// Static metadata for one agent run (one Langfuse trace).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunMeta {
    /// Session id the run belongs to (empty when the host has none).
    pub session_id: String,
    /// Turn index within the session (`0` for a one-shot run).
    pub turn: u32,
    /// Model name reported on every generation span (empty → env fallback).
    pub model: String,
    /// Provider system reported on every generation span.
    pub provider: String,
    /// Langfuse trace name (`langfuse.trace.name`).
    pub trace_name: String,
    /// `langfuse.trace.tags` applied to the trace.
    pub tags: Vec<String>,
}

impl RunMeta {
    /// Build run metadata with the default trace name (`recursive.run`).
    pub fn new(
        session_id: impl Into<String>,
        model: impl Into<String>,
        provider: impl Into<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            turn: 0,
            model: model.into(),
            provider: provider.into(),
            trace_name: "recursive.run".to_string(),
            tags: Vec::new(),
        }
    }

    /// Fill `model` / `provider` from the environment when a host did not
    /// supply them (`RECURSIVE_MODEL` / `RECURSIVE_PROVIDER_TYPE`).
    ///
    /// Structurally untestable without mutating the process env; the pure
    /// form below carries the behaviour and its tests.
    #[cfg_attr(test, mutants::skip)]
    pub fn with_env_fallbacks(self) -> Self {
        self.with_defaults(|k| std::env::var(k).ok())
    }

    /// Set the turn index within the session (0 for a one-shot run).
    pub fn with_turn(mut self, turn: u32) -> Self {
        self.turn = turn;
        self
    }

    /// Pure form of [`RunMeta::with_env_fallbacks`] taking an explicit getter.
    pub fn with_defaults(mut self, get: impl Fn(&str) -> Option<String>) -> Self {
        if self.model.is_empty() {
            self.model = get("RECURSIVE_MODEL").unwrap_or_default();
        }
        if self.provider.is_empty() {
            self.provider = get("RECURSIVE_PROVIDER_TYPE").unwrap_or_default();
        }
        self
    }
}

/// Handle to an in-flight Langfuse run.
///
/// Cheap and inert when the `otel` feature is not compiled in or the env
/// vars are unset: [`sink`](Self::sink) returns `None` and
/// [`finish`](Self::finish) is a no-op.
pub struct LangfuseRun {
    #[cfg(feature = "otel")]
    inner: Option<exporter::ActiveRun>,
}

impl LangfuseRun {
    #[cfg(feature = "otel")]
    fn inert() -> Self {
        Self { inner: None }
    }

    #[cfg(not(feature = "otel"))]
    fn inert() -> Self {
        Self {}
    }

    /// Start a run trace when the `otel` feature is on and the env is
    /// configured; otherwise return an inert handle.
    #[cfg_attr(test, mutants::skip)]
    pub fn try_new(meta: RunMeta) -> Self {
        #[cfg(feature = "otel")]
        {
            Self::from_config(meta, config::langfuse_config_from_env())
        }
        #[cfg(not(feature = "otel"))]
        {
            let _ = &meta;
            Self::inert()
        }
    }

    /// Pure form of [`try_new`](Self::try_new) taking an explicit config
    /// decision.
    ///
    /// Keeping the env lookup in `try_new` alone lets the tests exercise the
    /// inert path without depending on — or mutating — the process
    /// environment, and without ever building a real exporter.
    #[cfg(feature = "otel")]
    fn from_config(meta: RunMeta, cfg: Option<LangfuseConfig>) -> Self {
        if let Some(cfg) = cfg {
            let meta = meta.with_env_fallbacks();
            if let Some(inner) = exporter::ActiveRun::start(&meta, &cfg) {
                return Self { inner: Some(inner) };
            }
        }
        Self::inert()
    }

    /// Whether a live exporter is attached.
    pub fn is_active(&self) -> bool {
        #[cfg(feature = "otel")]
        {
            self.inner.is_some()
        }
        #[cfg(not(feature = "otel"))]
        {
            false
        }
    }

    /// The event sink to add to a host's composite sink, if active.
    pub fn sink(&self) -> Option<Box<dyn EventSink>> {
        #[cfg(feature = "otel")]
        {
            self.inner.as_ref().map(|i| i.sink())
        }
        #[cfg(not(feature = "otel"))]
        {
            None
        }
    }

    /// Record the terminal state and flush the trace. Idempotent.
    ///
    /// `finish_reason` is the run's terminal reason (`"no_more_tool_calls"`,
    /// `"budget_exceeded"`, …) and `error` the provider/transport error
    /// message for a failed run — exactly one is normally `Some`. Awaiting it
    /// guarantees the export has completed before the host tears down the
    /// Tokio runtime the batch processor runs on.
    #[cfg_attr(test, mutants::skip)]
    pub async fn finish(&self, finish_reason: Option<&str>, error: Option<&str>) {
        #[cfg(feature = "otel")]
        if let Some(inner) = &self.inner {
            inner.finish(finish_reason, error).await;
        }
        #[cfg(not(feature = "otel"))]
        {
            let _ = (finish_reason, error);
        }
    }
}

/// Append the run's event sink to `sinks` when the exporter is active.
///
/// Keeps the conditional wiring — and its mutation coverage — in one place,
/// so hosts only need a single call.
pub fn with_sink(run: &LangfuseRun, sinks: &mut Vec<Box<dyn EventSink>>) {
    if let Some(sink) = run.sink() {
        sinks.push(sink);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DropMarker;

    #[async_trait::async_trait]
    impl EventSink for DropMarker {
        async fn emit(&self, _event: crate::event::AgentEvent) {}
    }

    /// An inert run built without any env lookup, so the unit tests neither
    /// depend on the ambient `LANGFUSE_*` / `RECURSIVE_OTEL_*` environment nor
    /// construct a live exporter (and thus never attempt network setup).
    fn inert_run() -> LangfuseRun {
        #[cfg(feature = "otel")]
        {
            LangfuseRun::from_config(RunMeta::new("s1", "test-model", "mock"), None)
        }
        #[cfg(not(feature = "otel"))]
        {
            LangfuseRun::try_new(RunMeta::new("s1", "test-model", "mock"))
        }
    }

    #[test]
    fn no_config_yields_an_inert_run() {
        let run = inert_run();
        assert!(!run.is_active());
        assert!(run.sink().is_none());
    }

    #[test]
    fn with_sink_is_a_noop_when_inactive() {
        let run = inert_run();
        let mut sinks: Vec<Box<dyn EventSink>> = vec![Box::new(DropMarker)];
        with_sink(&run, &mut sinks);
        assert_eq!(sinks.len(), 1);
    }

    #[tokio::test]
    async fn finish_is_safe_when_inactive() {
        let run = inert_run();
        run.finish(Some("no_more_tool_calls"), None).await;
        run.finish(None, Some("boom")).await;
        run.finish(None, None).await;
    }

    #[test]
    fn run_meta_new_sets_defaults() {
        let meta = RunMeta::new("sess", "gpt-x", "openai");
        assert_eq!(meta.session_id, "sess");
        assert_eq!(meta.model, "gpt-x");
        assert_eq!(meta.provider, "openai");
        assert_eq!(meta.turn, 0);
        assert_eq!(meta.trace_name, "recursive.run");
        assert!(meta.tags.is_empty());
    }

    #[test]
    fn run_meta_with_defaults_fills_only_empty_fields() {
        let filled = RunMeta::new("s", "", "").with_defaults(|k| match k {
            "RECURSIVE_MODEL" => Some("env-model".to_string()),
            "RECURSIVE_PROVIDER_TYPE" => Some("env-provider".to_string()),
            _ => None,
        });
        assert_eq!(filled.model, "env-model");
        assert_eq!(filled.provider, "env-provider");

        let kept = RunMeta::new("s", "explicit", "explicit-provider")
            .with_defaults(|_| Some("env".to_string()));
        assert_eq!(kept.model, "explicit");
        assert_eq!(kept.provider, "explicit-provider");
    }

    #[test]
    fn run_meta_with_turn_sets_the_index() {
        assert_eq!(RunMeta::new("s", "m", "p").with_turn(3).turn, 3);
    }

    #[test]
    fn run_meta_with_defaults_tolerates_missing_env() {
        let meta = RunMeta::new("s", "", "").with_defaults(|_| None);
        assert!(meta.model.is_empty());
        assert!(meta.provider.is_empty());
    }
}
