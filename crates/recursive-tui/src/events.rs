//! UI-facing event and action types.
//!
//! [`UiEvent`] flows from the agent backend → UI thread; the UI applies
//! them to [`crate::app::App`] state. [`UserAction`] flows the other way
//! — the UI thread captures key events and the
//! [`crate::backend::Backend`] worker dispatches them onto the
//! `AgentRuntime`.
//!
//! This step (goal-143) only mirrors the four event variants the
//! pre-revamp TUI already consumed; later goals will widen the surface
//! to cover all 11 [`recursive::AgentEvent`] variants.

/// Events bubbled up from the backend worker into the UI loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiEvent {
    /// Model asked for a tool to run.
    ToolCall { name: String },
    /// Tool finished executing.
    ToolResult { name: String, success: bool },
    /// Model produced a final assistant message.
    AssistantMessage { content: String },
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
