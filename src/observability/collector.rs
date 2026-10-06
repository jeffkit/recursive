//! Turn [`AgentEvent`] streams into Langfuse-shaped observations.
//!
//! The collector is a pure state machine: it consumes events and produces
//! [`Observation`] records with explicit wall-clock start/end times and
//! `langfuse.*` attributes. The [`exporter`](super::exporter) converts those
//! records to OpenTelemetry spans — keeping the testable mapping separate
//! from the network plumbing.
//!
//! Attribute names follow the Langfuse OTel ingestion convention so traces
//! produced here and by the plaita `obs.py` stack are comparable in the same
//! Langfuse project.

use std::time::SystemTime;

use crate::event::AgentEvent;
use crate::llm::pricing::pricing_for;
use crate::llm::TokenUsage;
use crate::message::Role;

use super::RunMeta;

// ── Attribute-name constants (Langfuse OTel convention) ─────────────────────

/// `langfuse.trace.name` — the trace (root span) name.
pub const ATTR_TRACE_NAME: &str = "langfuse.trace.name";
/// `langfuse.trace.sessionId` — groups traces by session.
pub const ATTR_TRACE_SESSION_ID: &str = "langfuse.trace.sessionId";
/// `langfuse.trace.tags` — string-array tag list.
pub const ATTR_TRACE_TAGS: &str = "langfuse.trace.tags";
/// `langfuse.observation.type` — `"span"` or `"generation"`.
pub const ATTR_OBSERVATION_TYPE: &str = "langfuse.observation.type";
/// `langfuse.observation.model.name` — model for a generation.
pub const ATTR_OBSERVATION_MODEL: &str = "langfuse.observation.model.name";
/// `langfuse.observation.input` — (redacted) observation input.
pub const ATTR_OBSERVATION_INPUT: &str = "langfuse.observation.input";
/// `langfuse.observation.output` — (redacted) observation output.
pub const ATTR_OBSERVATION_OUTPUT: &str = "langfuse.observation.output";
/// `langfuse.observation.level` — `DEFAULT` / `ERROR`.
pub const ATTR_OBSERVATION_LEVEL: &str = "langfuse.observation.level";
/// `gen_ai.usage.input_tokens`.
pub const ATTR_GEN_AI_INPUT_TOKENS: &str = "gen_ai.usage.input_tokens";
/// `gen_ai.usage.output_tokens`.
pub const ATTR_GEN_AI_OUTPUT_TOKENS: &str = "gen_ai.usage.output_tokens";
/// `gen_ai.request.model`.
pub const ATTR_GEN_AI_REQUEST_MODEL: &str = "gen_ai.request.model";
/// `gen_ai.system` — provider identity.
pub const ATTR_GEN_AI_SYSTEM: &str = "gen_ai.system";

/// Prefix for `langfuse.trace.metadata.<key>` attributes.
pub const TRACE_METADATA_PREFIX: &str = "langfuse.trace.metadata.";
/// Prefix for `langfuse.observation.metadata.<key>` attributes.
pub const OBSERVATION_METADATA_PREFIX: &str = "langfuse.observation.metadata.";

/// Plaintext payloads longer than this are truncated when redaction is off.
pub const MAX_INLINE_CHARS: usize = 4096;

// ── Observation model ───────────────────────────────────────────────────────

/// The Langfuse observation kind a record maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObsKind {
    /// A generic span.
    Span,
    /// An LLM generation.
    Generation,
}

/// A typed attribute value.
#[derive(Debug, Clone, PartialEq)]
pub enum AttrValue {
    /// UTF-8 string.
    Str(String),
    /// Signed integer.
    Int(i64),
    /// Floating point.
    Float(f64),
    /// Boolean.
    Bool(bool),
    /// String array (e.g. `langfuse.trace.tags`).
    StrArray(Vec<String>),
}

/// Span status.
#[derive(Debug, Clone, PartialEq)]
pub enum ObsStatus {
    /// Not explicitly set.
    Unset,
    /// Success.
    Ok,
    /// Failure with a description.
    Error(String),
}

/// A timestamped span event.
#[derive(Debug, Clone, PartialEq)]
pub struct ObsEvent {
    /// Event name.
    pub name: String,
    /// Event time.
    pub time: SystemTime,
    /// Event attributes.
    pub attrs: Vec<(String, AttrValue)>,
}

