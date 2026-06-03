//! Transcript data model types for the Recursive TUI.
//!
//! Contains the screen enum and the types used to represent rendered
//! transcript blocks (user messages, assistant replies, tool calls/results,
//! diffs, etc.).

// ──────────────────────────────────────────────────────────────────────
// Screens
// ──────────────────────────────────────────────────────────────────────

/// Which top-level screen is currently rendered.
///
/// Goal 147 removed the `PlanReview` variant — the plan-mode
/// confirmation now lives on the modal stack as
/// [`crate::tui::ui::modal::Modal::PlanReview`], so we are down to one
/// screen: the chat surface. The splash screen was replaced by a
/// startup banner printed to stdout before the inline TUI starts.
#[derive(Clone, Debug, PartialEq)]
pub enum AppScreen {
    Chat,
}

// ──────────────────────────────────────────────────────────────────────
// Transcript model
// ──────────────────────────────────────────────────────────────────────

/// One unit of context within a [`Diff`] block.
///
/// We model only the three line kinds we need to colour-code: addition,
/// removal, and unchanged context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffLineKind {
    Add,
    Remove,
    Context,
}

/// A single line inside a diff hunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub text: String,
}

/// A grouped sequence of diff lines belonging to one logical change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffHunk {
    pub lines: Vec<DiffLine>,
}

/// Result payload of a tool execution. Lives inside a
/// [`TranscriptBlock::ToolCall`] once the runtime delivers the
/// matching `UiEvent::ToolResult`.
///
/// The UI used to keep `ToolCall` and `ToolResult` as two separate
/// blocks, but Claude-Code-style renderings pair them into a single
/// "function call" unit: one bullet, then the result on the next
/// line. `None` on the call side means the tool is still running.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolResultData {
    pub success: bool,
    pub output: String,
    /// `false` = collapsed (first 3 lines + "… N more" hint),
    /// `true` = full output. Toggled by Ctrl+E on the chat
    /// surface when the input buffer is empty.
    pub expanded: bool,
}

/// One renderable transcript block.
///
/// The chat screen renders a `Vec<TranscriptBlock>` in order, with one
/// blank line between adjacent blocks. Each variant has a corresponding
/// renderer in [`crate::tui::ui::transcript`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TranscriptBlock {
    User {
        text: String,
    },
    Assistant {
        text: String,
        streaming: bool,
        latency_ms: Option<u64>,
    },
    /// Reasoning / thinking content for a step. Emitted by
    /// [`UiEvent::Reasoning`] before the matching assistant text
    /// block; rendered inline as a `thinking…` header followed by
    /// the reasoning text in dim grey italics.
    Reasoning {
        text: String,
    },
    /// Tool call (paired with its result once available).
    ///
    /// While the tool is still running, `result` is `None`; the
    /// renderer shows the call in a "running" state (yellow ⏺,
    /// `Running…` placeholder). When the runtime pushes the
    /// matching `UiEvent::ToolResult`, the field is filled in.
    ToolCall {
        id: String,
        name: String,
        args_preview: String,
        result: Option<ToolResultData>,
    },
    Diff {
        path: String,
        hunks: Vec<DiffHunk>,
    },
    Compacted {
        removed: usize,
        kept: usize,
    },
    System {
        text: String,
    },
    Error {
        text: String,
    },
    /// Goal-E: plan-mode proposal rendered inline in the transcript.
    /// Replaces the `Modal::PlanReview` pop-up so the plan is visible
    /// in the message stream without obscuring prior context.
    PlanProposal {
        plan_text: String,
        tool_calls: Vec<serde_json::Value>,
    },
    /// Goal-202: plan-mode entry request rendered inline in the transcript.
    /// Agent called `request_plan_mode`; user should approve or skip.
    PlanModeRequest {
        reason: String,
        /// Set to `Some(true/false)` after the user decides.
        approved: Option<bool>,
    },
    /// Incoming message from a WeChat user, forwarded through the iLink channel.
    /// Rendered with a 📱 prefix so it is visually distinct from local TUI input.
    #[cfg(feature = "weixin")]
    WeixinMessage {
        user_id: String,
        text: String,
    },
}
