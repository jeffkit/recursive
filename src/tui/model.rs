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
/// [`crate::tui::ui::modal::Modal::PlanReview`], so we are down to two
/// screens: the brief splash and the chat surface.
#[derive(Clone, Debug, PartialEq)]
pub enum AppScreen {
    Splash,
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
    ToolCall {
        id: String,
        name: String,
        args_preview: String,
    },
    ToolResult {
        id: String,
        name: String,
        success: bool,
        output: String,
        expanded: bool,
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
}
