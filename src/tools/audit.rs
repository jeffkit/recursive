//! Tool audit types: metadata recorded for every tool invocation.
//!
//! These types are used by [`super::dispatch::ToolDispatch`] and
//! [`super::registry::ToolRegistry`] to record side-effect
//! classification, timing, and exit status for each tool call.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

/// Classification of a tool's observable side-effects on state outside
/// the agent process. Used by orphan detection and safe-replay (g154) to
/// decide how aggressively to retry or skip an unfinished tool call.
///
/// Distinct from `crate::kernel::SideEffect`, which tracks background-job
/// scheduling; the two live in different modules and never collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSideEffect {
    /// No mutation of any state outside the agent process. Safe to
    /// replay at any time. Examples: `Read`, `Grep`,
    /// `recall`, `checkpoint_list`.
    ReadOnly,
    /// Modifies local state (filesystem, scratchpad) in an idempotent-
    /// friendly way. Examples: `Write`, `Edit`, `remember`.
    Mutating,
    /// Reaches out to the external world or triggers opaque side-effects.
    /// Cannot determine safe re-execution from local state alone.
    /// Examples: `Bash`, `Agent`, `schedule_wakeup`.
    /// **Default** for any tool that does not override `side_effect_class`.
    External,
}

/// Maximum length of the persisted error message in [`ExitStatus::Err`].
/// Anything longer is UTF-8 char-boundary clipped and `truncated` is set.
pub const AUDIT_ERR_MAX_BYTES: usize = 512;

#[inline]
fn is_false(b: &bool) -> bool {
    !b
}

/// Outcome of a single tool invocation, as recorded in [`AuditMeta`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExitStatus {
    Ok,
    Err {
        /// Error message, truncated to [`AUDIT_ERR_MAX_BYTES`] bytes.
        message: String,
        /// `true` when the original message was longer and was clipped.
        #[serde(default, skip_serializing_if = "is_false")]
        truncated: bool,
    },
}

/// Key type for [`AuditMeta`] maps, scoped by (turn, tool_call_id).
///
/// Each turn may reuse tool_call_ids (e.g. MockProvider recycles them),
/// so the turn index disambiguates collisions across turns.
pub type AuditKey = (u32 /* turn */, String /* tool_call_id */);

/// Per-call audit record returned by [`super::registry::ToolRegistry::invoke_with_audit`]
/// and stored in [`crate::session::TranscriptEntry::audit`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditMeta {
    /// UUIDv7 step identifier (time-ordered).
    pub step_id: String,
    /// Unix epoch millis at registry dispatch start.
    pub started_at: i64,
    /// Unix epoch millis when the tool returned.
    pub finished_at: i64,
    /// BLAKE3 of the canonical JSON of `arguments` (hex-encoded).
    /// Detects argument drift across resumes.
    pub args_hash: String,
    /// Side-effect class as reported by the tool at call time.
    pub side_effect: ToolSideEffect,
    /// Whether the tool returned `Ok` or `Err`.
    pub exit_status: ExitStatus,
}

impl AuditMeta {
    /// Synthetic `AuditMeta` for an unknown-tool dispatch (tool not in
    /// registry). Called when `invoke_with_audit` cannot find the tool.
    pub fn synthetic_unknown_tool(name: &str) -> Self {
        let now = unix_millis();
        Self {
            step_id: uuid::Uuid::now_v7().hyphenated().to_string(),
            started_at: now,
            finished_at: now,
            args_hash: String::new(),
            side_effect: ToolSideEffect::External,
            exit_status: ExitStatus::Err {
                message: format!("unknown tool: {name}"),
                truncated: false,
            },
        }
    }
}

/// Observer that records files touched by structured filesystem tools
/// during a single agent turn. Owned by `AgentRuntime` and reset at
/// every turn boundary; passed by `Arc<Mutex<...>>` to the
/// `ToolRegistry` so tool dispatch can record `path` arguments.
#[derive(Debug, Default, Clone)]
pub struct TouchedFiles {
    /// Workspace-relative file paths recorded from `Write`, `Edit`, etc.
    pub paths: HashSet<String>,
    /// True if the agent invoked `Bash` this turn — runtime will
    /// use a pre/post snapshot diff to attribute file changes.
    pub saw_shell: bool,
}

impl TouchedFiles {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && !self.saw_shell
    }
    pub fn paths_sorted(&self) -> Vec<String> {
        let mut v: Vec<_> = self.paths.iter().cloned().collect();
        v.sort();
        v
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────

pub(crate) fn unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Clip `s` to at most `AUDIT_ERR_MAX_BYTES` bytes on a UTF-8 char boundary.
/// Returns `(clipped, was_truncated)`.
pub(crate) fn truncate_for_audit(s: &str) -> (String, bool) {
    if s.len() <= AUDIT_ERR_MAX_BYTES {
        return (s.to_string(), false);
    }
    let mut end = AUDIT_ERR_MAX_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

/// BLAKE3 hash of the canonical JSON encoding of `v`.
pub(crate) fn blake3_canonical_json(v: &Value) -> String {
    let canonical = v.to_string();
    let hash = blake3::hash(canonical.as_bytes());
    hash.to_hex().to_string()
}