/// One observation (span) in a run trace.
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    /// Span name.
    pub name: String,
    /// Span kind.
    pub kind: ObsKind,
    /// Index of the parent observation in the same `Vec` (`None` = root).
    pub parent: Option<usize>,
    /// Start time.
    pub start: SystemTime,
    /// End time.
    pub end: SystemTime,
    /// Attributes.
    pub attrs: Vec<(String, AttrValue)>,
    /// Span events.
    pub events: Vec<ObsEvent>,
    /// Span status.
    pub status: ObsStatus,
}

// ── Redaction ───────────────────────────────────────────────────────────────

/// Deterministic 64-bit FNV-1a fingerprint of `text`.
pub fn fingerprint(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Redacted representation of `text`: byte length plus a stable fingerprint.
///
/// The default for every prompt / tool payload — plaintext is never reported
/// unless the operator explicitly disables redaction.
pub fn redact(text: &str) -> String {
    format!("len={} hash={:016x}", text.len(), fingerprint(text))
}

fn payload(text: &str, redact_payload: bool) -> String {
    if redact_payload {
        redact(text)
    } else {
        text.chars().take(MAX_INLINE_CHARS).collect()
    }
}

fn opt_payload(text: Option<&str>, redact_payload: bool) -> Option<String> {
    text.map(|t| payload(t, redact_payload))
}

// ── Collector ───────────────────────────────────────────────────────────────

struct RetryRec {
    attempt: u32,
    wait_ms: u64,
    reason: String,
    time: SystemTime,
}

struct ToolState {
    step_index: usize,
    name: String,
    id: String,
    start: SystemTime,
    end: SystemTime,
    is_error: bool,
    arguments: Option<String>,
    output: Option<String>,
}

struct StepState {
    turn: u32,
    step: usize,
    start: SystemTime,
    end: SystemTime,
    latency_ms: Option<u64>,
    usage: TokenUsage,
    cost_usd: f64,
    output: Option<String>,
    retries: Vec<RetryRec>,
}

/// Accumulates one run's events into observations.
pub struct RunCollector {
    meta: RunMeta,
    redact_payload: bool,
    start: SystemTime,
    end: Option<SystemTime>,
    turn: u32,
    steps: Vec<StepState>,
    tools: Vec<ToolState>,
    input: Option<String>,
    total_usage: TokenUsage,
    total_cost_usd: f64,
    retries: u32,
    compactions: u32,
    finish_reason: Option<String>,
    error: Option<String>,
}

impl RunCollector {
    /// Create a collector for `meta`, redacting payloads when requested.
    pub fn new(meta: RunMeta, redact_payload: bool, start: SystemTime) -> Self {
        Self {
            turn: meta.turn,
            meta,
            redact_payload,
            start,
            end: None,
            steps: Vec::new(),
            tools: Vec::new(),
            input: None,
            total_usage: TokenUsage::default(),
            total_cost_usd: 0.0,
            retries: 0,
            compactions: 0,
            finish_reason: None,
            error: None,
        }
    }

    /// Whether the collector has seen the run's terminal event.
    pub fn is_finished(&self) -> bool {
        self.end.is_some()
    }

    /// Feed one agent event stamped at `now`.
    pub fn ingest(&mut self, event: &AgentEvent, now: SystemTime) {
        self.end = Some(now);
        match event {
            AgentEvent::MessageAppended { message, .. }
            | AgentEvent::MessageAppendedWithAudit { message, .. } => {
                if message.role == Role::User && self.input.is_none() {
                    self.input = Some(message.content.clone());
                }
            }
            AgentEvent::AssistantText { text, step } => {
                let idx = self.step_slot(*step, now);
                self.steps[idx].output = Some(text.clone());
                self.steps[idx].end = now;
            }
            AgentEvent::ToolCall {
                name,
                id,
                arguments,
                step,
            } => {
                let idx = self.step_slot(*step, now);
                self.steps[idx].end = now;
                self.tools.push(ToolState {
                    step_index: idx,
                    name: name.clone(),
                    id: id.clone(),
                    start: now,
                    end: now,
                    is_error: false,
                    arguments: Some(arguments.clone()),
                    output: None,
                });
            }
            AgentEvent::ToolResult {
                id,
                output,
                is_error,
                step,
                ..
            } => {
                let idx = self.step_slot(*step, now);
                self.steps[idx].end = now;
                if let Some(tool) = self.tools.iter_mut().rev().find(|t| &t.id == id) {
                    tool.end = now;
                    tool.is_error = *is_error;
                    tool.output = Some(output.clone());
                }
            }
            AgentEvent::Latency { step, llm_ms } => {
                let idx = self.step_slot(*step, now);
                self.steps[idx].latency_ms = Some(*llm_ms);
                self.steps[idx].end = now;
            }
            AgentEvent::Usage {
                input_tokens,
                output_tokens,
                cache_hit_tokens,
                cache_miss_tokens,
                step,
            } => {
                let idx = self.step_slot(*step, now);
                let usage = TokenUsage {
                    prompt_tokens: *input_tokens,
                    completion_tokens: *output_tokens,
                    total_tokens: input_tokens.saturating_add(*output_tokens),
                    cache_hit_tokens: *cache_hit_tokens,
                    cache_miss_tokens: *cache_miss_tokens,
                    reasoning_tokens: 0,
                };
                self.steps[idx].usage = usage;
                self.steps[idx].cost_usd = cost_usd(&self.meta.model, usage);
                self.steps[idx].end = now;
                self.total_usage = self.total_usage.accumulate(usage);
                self.total_cost_usd += cost_usd(&self.meta.model, usage);
            }
            AgentEvent::LlmRetry {
                step,
                attempt,
                wait_ms,
                reason,
            } => {
                let idx = self.step_slot(*step, now);
                self.steps[idx].end = now;
                self.steps[idx].retries.push(RetryRec {
                    attempt: *attempt,
                    wait_ms: *wait_ms,
                    reason: reason.clone(),
                    time: now,
                });
                self.retries = self.retries.saturating_add(1);
            }
            AgentEvent::Compacted { .. }
            | AgentEvent::CompactionBoundary { .. }
            | AgentEvent::CompactionSkipped { .. } => {
                self.compactions = self.compactions.saturating_add(1);
            }
            AgentEvent::TurnFinished { reason, .. } => {
                self.finish_reason = Some(reason.clone());
                self.turn = self.turn.saturating_add(1);
            }
            _ => {}
        }
    }

    /// Record the terminal state explicitly (success reason or error message).
    pub fn finish(&mut self, finish_reason: Option<&str>, error: Option<&str>, now: SystemTime) {
        if let Some(reason) = finish_reason {
            self.finish_reason = Some(reason.to_string());
        }
        if let Some(err) = error {
            self.error = Some(err.to_string());
        }
        self.end = Some(now);
    }

    fn step_slot(&mut self, step: usize, now: SystemTime) -> usize {
        if let Some(idx) = self
            .steps
            .iter()
            .position(|s| s.turn == self.turn && s.step == step)
        {
            return idx;
        }
        self.steps.push(StepState {
            turn: self.turn,
            step,
            start: now,
            end: now,
            latency_ms: None,
            usage: TokenUsage::default(),
            cost_usd: 0.0,
            output: None,
            retries: Vec::new(),
        });
        self.steps.len() - 1
    }

    /// Build the flat observation list (root first, then steps, then tools).
    pub fn records(&self) -> Vec<Observation> {
        let mut records = Vec::with_capacity(1 + self.steps.len() + self.tools.len());
        records.push(self.root_record());

        for step in &self.steps {
            records.push(self.step_record(step));
        }
        for tool in &self.tools {
            records.push(self.tool_record(tool));
        }
        records
    }

    fn root_record(&self) -> Observation {
        let mut attrs = vec![
            (
                ATTR_TRACE_NAME.to_string(),
                AttrValue::Str(self.meta.trace_name.clone()),
            ),
            (
                format!("{TRACE_METADATA_PREFIX}steps"),
                AttrValue::Int(self.steps.len() as i64),
            ),
            (
                format!("{TRACE_METADATA_PREFIX}turn"),
                AttrValue::Int(i64::from(self.meta.turn)),
            ),
            (
                format!("{TRACE_METADATA_PREFIX}llm_retries"),
                AttrValue::Int(i64::from(self.retries)),
            ),
            (
                format!("{TRACE_METADATA_PREFIX}compactions"),
                AttrValue::Int(i64::from(self.compactions)),
            ),
            (
                ATTR_OBSERVATION_TYPE.to_string(),
                AttrValue::Str("span".into()),
            ),
            (
                ATTR_GEN_AI_INPUT_TOKENS.to_string(),
                AttrValue::Int(i64::from(self.total_usage.prompt_tokens)),
            ),
            (
                ATTR_GEN_AI_OUTPUT_TOKENS.to_string(),
                AttrValue::Int(i64::from(self.total_usage.completion_tokens)),
            ),
            (
                format!("{OBSERVATION_METADATA_PREFIX}cost_usd"),
                AttrValue::Float(self.total_cost_usd),
            ),
        ];
        if !self.meta.session_id.is_empty() {
            attrs.push((
                ATTR_TRACE_SESSION_ID.to_string(),
                AttrValue::Str(self.meta.session_id.clone()),
            ));
        }
        if !self.meta.model.is_empty() {
            attrs.push((
                format!("{TRACE_METADATA_PREFIX}model"),
                AttrValue::Str(self.meta.model.clone()),
            ));
        }
        if !self.meta.tags.is_empty() {
            attrs.push((
                ATTR_TRACE_TAGS.to_string(),
                AttrValue::StrArray(self.meta.tags.clone()),
            ));
        }
        if let Some(reason) = &self.finish_reason {
            attrs.push((
                format!("{TRACE_METADATA_PREFIX}finish_reason"),
                AttrValue::Str(reason.clone()),
            ));
        }
        if let Some(err) = &self.error {
            attrs.push((
                format!("{TRACE_METADATA_PREFIX}error"),
                AttrValue::Str(err.clone()),
            ));
        }
        if let Some(input) = opt_payload(self.input.as_deref(), self.redact_payload) {
            attrs.push((ATTR_OBSERVATION_INPUT.to_string(), AttrValue::Str(input)));
        }

        let status = if let Some(err) = &self.error {
            ObsStatus::Error(err.clone())
        } else {
            match &self.finish_reason {
                Some(reason) if is_success_finish(reason) => ObsStatus::Ok,
                Some(reason) => ObsStatus::Error(reason.clone()),
                None => ObsStatus::Unset,
            }
        };

        Observation {
            name: trace_name(&self.meta),
            kind: ObsKind::Span,
            parent: None,
            start: self.start,
            end: self.run_end(),
            attrs,
            events: Vec::new(),
            status,
        }
    }

    fn step_record(&self, step: &StepState) -> Observation {
        let mut attrs = vec![
            (
                ATTR_OBSERVATION_TYPE.to_string(),
                AttrValue::Str("generation".into()),
            ),
            (
                ATTR_OBSERVATION_MODEL.to_string(),
                AttrValue::Str(self.meta.model.clone()),
            ),
            (
                ATTR_GEN_AI_REQUEST_MODEL.to_string(),
                AttrValue::Str(self.meta.model.clone()),
            ),
            (
                ATTR_GEN_AI_SYSTEM.to_string(),
                AttrValue::Str(self.meta.provider.clone()),
            ),
            (
                ATTR_GEN_AI_INPUT_TOKENS.to_string(),
                AttrValue::Int(i64::from(step.usage.prompt_tokens)),
            ),
            (
                ATTR_GEN_AI_OUTPUT_TOKENS.to_string(),
                AttrValue::Int(i64::from(step.usage.completion_tokens)),
            ),
            (
                format!("{OBSERVATION_METADATA_PREFIX}step"),
                AttrValue::Int(step.step as i64),
            ),
            (
                format!("{OBSERVATION_METADATA_PREFIX}turn"),
                AttrValue::Int(i64::from(step.turn)),
            ),
            (
                format!("{OBSERVATION_METADATA_PREFIX}llm_retries"),
                AttrValue::Int(step.retries.len() as i64),
            ),
            (
                format!("{OBSERVATION_METADATA_PREFIX}cost_usd"),
                AttrValue::Float(step.cost_usd),
            ),
        ];
        if let Some(latency) = step.latency_ms {
            attrs.push((
                format!("{OBSERVATION_METADATA_PREFIX}llm_latency_ms"),
                AttrValue::Int(latency as i64),
            ));
        }
        if let Some(output) = opt_payload(step.output.as_deref(), self.redact_payload) {
            attrs.push((ATTR_OBSERVATION_OUTPUT.to_string(), AttrValue::Str(output)));
        }

        let events = step
            .retries
            .iter()
            .map(|r| ObsEvent {
                name: "llm.retry".to_string(),
                time: r.time,
                attrs: vec![
                    (
                        format!("{OBSERVATION_METADATA_PREFIX}retry_attempt"),
                        AttrValue::Int(i64::from(r.attempt)),
                    ),
                    (
                        format!("{OBSERVATION_METADATA_PREFIX}retry_wait_ms"),
                        AttrValue::Int(r.wait_ms as i64),
                    ),
                    (
                        format!("{OBSERVATION_METADATA_PREFIX}retry_reason"),
                        AttrValue::Str(r.reason.clone()),
                    ),
                ],
            })
            .collect();

        Observation {
            name: format!("llm step {}", step.step),
            kind: ObsKind::Generation,
            parent: Some(0),
            start: step.start,
            end: step.end,
            attrs,
            events,
            status: ObsStatus::Unset,
        }
    }

    fn tool_record(&self, tool: &ToolState) -> Observation {
        let duration_ms = tool
            .end
            .duration_since(tool.start)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let mut attrs = vec![
            (
                ATTR_OBSERVATION_TYPE.to_string(),
                AttrValue::Str("span".into()),
            ),
            (
                format!("{OBSERVATION_METADATA_PREFIX}tool_name"),
                AttrValue::Str(tool.name.clone()),
            ),
            (
                format!("{OBSERVATION_METADATA_PREFIX}tool_call_id"),
                AttrValue::Str(tool.id.clone()),
            ),
            (
                format!("{OBSERVATION_METADATA_PREFIX}duration_ms"),
                AttrValue::Int(duration_ms),
            ),
        ];
        if let Some(input) = opt_payload(tool.arguments.as_deref(), self.redact_payload) {
            attrs.push((ATTR_OBSERVATION_INPUT.to_string(), AttrValue::Str(input)));
        }
        if let Some(output) = opt_payload(tool.output.as_deref(), self.redact_payload) {
            attrs.push((ATTR_OBSERVATION_OUTPUT.to_string(), AttrValue::Str(output)));
        }
        attrs.push((
            ATTR_OBSERVATION_LEVEL.to_string(),
            AttrValue::Str(if tool.is_error { "ERROR" } else { "DEFAULT" }.into()),
        ));

        Observation {
            name: format!("tool {}", tool.name),
            kind: ObsKind::Span,
            parent: Some(1 + tool.step_index),
            start: tool.start,
            end: tool.end,
            attrs,
            events: Vec::new(),
            status: if tool.is_error {
                ObsStatus::Error("tool error".into())
            } else {
                ObsStatus::Ok
            },
        }
    }

    fn run_end(&self) -> SystemTime {
        self.end
            .or_else(|| self.steps.iter().map(|s| s.end).max())
            .or_else(|| self.tools.iter().map(|t| t.end).max())
            .unwrap_or(self.start)
    }
}

fn trace_name(meta: &RunMeta) -> String {
    if meta.trace_name.is_empty() {
        "recursive.run".to_string()
    } else {
        meta.trace_name.clone()
    }
}

/// Whether a terminal finish reason represents a normally completed run.
///
/// Every other terminal reason (`budget_exceeded`, `stuck`, `cancelled`,
/// `wall_clock_exceeded`, …) marks the root span `Error` so incomplete runs
/// are distinguishable in Langfuse, not only in the trace metadata.
fn is_success_finish(reason: &str) -> bool {
    reason == "no_more_tool_calls" || reason.starts_with("provider_stop:")
}

fn cost_usd(model: &str, usage: TokenUsage) -> f64 {
    pricing_for(model).map(|p| p.cost_usd(usage)).unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::AgentEvent;
    use std::time::Duration;

    fn t0() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    fn at(offset_ms: u64) -> SystemTime {
        t0() + Duration::from_millis(offset_ms)
    }

    fn meta() -> RunMeta {
        let mut m = RunMeta::new("sess-1", "deepseek-chat", "deepseek");
        m.tags = vec!["self-improve".into()];
        m
    }

    fn collector() -> RunCollector {
        RunCollector::new(meta(), true, t0())
    }

    fn attr<'a>(obs: &'a Observation, key: &str) -> Option<&'a AttrValue> {
        obs.attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    #[test]
    fn fingerprint_is_deterministic_and_distinguishes_inputs() {
        assert_eq!(fingerprint("hello"), fingerprint("hello"));
        assert_ne!(fingerprint("hello"), fingerprint("world"));
        assert_eq!(
            redact("hello"),
            format!("len=5 hash={:016x}", fingerprint("hello"))
        );
    }

    #[test]
    fn redact_is_default_and_len_prefixed() {
        let out = redact("secret-prompt");
        assert!(out.starts_with("len=13 hash="));
        assert!(!out.contains("secret"));
    }

    #[test]
    fn root_record_carries_trace_identity_and_totals() {
        let mut c = collector();
        c.ingest(
            &AgentEvent::Usage {
                input_tokens: 100,
                output_tokens: 40,
                cache_hit_tokens: 10,
                cache_miss_tokens: 90,
                step: 0,
            },
            at(10),
        );
        c.ingest(
            &AgentEvent::TurnFinished {
                reason: "no_more_tool_calls".into(),
                steps: 1,
            },
            at(20),
        );
        let records = c.records();
        let root = &records[0];
        assert_eq!(root.name, "recursive.run");
        assert_eq!(root.parent, None);
        assert_eq!(root.end, at(20));
        assert_eq!(
            attr(root, ATTR_TRACE_NAME),
            Some(&AttrValue::Str("recursive.run".into()))
        );
        assert_eq!(
            attr(root, ATTR_TRACE_SESSION_ID),
            Some(&AttrValue::Str("sess-1".into()))
        );
        assert_eq!(
            attr(root, ATTR_TRACE_TAGS),
            Some(&AttrValue::StrArray(vec!["self-improve".into()]))
        );
        assert_eq!(
            attr(root, ATTR_GEN_AI_INPUT_TOKENS),
            Some(&AttrValue::Int(100))
        );
        assert_eq!(
            attr(root, ATTR_GEN_AI_OUTPUT_TOKENS),
            Some(&AttrValue::Int(40))
        );
        assert_eq!(root.status, ObsStatus::Ok);
        assert!(matches!(
            attr(root, &format!("{TRACE_METADATA_PREFIX}finish_reason")),
            Some(AttrValue::Str(r)) if r == "no_more_tool_calls"
        ));
    }

    #[test]
    fn error_finish_marks_root_status_and_attribute() {
        let mut c = collector();
        c.finish(None, Some("connection reset"), at(50));
        let records = c.records();
        assert_eq!(
            records[0].status,
            ObsStatus::Error("connection reset".into())
        );
        assert_eq!(
            attr(&records[0], &format!("{TRACE_METADATA_PREFIX}error")),
            Some(&AttrValue::Str("connection reset".into()))
        );
        assert_eq!(records[0].end, at(50));
    }

    #[test]
    fn non_success_finish_marks_root_error() {
        for reason in [
            "budget_exceeded",
            "stuck",
            "cancelled",
            "wall_clock_exceeded",
        ] {
            let mut c = collector();
            c.ingest(
                &AgentEvent::TurnFinished {
                    reason: reason.to_string(),
                    steps: 1,
                },
                at(10),
            );
            assert_eq!(
                c.records()[0].status,
                ObsStatus::Error(reason.to_string()),
                "finish reason {reason} must mark the root span error"
            );
        }
    }

    #[test]
    fn normal_and_provider_stop_finishes_stay_ok() {
        for reason in ["no_more_tool_calls", "provider_stop:end_turn"] {
            let mut c = collector();
            c.ingest(
                &AgentEvent::TurnFinished {
                    reason: reason.to_string(),
                    steps: 1,
                },
                at(10),
            );
            assert_eq!(
                c.records()[0].status,
                ObsStatus::Ok,
                "finish reason {reason} must stay ok"
            );
        }
    }

    #[test]
    fn steps_become_generation_records_parented_on_the_root() {
        let mut c = collector();
        c.ingest(
            &AgentEvent::AssistantText {
                text: "hi".into(),
                step: 0,
            },
            at(5),
        );
        c.ingest(
            &AgentEvent::Latency {
                step: 0,
                llm_ms: 123,
            },
            at(6),
        );
        c.ingest(
            &AgentEvent::Usage {
                input_tokens: 10,
                output_tokens: 2,
                cache_hit_tokens: 0,
                cache_miss_tokens: 10,
                step: 0,
            },
            at(7),
        );
        // A second step creates its own record.
        c.ingest(&AgentEvent::Latency { step: 1, llm_ms: 9 }, at(8));

        let records = c.records();
        assert_eq!(records.len(), 3);
        let step0 = &records[1];
        assert_eq!(step0.name, "llm step 0");
        assert_eq!(step0.kind, ObsKind::Generation);
        assert_eq!(step0.parent, Some(0));
        assert_eq!(
            attr(step0, ATTR_OBSERVATION_MODEL),
            Some(&AttrValue::Str("deepseek-chat".into()))
        );
        assert_eq!(
            attr(step0, ATTR_GEN_AI_SYSTEM),
            Some(&AttrValue::Str("deepseek".into()))
        );
        assert_eq!(
            attr(
                step0,
                &format!("{OBSERVATION_METADATA_PREFIX}llm_latency_ms")
            ),
            Some(&AttrValue::Int(123))
        );
        assert_eq!(
            attr(step0, &format!("{OBSERVATION_METADATA_PREFIX}step")),
            Some(&AttrValue::Int(0))
        );
        assert_eq!(records[2].name, "llm step 1");
    }

    #[test]
    fn tool_calls_produce_duration_spans_parented_on_their_step() {
        let mut c = collector();
        c.ingest(
            &AgentEvent::ToolCall {
                name: "Bash".into(),
                id: "call-1".into(),
                arguments: "{\"command\":\"ls\"}".into(),
                step: 0,
            },
            at(10),
        );
        c.ingest(
            &AgentEvent::ToolResult {
                id: "call-1".into(),
                name: "Bash".into(),
                output: "ok".into(),
                step: 0,
                is_error: false,
            },
            at(60),
        );
        let records = c.records();
        let tool = records.last().expect("tool record");
        assert_eq!(tool.name, "tool Bash");
        assert_eq!(tool.kind, ObsKind::Span);
        assert_eq!(tool.parent, Some(1));
        assert_eq!(
            attr(tool, &format!("{OBSERVATION_METADATA_PREFIX}duration_ms")),
            Some(&AttrValue::Int(50))
        );
        assert_eq!(tool.status, ObsStatus::Ok);
        assert!(matches!(
            attr(tool, ATTR_OBSERVATION_OUTPUT),
            Some(AttrValue::Str(s)) if s.starts_with("len=2 hash=")
        ));
    }

    #[test]
    fn failing_tool_marks_error_level_and_status() {
        let mut c = collector();
        c.ingest(
            &AgentEvent::ToolCall {
                name: "Read".into(),
                id: "c1".into(),
                arguments: "{}".into(),
                step: 0,
            },
            at(1),
        );
        c.ingest(
            &AgentEvent::ToolResult {
                id: "c1".into(),
                name: "Read".into(),
                output: "ERROR: nope".into(),
                step: 0,
                is_error: true,
            },
            at(2),
        );
        let tool = c.records().pop().expect("tool");
        assert_eq!(tool.status, ObsStatus::Error("tool error".into()));
        assert_eq!(
            attr(&tool, ATTR_OBSERVATION_LEVEL),
            Some(&AttrValue::Str("ERROR".into()))
        );
    }

    #[test]
    fn retries_become_span_events_and_metadata() {
        let mut c = collector();
        c.ingest(
            &AgentEvent::LlmRetry {
                step: 0,
                attempt: 2,
                wait_ms: 1500,
                reason: "rate_limited".into(),
            },
            at(3),
        );
        let step = &c.records()[1];
        assert_eq!(step.events.len(), 1);
        assert_eq!(step.events[0].name, "llm.retry");
        assert_eq!(step.events[0].time, at(3));
        assert_eq!(
            attr(step, &format!("{OBSERVATION_METADATA_PREFIX}llm_retries")),
            Some(&AttrValue::Int(1))
        );
    }

    #[test]
    fn compaction_events_are_counted_on_the_root() {
        let mut c = collector();
        c.ingest(
            &AgentEvent::Compacted {
                removed: 3,
                kept: 2,
                summary_chars: 100,
                step: 1,
            },
            at(1),
        );
        c.ingest(
            &AgentEvent::CompactionSkipped {
                step: 1,
                reason: crate::event::CompactionSkipReason::CircuitBreaker,
            },
            at(2),
        );
        let root = &c.records()[0];
        assert_eq!(
            attr(root, &format!("{TRACE_METADATA_PREFIX}compactions")),
            Some(&AttrValue::Int(2))
        );
    }

    #[test]
    fn microcompaction_is_not_counted_as_compaction() {
        let mut c = collector();
        c.ingest(&AgentEvent::Microcompact { step: 1, pruned: 5 }, at(1));
        let root = &c.records()[0];
        assert_eq!(
            attr(root, &format!("{TRACE_METADATA_PREFIX}compactions")),
            Some(&AttrValue::Int(0))
        );
    }

    #[test]
    fn multi_turn_runs_reuse_step_indices_per_turn() {
        let mut c = collector();
        c.ingest(&AgentEvent::Latency { step: 0, llm_ms: 1 }, at(1));
        c.ingest(
            &AgentEvent::TurnFinished {
                reason: "no_more_tool_calls".into(),
                steps: 1,
            },
            at(2),
        );
        c.ingest(&AgentEvent::Latency { step: 0, llm_ms: 2 }, at(3));
        let records = c.records();
        // root + two step records (step 0 turn 0, step 0 turn 1)
        assert_eq!(records.len(), 3);
        assert_eq!(
            attr(&records[1], &format!("{OBSERVATION_METADATA_PREFIX}turn")),
            Some(&AttrValue::Int(0))
        );
        assert_eq!(
            attr(&records[2], &format!("{OBSERVATION_METADATA_PREFIX}turn")),
            Some(&AttrValue::Int(1))
        );
    }

    #[test]
    fn no_redact_reports_plaintext_truncated() {
        let mut c = RunCollector::new(meta(), false, t0());
        c.ingest(
            &AgentEvent::AssistantText {
                text: "plain answer".into(),
                step: 0,
            },
            at(1),
        );
        let step = &c.records()[1];
        assert_eq!(
            attr(step, ATTR_OBSERVATION_OUTPUT),
            Some(&AttrValue::Str("plain answer".into()))
        );
    }

    #[test]
    fn user_message_becomes_redacted_run_input() {
        let mut c = collector();
        let message = crate::message::Message::user("top secret goal");
        c.ingest(
            &AgentEvent::MessageAppended {
                message,
                usage: None,
                step: None,
            },
            at(1),
        );
        let root = &c.records()[0];
        match attr(root, ATTR_OBSERVATION_INPUT) {
            Some(AttrValue::Str(s)) => {
                assert!(s.starts_with("len=15 hash="), "unexpected redaction: {s}");
                assert!(!s.contains("secret"), "plaintext leaked: {s}");
            }
            other => panic!("expected redacted input, got {other:?}"),
        }
    }

    #[test]
    fn cost_uses_bundled_pricing_for_known_model() {
        // deepseek-chat is in the bundled catalog; a billable usage must
        // produce a positive USD estimate on the root record.
        let mut c = collector();
        c.ingest(
            &AgentEvent::Usage {
                input_tokens: 1_000_000,
                output_tokens: 1_000_000,
                cache_hit_tokens: 0,
                cache_miss_tokens: 1_000_000,
                step: 0,
            },
            at(1),
        );
        let root = &c.records()[0];
        match attr(root, &format!("{OBSERVATION_METADATA_PREFIX}cost_usd")) {
            Some(AttrValue::Float(v)) => assert!(*v > 0.0, "expected positive cost, got {v}"),
            other => panic!("expected float cost, got {other:?}"),
        }
    }

    #[test]
    fn unknown_model_reports_zero_cost() {
        let mut m = RunMeta::new("s", "no-such-model-xyz", "p");
        m.trace_name = String::new();
        let mut c = RunCollector::new(m, true, t0());
        c.ingest(
            &AgentEvent::Usage {
                input_tokens: 10,
                output_tokens: 10,
                cache_hit_tokens: 0,
                cache_miss_tokens: 10,
                step: 0,
            },
            at(1),
        );
        let root = &c.records()[0];
        assert_eq!(root.name, "recursive.run");
        assert_eq!(
            attr(root, &format!("{OBSERVATION_METADATA_PREFIX}cost_usd")),
            Some(&AttrValue::Float(0.0))
        );
    }

    #[test]
    fn is_finished_tracks_terminal_state() {
        let mut c = collector();
        assert!(!c.is_finished());
        c.finish(Some("stuck"), None, at(9));
        assert!(c.is_finished());
    }
}
