//! Event-sink abstraction for consuming agent lifecycle events.
// This module bridges StepEvent → AgentEvent; allow use of the deprecated type.
#![allow(deprecated)]
//!
//! This module defines [`AgentEvent`] (a serialisable, non-exhaustive enum of
//! agent lifecycle events), the [`EventSink`] trait (the abstract consumer),
//! and four concrete implementations:
//!
//! * [`ChannelSink`] — delivers events into a `tokio::sync::mpsc` channel.
//! * [`BroadcastSink`] — delivers events into a `tokio::sync::broadcast` channel.
//! * [`NullSink`] — discards every event (no-op).
//! * [`CompositeSink`] — fans out to multiple inner sinks.
//!
//! # Bridge from `StepEvent`
//!
//! `AgentEvent` implements `From<StepEvent>`, so any `StepEvent` produced by
//! the agent loop can be converted into an `AgentEvent` for downstream
//! consumers.

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio::sync::mpsc;

use crate::agent::StepEvent;

// ---------------------------------------------------------------------------
// AgentEvent
// ---------------------------------------------------------------------------

/// A serialisable, non-exhaustive agent lifecycle event.
///
/// This enum mirrors the variants of [`StepEvent`] but uses `String` for the
/// finish reason to avoid coupling to the `FinishReason` type.  New variants
/// can be added without a breaking change (see `#[non_exhaustive]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AgentEvent {
    /// Model generated text without tool calls.
    AssistantText { text: String, step: usize },
    /// Model requested to execute a tool.
    ToolCall {
        name: String,
        id: String,
        arguments: String,
        step: usize,
    },
    /// Time taken for the LLM request (excluding tool execution), in ms.
    Latency { step: usize, llm_ms: u64 },
    /// Result of executing a tool call.
    ToolResult {
        id: String,
        name: String,
        output: String,
        step: usize,
    },
    /// Token usage statistics from the LLM provider.
    Usage {
        input_tokens: u32,
        output_tokens: u32,
        step: usize,
    },
    /// Partial token from streaming response (if streaming enabled).
    PartialToken { text: String, step: usize },
    /// Transcript was compacted to fit size constraints.
    Compacted {
        removed: usize,
        kept: usize,
        summary_chars: usize,
        step: usize,
    },
    /// Agent run completed.
    TurnFinished {
        /// Human-readable reason for termination (e.g. "no_more_tool_calls").
        reason: String,
        /// Number of iterations executed.
        steps: usize,
    },
    /// Agent has produced a plan and is waiting for confirmation.
    PlanProposed {
        plan_text: String,
        tool_calls: Vec<serde_json::Value>,
    },
    /// Plan was confirmed, execution will proceed.
    PlanConfirmed,
    /// Plan was rejected with a reason.
    PlanRejected { reason: String },

    /// A complete message was just appended to the agent transcript.
    ///
    /// Fired exactly once per committed message inside the agent runtime:
    /// once for the user message that starts a turn, once for the compaction
    /// summary if cross-turn compaction fires, and once per message in the
    /// kernel's output batch. Carries the full `Message` (role, content,
    /// tool_calls, tool_call_id, reasoning_content) so persistence consumers
    /// can write the canonical record without reassembling it from the finer
    /// `AssistantText` / `ToolCall` / `ToolResult` streaming events.
    ///
    /// `parent_uuid` — if `Some`, the emitter wants this message to be written
    /// as a branch off the given UUID rather than the SessionWriter's internal
    /// chain pointer. Used by subagent runtimes (g155).
    ///
    /// `usage` — token usage for this message (non-None for assistant messages
    /// produced by an LLM call, g156).
    ///
    /// Not emitted for seeded transcript messages loaded from an existing
    /// session on resume (those are already on disk).
    MessageAppended {
        message: crate::message::Message,
        /// Explicit parent UUID override for subagent branch points (g155).
        parent_uuid: Option<String>,
        /// Token usage for this message (g156).
        usage: Option<crate::session::UsageMeta>,
    },

    /// Variant of [`MessageAppended`] specifically for `Role::Tool` messages
    /// that have an associated [`AuditMeta`] (Goal 153). The persistence sink
    /// handles this identically to `MessageAppended` but populates the
    /// `audit` field of [`crate::session::TranscriptEntry`].
    ///
    /// Emitting a separate variant (rather than `Option<AuditMeta>` on
    /// `MessageAppended`) keeps the common path zero-cost and avoids
    /// making audit an optional field on every event.
    MessageAppendedWithAudit {
        message: crate::message::Message,
        audit: crate::tools::AuditMeta,
    },

    /// Cross-turn compaction just fired; a compact_boundary marker should be
    /// written to the session JSONL (g157).
    ///
    /// `turn` — the turn index when compaction occurred.
    /// `compacted_count` — how many messages were removed.
    /// `summary_uuid` — UUID of the compaction summary message that replaced them.
    CompactionBoundary {
        turn: u32,
        compacted_count: usize,
        summary_uuid: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// From<StepEvent>
// ---------------------------------------------------------------------------

impl From<StepEvent> for AgentEvent {
    fn from(ev: StepEvent) -> Self {
        match ev {
            StepEvent::AssistantText { text, step } => AgentEvent::AssistantText { text, step },
            StepEvent::ToolCall { call, step } => AgentEvent::ToolCall {
                name: call.name,
                id: call.id,
                arguments: call.arguments.to_string(),
                step,
            },
            StepEvent::Latency { step, llm_ms } => AgentEvent::Latency { step, llm_ms },
            StepEvent::ToolResult {
                id,
                name,
                output,
                step,
            } => AgentEvent::ToolResult {
                id,
                name,
                output,
                step,
            },
            StepEvent::Usage { usage, step } => AgentEvent::Usage {
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
                step,
            },
            StepEvent::PartialToken { text, step } => AgentEvent::PartialToken { text, step },
            StepEvent::Compacted {
                removed,
                kept,
                summary_chars,
                step,
            } => AgentEvent::Compacted {
                removed,
                kept,
                summary_chars,
                step,
            },
            StepEvent::Finished { reason: _, steps } => AgentEvent::TurnFinished {
                reason: "finished".into(),
                steps,
            },
            StepEvent::PlanProposed {
                plan_text,
                tool_calls,
            } => AgentEvent::PlanProposed {
                plan_text,
                tool_calls: tool_calls
                    .into_iter()
                    .map(|tc| {
                        serde_json::json!({
                            "name": tc.name,
                            "id": tc.id,
                            "arguments": tc.arguments,
                        })
                    })
                    .collect(),
            },
            StepEvent::PlanConfirmed => AgentEvent::PlanConfirmed,
            StepEvent::PlanRejected { reason } => AgentEvent::PlanRejected { reason },
        }
    }
}

// ---------------------------------------------------------------------------
// EventSink trait
// ---------------------------------------------------------------------------

/// Abstract consumer of [`AgentEvent`]s.
///
/// Implementations are free to buffer, forward, filter, or discard events.
/// The trait is designed to be object-safe so that a single `Box<dyn EventSink>`
/// can hold any implementation.
#[async_trait::async_trait]
pub trait EventSink: Send + Sync + 'static {
    /// Deliver one event to this sink.
    ///
    /// Implementations should not panic. If the sink is full or closed, the
    /// event may be silently dropped (or an error logged).
    async fn emit(&self, event: AgentEvent);
}

