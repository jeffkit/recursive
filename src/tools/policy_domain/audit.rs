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

#[cfg(test)]
mod tests {
    use super::*;

    // --- is_false (serde skip helper) ---

    #[test]
    fn is_false_returns_true_for_false() {
        assert!(is_false(&false), "is_false(&false) must be true");
    }

    #[test]
    fn is_false_returns_false_for_true() {
        assert!(!is_false(&true), "is_false(&true) must be false");
    }

    // --- AuditMeta::synthetic_unknown_tool ---

    #[test]
    fn synthetic_unknown_tool_has_reasonable_fields() {
        let meta = AuditMeta::synthetic_unknown_tool("my_tool");
        assert!(!meta.step_id.is_empty(), "step_id must be non-empty");
        assert!(meta.started_at > 0, "started_at must be positive");
        assert_eq!(meta.side_effect, ToolSideEffect::External);
        match &meta.exit_status {
            ExitStatus::Err { message, .. } => {
                assert!(
                    message.contains("my_tool"),
                    "error message must contain tool name"
                );
            }
            ExitStatus::Ok => panic!("unknown tool must produce ExitStatus::Err"),
        }
    }

    // --- TouchedFiles::is_empty ---

    #[test]
    fn touched_files_empty_paths_no_shell_is_empty() {
        let tf = TouchedFiles::new();
        assert!(tf.is_empty(), "no paths + no shell must be empty");
    }

    #[test]
    fn touched_files_with_path_is_not_empty() {
        let mut tf = TouchedFiles::new();
        tf.paths.insert("src/main.rs".into());
        assert!(!tf.is_empty(), "non-empty paths must not be empty");
    }

    #[test]
    fn touched_files_saw_shell_is_not_empty() {
        let mut tf = TouchedFiles::new();
        tf.saw_shell = true;
        assert!(
            !tf.is_empty(),
            "saw_shell=true must not be empty (kills &&→|| mutant)"
        );
    }

    // --- TouchedFiles::paths_sorted ---

    #[test]
    fn paths_sorted_returns_sorted_vec() {
        let mut tf = TouchedFiles::new();
        tf.paths.insert("z.rs".into());
        tf.paths.insert("a.rs".into());
        tf.paths.insert("m.rs".into());
        let sorted = tf.paths_sorted();
        assert_eq!(sorted, vec!["a.rs", "m.rs", "z.rs"]);
    }

    #[test]
    fn paths_sorted_on_empty_is_empty_vec() {
        let tf = TouchedFiles::new();
        assert!(tf.paths_sorted().is_empty());
    }

    // --- unix_millis ---

    #[test]
    fn unix_millis_is_positive() {
        let ms = unix_millis();
        assert!(
            ms > 0,
            "unix_millis must return a positive timestamp; got {ms}"
        );
        // Sanity: must be after 2024-01-01 (1704067200000 ms)
        assert!(
            ms > 1_704_067_200_000,
            "unix_millis must be after 2024-01-01"
        );
    }

    // --- truncate_for_audit ---

    #[test]
    fn truncate_short_string_unchanged() {
        let short = "hello";
        let (out, truncated) = truncate_for_audit(short);
        assert_eq!(out, short);
        assert!(!truncated, "short string must not be truncated");
    }

    #[test]
    fn truncate_long_string_is_clipped_and_flagged() {
        let long = "x".repeat(AUDIT_ERR_MAX_BYTES + 10);
        let (out, truncated) = truncate_for_audit(&long);
        assert!(truncated, "long string must be flagged as truncated");
        assert_eq!(out.len(), AUDIT_ERR_MAX_BYTES);
    }

    #[test]
    fn truncate_exactly_max_bytes_not_truncated() {
        let exact = "a".repeat(AUDIT_ERR_MAX_BYTES);
        let (out, truncated) = truncate_for_audit(&exact);
        assert!(!truncated, "exactly max bytes must NOT be truncated");
        assert_eq!(out.len(), AUDIT_ERR_MAX_BYTES);
    }

    // --- blake3_canonical_json ---

    #[test]
    fn blake3_canonical_json_is_nonempty_and_not_placeholder() {
        let hash = blake3_canonical_json(&serde_json::json!({"key": "value"}));
        assert!(!hash.is_empty(), "blake3 hash must not be empty");
        assert_ne!(hash, "xyzzy", "blake3 hash must not be placeholder");
    }

    #[test]
    fn blake3_canonical_json_differs_for_different_inputs() {
        let h1 = blake3_canonical_json(&serde_json::json!({"a": 1}));
        let h2 = blake3_canonical_json(&serde_json::json!({"a": 2}));
        assert_ne!(h1, h2, "different inputs must produce different hashes");
    }
}
