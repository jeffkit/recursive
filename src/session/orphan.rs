//! Orphan tool-call detection types (Goal 153).
//!
//! Describes a tool call that was dispatched but never completed
//! (no matching `tool` result message in the transcript).
//!
//! Split from `session.rs` during the Goal 221 module refactor.

/// Result content that stands in for an orphan tool call the caller chose
/// *not* to re-execute. It answers the call so the resumed seed stays a
/// valid tool-call/tool-result chain (invariant #8) instead of hitting the
/// provider's "tool_use without tool_result" HTTP 400.
pub const ORPHAN_SKIPPED_RESULT: &str = "[interrupted: no result recorded]";

/// Prefix on the result content of an orphan tool call whose re-execution
/// failed. The error text is appended after it so the model can react
/// instead of the resume dying on the spot.
pub const ORPHAN_REDO_FAILED_PREFIX: &str = "[interrupted: redo failed] ";

/// Goal-153: describes a tool call that was dispatched but never completed
/// (no matching `tool` result message in the transcript).
#[derive(Debug, Clone)]
pub struct OrphanToolCall {
    /// `id` field of the assistant `TranscriptEntry` that issued the call.
    pub assistant_msg_id: String,
    /// The `id` of the tool call itself (matches `tool_call_id` on the
    /// expected — but missing — tool result message).
    pub tool_call_id: String,
    /// The name of the tool that was called.
    pub tool_name: String,
    /// The original call, verbatim from the transcript. Carries the
    /// arguments needed to re-execute the call under `--orphans=redo`.
    pub call: crate::llm::ToolCall,
    /// BLAKE3 of canonical JSON of the call arguments (for drift detection).
    pub args_hash: String,
    /// Side-effect class, determined from the current registry. Falls back
    /// to `External` when the tool is no longer registered (registry
    /// drifted since the session was saved) so `redo` always shows the
    /// conservative warning.
    pub side_effect_at_call: crate::tools::ToolSideEffect,
}