// ---------------------------------------------------------------------------
// ChannelSink
// ---------------------------------------------------------------------------

/// An [`EventSink`] that sends events into a `tokio::sync::mpsc` channel.
///
/// If the channel is full (respects the bounded capacity) the event is
/// silently dropped.
pub struct ChannelSink {
    tx: mpsc::UnboundedSender<AgentEvent>,
}

impl ChannelSink {
    /// Create a new `ChannelSink` and return it together with the receiver
    /// half.
    pub fn new() -> (Self, mpsc::UnboundedReceiver<AgentEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx }, rx)
    }
}

#[async_trait::async_trait]
impl EventSink for ChannelSink {
    async fn emit(&self, event: AgentEvent) {
        let _ = self.tx.send(event);
    }
}

// ---------------------------------------------------------------------------
// BroadcastSink
// ---------------------------------------------------------------------------

/// An [`EventSink`] that sends events into a `tokio::sync::broadcast` channel.
///
/// If all receivers are lagging behind the channel capacity, the oldest
/// events are dropped (standard broadcast behaviour).
pub struct BroadcastSink {
    tx: broadcast::Sender<AgentEvent>,
}

impl BroadcastSink {
    /// Create a new `BroadcastSink` with the given channel capacity.
    pub fn new(capacity: usize) -> (Self, broadcast::Receiver<AgentEvent>) {
        let (tx, rx) = broadcast::channel(capacity);
        (Self { tx }, rx)
    }
}

