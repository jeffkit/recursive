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
    /// Goal-147: structured plan proposal from the runtime
    /// (`AgentEvent::PlanProposed`). The UI opens a `Modal::PlanReview`
    /// over the chat screen and freezes input until the user
    /// approves / rejects / edits.
    ///
    /// `tool_calls` carries the pending tool calls as JSON values
    /// — each one has `name`, `id`, and `arguments` fields, mirroring
    /// the kernel's serialised `StepEvent::PlanProposed` payload.
    PlanProposed {
        plan_text: String,
        tool_calls: Vec<serde_json::Value>,
    },
    /// Goal-147: the runtime accepted the plan and resumed execution.
    /// Closes any open `Modal::PlanReview` and pushes a System block.
    PlanConfirmed,
    /// Goal-147: the runtime rejected (or had its plan rejected) with
    /// a free-form `reason`. Same UI handling as `PlanConfirmed` plus
    /// the reason in the System block.
    PlanRejected { reason: String },
}

/// Actions originating from key events that the backend worker must
/// service against the [`recursive::AgentRuntime`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserAction {
    /// Send a user message and run one turn.
    SendMessage(String),
    /// Run a shell command directly via the runtime's tool registry,
    /// bypassing the LLM. The command is dispatched to the
    /// `run_shell` tool and its result surfaces as a
    /// [`UiEvent::ToolCall`] + [`UiEvent::ToolResult`] pair, but is
    /// **not** appended to the runtime transcript.
    ///
    /// Goal-145: this powers the `!`-prefixed bash mode of the
    /// PromptInput so users get a quick scratch shell without
    /// polluting the agent dialogue.
    RunShell(String),
    /// Confirm the pending plan and resume execution.
    ConfirmPlan,
    /// Reject the pending plan with a free-form reason.
    RejectPlan(String),
    /// Goal-146: trigger a transcript compaction pass via
    /// [`AgentRuntime::compact_now`]. The worker pushes a
    /// `Compacted` event when summarisation succeeds.
    Compact,
    /// Goal-146: flip the runtime's planning mode. `true` enables
    /// plan-first mode, `false` reverts to immediate execution.
    /// The worker echoes a `System` block confirming the new state.
    SetPlanningMode(bool),
    /// Goal-147: signal the worker to abort the in-flight turn.
    /// The worker flips its `cancel_flag`, and any `tokio::select!`
    /// waiting on `wait_for_cancel` returns immediately. The runtime
    /// is *not* cancelled mid-HTTP-request (reqwest doesn't support
    /// that); on the next tool-call boundary the next turn will
    /// surface as a `UiEvent::Error { message: "interrupted" }`.
    Interrupt,
    /// Tear down the worker and exit the runtime.
    Shutdown,
}
