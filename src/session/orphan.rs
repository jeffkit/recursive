//! Orphan tool-call detection types (Goal 153).
//!
//! Describes a tool call that was dispatched but never completed
//! (no matching `tool` result message in the transcript).
//!
//! Split from `session.rs` during the Goal 221 module refactor.

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
    /// BLAKE3 of canonical JSON of the call arguments (for drift detection).
    pub args_hash: String,
    /// Side-effect class, determined from the current registry (valid because
    /// `recursive resume` validates the registry hash before calling this).
    pub side_effect_at_call: crate::tools::ToolSideEffect,
}