#[async_trait::async_trait]
impl EventSink for BroadcastSink {
    async fn emit(&self, event: AgentEvent) {
        let _ = self.tx.send(event);
    }
}

// ---------------------------------------------------------------------------
// NullSink
// ---------------------------------------------------------------------------

/// An [`EventSink`] that discards every event.
pub struct NullSink;

#[async_trait::async_trait]
impl EventSink for NullSink {
    async fn emit(&self, _event: AgentEvent) {
        // no-op
    }
}

// ---------------------------------------------------------------------------
// CompositeSink
// ---------------------------------------------------------------------------

/// An [`EventSink`] that fans out to multiple inner sinks.
///
/// Each inner sink receives every event. If any sink panics, the panic
/// propagates (i.e. there is no `catch_unwind` barrier).
pub struct CompositeSink {
    sinks: Vec<Box<dyn EventSink>>,
}

impl CompositeSink {
    /// Create a new `CompositeSink` from an iterator of sinks.
    pub fn new(sinks: impl IntoIterator<Item = Box<dyn EventSink>>) -> Self {
        Self {
            sinks: sinks.into_iter().collect(),
        }
    }
}

#[async_trait::async_trait]
impl EventSink for CompositeSink {
    async fn emit(&self, event: AgentEvent) {
        for sink in &self.sinks {
            sink.emit(event.clone()).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::FinishReason;
    use crate::llm::ToolCall as LlmToolCall;

    // -- ChannelSink -------------------------------------------------------

    #[tokio::test]
    async fn channel_sink_delivers_events() {
        let (sink, mut rx) = ChannelSink::new();
        let event = AgentEvent::AssistantText {
            text: "hello".into(),
            step: 0,
        };

        sink.emit(event.clone()).await;
        let received = rx.recv().await.unwrap();
        assert_eq!(received, event);
    }

    // -- BroadcastSink -----------------------------------------------------

    #[tokio::test]
    async fn broadcast_sink_delivers_to_multiple() {
        let (sink, mut rx1) = BroadcastSink::new(16);
        let mut rx2 = sink.tx.subscribe();

        let event = AgentEvent::PlanConfirmed;
        sink.emit(event.clone()).await;

        assert_eq!(rx1.recv().await.unwrap(), event);
        assert_eq!(rx2.recv().await.unwrap(), event);
    }

    // -- NullSink ----------------------------------------------------------

    #[tokio::test]
    async fn null_sink_does_not_panic() {
        let sink = NullSink;
        sink.emit(AgentEvent::PlanConfirmed).await;
        // If we reach here, the test passes.
    }

    // -- CompositeSink -----------------------------------------------------

    #[tokio::test]
    async fn composite_sink_fans_out() {
        let (sink1, mut rx1) = ChannelSink::new();
        let (sink2, mut rx2) = ChannelSink::new();

        let composite = CompositeSink::new(vec![
            Box::new(sink1) as Box<dyn EventSink>,
            Box::new(sink2) as Box<dyn EventSink>,
        ]);

        let event = AgentEvent::PlanConfirmed;
        composite.emit(event.clone()).await;

        assert_eq!(rx1.recv().await.unwrap(), event);
        assert_eq!(rx2.recv().await.unwrap(), event);
    }

    // -- From<StepEvent> conversion ----------------------------------------

    #[test]
    fn step_event_to_agent_event_conversion() {
        // AssistantText
        let se = StepEvent::AssistantText {
            text: "hi".into(),
            step: 1,
        };
        let ae: AgentEvent = se.into();
        assert_eq!(
            ae,
            AgentEvent::AssistantText {
                text: "hi".into(),
                step: 1
            }
        );

        // ToolCall
        let se = StepEvent::ToolCall {
            call: LlmToolCall {
                name: "get_weather".into(),
                id: "call_1".into(),
                arguments: serde_json::json!({"city": "NYC"}),
            },
            step: 2,
        };
        let ae: AgentEvent = se.into();
        assert_eq!(
            ae,
            AgentEvent::ToolCall {
                name: "get_weather".into(),
                id: "call_1".into(),
                arguments: r#"{"city":"NYC"}"#.into(),
                step: 2
            }
        );

        // Latency
        let se = StepEvent::Latency {
            step: 3,
            llm_ms: 150,
        };
        let ae: AgentEvent = se.into();
        assert_eq!(
            ae,
            AgentEvent::Latency {
                step: 3,
                llm_ms: 150
            }
        );

        // ToolResult
        let se = StepEvent::ToolResult {
            id: "call_1".into(),
            name: "get_weather".into(),
            output: "sunny".into(),
            step: 4,
        };
        let ae: AgentEvent = se.into();
        assert_eq!(
            ae,
            AgentEvent::ToolResult {
                id: "call_1".into(),
                name: "get_weather".into(),
                output: "sunny".into(),
                step: 4
            }
        );

        // Usage
        let se = StepEvent::Usage {
            usage: crate::llm::TokenUsage {
                prompt_tokens: 10,
                completion_tokens: 20,
                ..Default::default()
            },
            step: 5,
        };
        let ae: AgentEvent = se.into();
        assert_eq!(
            ae,
            AgentEvent::Usage {
                input_tokens: 10,
                output_tokens: 20,
                step: 5
            }
        );

        // PartialToken
        let se = StepEvent::PartialToken {
            text: "hel".into(),
            step: 6,
        };
        let ae: AgentEvent = se.into();
        assert_eq!(
            ae,
            AgentEvent::PartialToken {
                text: "hel".into(),
                step: 6
            }
        );

        // Compacted
        let se = StepEvent::Compacted {
            removed: 5,
            kept: 3,
            summary_chars: 200,
            step: 7,
        };
        let ae: AgentEvent = se.into();
        assert_eq!(
            ae,
            AgentEvent::Compacted {
                removed: 5,
                kept: 3,
                summary_chars: 200,
                step: 7
            }
        );

        // Finished -> TurnFinished
        let se = StepEvent::Finished {
            reason: FinishReason::NoMoreToolCalls,
            steps: 8,
        };
        let ae: AgentEvent = se.into();
        assert_eq!(
            ae,
            AgentEvent::TurnFinished {
                reason: "finished".into(),
                steps: 8
            }
        );

        // PlanProposed
        let se = StepEvent::PlanProposed {
            plan_text: "do stuff".into(),
            tool_calls: vec![],
        };
        let ae: AgentEvent = se.into();
        assert_eq!(
            ae,
            AgentEvent::PlanProposed {
                plan_text: "do stuff".into(),
                tool_calls: vec![]
            }
        );

        // PlanConfirmed
        let se = StepEvent::PlanConfirmed;
        let ae: AgentEvent = se.into();
        assert_eq!(ae, AgentEvent::PlanConfirmed);

        // PlanRejected
        let se = StepEvent::PlanRejected {
            reason: "bad plan".into(),
        };
        let ae: AgentEvent = se.into();
        assert_eq!(
            ae,
            AgentEvent::PlanRejected {
                reason: "bad plan".into()
            }
        );
    }

    // -- Serialisation round-trip ------------------------------------------

    #[test]
    fn agent_event_serialization_round_trip() {
        let events = vec![
            AgentEvent::AssistantText {
                text: "hello".into(),
                step: 0,
            },
            AgentEvent::ToolCall {
                name: "foo".into(),
                id: "1".into(),
                arguments: "{}".into(),
                step: 1,
            },
            AgentEvent::Latency {
                step: 2,
                llm_ms: 100,
            },
            AgentEvent::ToolResult {
                id: "1".into(),
                name: "foo".into(),
                output: "ok".into(),
                step: 3,
            },
            AgentEvent::Usage {
                input_tokens: 10,
                output_tokens: 20,
                step: 4,
            },
            AgentEvent::PartialToken {
                text: "hel".into(),
                step: 5,
            },
            AgentEvent::Compacted {
                removed: 5,
                kept: 3,
                summary_chars: 200,
                step: 6,
            },
            AgentEvent::TurnFinished {
                reason: "done".into(),
                steps: 7,
            },
            AgentEvent::PlanProposed {
                plan_text: "plan".into(),
                tool_calls: vec![],
            },
            AgentEvent::PlanConfirmed,
            AgentEvent::PlanRejected {
                reason: "nope".into(),
            },
        ];

        for event in &events {
            let json = serde_json::to_string(event).unwrap();
            let deserialized: AgentEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(*event, deserialized, "round-trip failed for {json}");
        }
    }

    // -- MessageAppended unit tests ----------------------------------------

    /// `MessageAppended` carries all three fields (content, tool_calls,
    /// reasoning_content) through a JSON round-trip intact.
    #[test]
    fn message_appended_round_trips_through_event() {
        use crate::llm::ToolCall as LlmToolCall;
        use crate::message::Message;

        let tc = LlmToolCall {
            id: "call_1".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "/tmp/foo"}),
        };
        let msg = Message {
            role: crate::message::Role::Assistant,
            content: "some text".into(),
            tool_calls: vec![tc.clone()],
            tool_call_id: None,
            reasoning_content: Some("my reasoning".into()),
        };
        let event = AgentEvent::MessageAppended {
            message: msg.clone(),
            parent_uuid: None,
            usage: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: AgentEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, deserialized);
        if let AgentEvent::MessageAppended { message: m, .. } = deserialized {
            assert_eq!(m.content, "some text");
            assert_eq!(m.reasoning_content.as_deref(), Some("my reasoning"));
            assert_eq!(m.tool_calls.len(), 1);
            assert_eq!(m.tool_calls[0].name, "read_file");
        } else {
            panic!("deserialized to wrong variant");
        }
    }

    /// `CompositeSink` fans out `MessageAppended` to both inner sinks.
    #[tokio::test]
    async fn composite_sink_preserves_message_appended() {
        use crate::message::Message;

        let (sink1, mut rx1) = ChannelSink::new();
        let (sink2, mut rx2) = ChannelSink::new();
        let composite = CompositeSink::new(vec![
            Box::new(sink1) as Box<dyn EventSink>,
            Box::new(sink2) as Box<dyn EventSink>,
        ]);

        let msg = Message::user("hello");
        let event = AgentEvent::MessageAppended {
            message: msg.clone(),
            parent_uuid: None,
            usage: None,
        };
        composite.emit(event.clone()).await;

        let got1 = rx1.recv().await.unwrap();
        let got2 = rx2.recv().await.unwrap();
        assert_eq!(got1, event);
        assert_eq!(got2, event);

        // Other variant also propagates.
        composite.emit(AgentEvent::PlanConfirmed).await;
        assert_eq!(rx1.recv().await.unwrap(), AgentEvent::PlanConfirmed);
        assert_eq!(rx2.recv().await.unwrap(), AgentEvent::PlanConfirmed);
    }
}
