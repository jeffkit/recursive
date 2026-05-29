//! UI-facing event and action types.
//!
//! [`UiEvent`] flows from the agent backend → UI thread; the UI applies
//! them to [`crate::app::App`] state. [`UserAction`] flows the other way
//! — the UI thread captures key events and the
//! [`crate::backend::Backend`] worker dispatches them onto the
//! `AgentRuntime`.
//!
//! Goal-144 widens this surface from goal-143's four variants to
//! consume seven extra `AgentEvent` flavours: streaming partial
//! tokens, completed assistant text, token usage, latency, transcript
//! compaction and id-paired tool call/result events.

/// Events bubbled up from the backend worker into the UI loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiEvent {
    /// A streamed partial token chunk to append to the in-flight
    /// assistant message.
    AssistantPartial { text: String },
    /// A completed assistant message (non-streaming providers, or the
    /// final flush after `PartialToken` chunks).
    AssistantMessage { content: String },
    /// Model requested to execute a tool. Carries the call id so the
    /// matching [`UiEvent::ToolResult`] can pair up.
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    /// Tool finished executing. `id` matches the originating
    /// [`UiEvent::ToolCall`].
    ToolResult {
        id: String,
        name: String,
        output: String,
        success: bool,
    },
    /// Token usage for the latest LLM call.
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    /// Latency (ms) of the latest LLM call.
    Latency { llm_ms: u64 },
    /// Transcript compaction notification.
    Compacted { removed: usize, kept: usize },
    /// Marks the end of a turn so the UI can stop the spinner.
    TurnFinished,
    /// A non-fatal error worth surfacing to the user.
    Error { message: String },
}

/// Actions originating from key events that the backend worker must
/// service against the [`recursive::AgentRuntime`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserAction {
    /// Send a user message and run one turn.
    SendMessage(String),
    /// Confirm the pending plan and resume execution.
    ConfirmPlan,
    /// Reject the pending plan with a free-form reason.
    RejectPlan(String),
    /// Tear down the worker and exit the runtime.
    Shutdown,
}
