//! Session files for production pause/resume.
//!
//! A `SessionFile` captures enough agent state to resume a run that
//! terminated abnormally (budget exceeded, stuck, transcript limit).
//! It stores the goal, model, provider, a hash of the tool registry,
//! the steps consumed so far, and the full transcript.
//!
//! Sessions are written alongside transcripts when `--session-out` is
//! set and the finish reason is non-success. They live under
//! `<workspace>/.recursive/sessions/` by convention.
//!
//! Split into sub-modules during the Goal 221 module refactor:
//! - `serialize` — `TranscriptEntry`, `entry_to_message`
//! - `lifecycle` — `SessionLock`, `truncate_transcript_to_turn`
//! - `orphan`   — `OrphanToolCall`
//! - `reader`   — `SessionReader`
//! - `writer`   — `SessionWriter`, `SessionPersistenceSink`

pub mod lifecycle;
pub mod orphan;
pub mod reader;
pub mod serialize;
pub mod writer;

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::llm::ToolSpec;
use crate::message::Message;

// Re-exports from sub-modules (keep the 8 `pub use` in lib.rs working)
pub use lifecycle::{truncate_transcript_to_turn, SessionLock, SessionLockBusy, TruncateStats};
pub use orphan::OrphanToolCall;
pub use reader::SessionReader;
pub use serialize::{entry_to_message, TranscriptEntry};
pub use writer::{SessionPersistenceSink, SessionWriter};

/// Current schema version for session files.
/// Increment when the format changes in a breaking way.
const SESSION_SCHEMA_VERSION: u32 = 1;

/// Maximum `schema_version` this build of the binary can interpret.
/// `SessionReader::load_meta` rejects anything strictly greater
/// than this. Bump this constant (and the helper below) when
/// making a non-backward-compatible change to `SessionMeta`.
pub(crate) const SUPPORTED_SESSION_SCHEMA_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// SessionStatus
// ---------------------------------------------------------------------------

/// Status of a session. Persisted as a lowercase string.
///
/// Backward compatibility has THREE complementary mechanisms, all
/// required for this enum to load files written by every prior build:
///
/// 1. **`#[serde(alias = ...)]` on individual variants** — accepts
///    legacy on-disk strings that map to the same semantic state.
///    For example, `Completed` accepts both `"completed"` (current)
///    and `"success"` (the string the pre-Goal-276 writer emitted).
///    Without these aliases, old session files would silently load
///    as `Active` (the `other` catch-all) instead of `Completed`.
///
/// 2. **`#[serde(other)]` on the last variant (`Active`)** — catches
///    any *unknown* string the build does not recognise (e.g.
///    `"stalled"` written by a future version, or a typo). It MUST
///    stay last; serde requires the catch-all at the end.
///
/// 3. **`#[serde(default)]` on the field in `SessionMeta`** (see
///    below) — handles a *missing* `status` field on disk. Without
///    it, a `.meta.json` written before the field existed would
///    fail to load.
///
/// Aliases handle **known** legacy values and preserve semantics;
/// `#[serde(other)]` handles **unknown** values and degrades safely
/// to `Active`; the field-level `#[serde(default)]` handles a
/// **missing** field. They are distinct and not interchangeable.
///
/// New variants added here must:
/// - Provide aliases for any prior string literals mapping to the
///   same semantic state.
/// - Keep `Active` as the LAST variant (the `#[serde(other)]` arm
///   must remain terminal — moving it earlier is a serde error).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    /// Session finished its goal successfully.
    /// Legacy alias: `"success"` (written by `cli/resume.rs` and
    /// `main.rs` before Goal 276).
    #[serde(alias = "success")]
    Completed,
    /// Session ended because of an unrecoverable error
    /// (provider crash, panic, transcript-limit hard stop, etc.).
    /// Legacy alias: `"incomplete"` (written by the pre-Goal-276
    /// finish_status mapping for non-`NoMoreToolCalls` outcomes).
    #[serde(alias = "incomplete")]
    Crashed,
    /// Session was cancelled by a shutdown signal (SIGINT/SIGTERM).
    /// Legacy alias: `"cancelled"` (the string `FinishReason::Cancelled`
    /// emits via its `Display` impl; that string was used in
    /// `cli/session.rs` for the `interrupt` path).
    #[serde(alias = "cancelled")]
    Interrupted,
    /// Session is parked for later resume (e.g. via
    /// `recursive pause <id>`).
    Paused,
    /// Session is the live, currently-running state. Also acts as
    /// the catch-all (`#[serde(other)]`) for unknown on-disk
    /// statuses — see the type-level doc comment above.
    ///
    /// MUST remain the last variant. Adding a new variant after
    /// `Active` is a compile error; reorder first.
    ///
    /// `#[default]` makes this the `Default::default()` value,
    /// which `#[serde(default)]` on the field in `SessionMeta`
    /// relies on when a `.meta.json` file omits the `status`
    /// key.  The `#[serde(other)]` arm above is a separate
    /// catch-all for *unknown* string values — see the
    /// type-level doc comment.
    #[serde(other)]
    #[default]
    Active,
}

impl std::fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The lowercase string MUST match what serde produces via
        // `#[serde(rename_all = "lowercase")]`. The test
        // `session_status_display_matches_serde` guards against
        // accidental drift between this hand-written impl and the
        // derived serialization.
        match self {
            Self::Completed => write!(f, "completed"),
            Self::Crashed => write!(f, "crashed"),
            Self::Interrupted => write!(f, "interrupted"),
            Self::Paused => write!(f, "paused"),
            Self::Active => write!(f, "active"),
        }
    }
}

impl SessionStatus {
    /// Map a [`FinishReason`] to the canonical `SessionStatus` written
    /// when the writer is finalised.
    ///
    /// The mapping is EXHAUSTIVE: there is no wildcard arm, so
    /// adding a new variant to `FinishReason` becomes a compile error
    /// in this function rather than silently falling back to
    /// `Crashed`. The compiler is the safety net.
    pub fn for_finish(reason: &crate::agent::FinishReason) -> Self {
        use crate::agent::FinishReason;
        match reason {
            FinishReason::NoMoreToolCalls => Self::Completed,
            FinishReason::BudgetExceeded
            | FinishReason::ProviderStop(_)
            | FinishReason::Stuck { .. }
            | FinishReason::TranscriptLimit { .. }
            | FinishReason::PermissionDenialLimit => Self::Crashed,
            FinishReason::Cancelled => Self::Interrupted,
        }
    }
}

// ---------------------------------------------------------------------------
// SessionFile (legacy JSON session format)
// ---------------------------------------------------------------------------

/// A saved session that can be resumed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionFile {
    pub schema_version: u32,
    /// The original goal string.
    pub goal: String,
    /// Model name (e.g. "gpt-4o-mini").
    pub model: String,
    /// Provider identifier (e.g. "openai").
    pub provider: String,
    /// BLAKE3 hash of the tool registry specs at the time of save.
    /// Used to detect tool changes that would make the session invalid.
    pub tool_registry_hash: String,
    /// Number of steps already consumed.
    pub steps_consumed: usize,
    /// The full transcript so far (system prompt + user goal + all exchanges).
    pub transcript: Vec<Message>,
    /// Resolved provider preset id (e.g. "deepseek") at session creation
    /// time. `None` when the user did not configure a preset — kept
    /// optional for back-compat with pre-preset-config session files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
}

impl SessionFile {
    /// Create a new session file from the agent's current state.
    pub fn new(
        goal: String,
        model: String,
        provider: String,
        tool_specs: &[ToolSpec],
        steps_consumed: usize,
        transcript: Vec<Message>,
    ) -> Self {
        let tool_registry_hash = hash_tool_specs(tool_specs);
        Self {
            schema_version: SESSION_SCHEMA_VERSION,
            goal,
            model,
            provider,
            tool_registry_hash,
            steps_consumed,
            transcript,
            preset: None,
        }
    }

    /// Write the session to a JSON file at `path`.
    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::atomic::atomic_write(path, json.as_bytes())
    }

    /// Read a session from a JSON file at `path`.
    pub fn read_from(path: &Path) -> std::io::Result<Self> {
        let bytes = std::fs::read(path)?;
        serde_json::from_slice(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Validate that the tool registry hash matches the current tool specs.
    /// Returns `Ok(())` if they match, or an error describing the mismatch.
    pub fn validate_tool_registry(&self, current_specs: &[ToolSpec]) -> Result<(), String> {
        let current_hash = hash_tool_specs(current_specs);
        if self.tool_registry_hash == current_hash {
            Ok(())
        } else {
            Err(format!(
                "tool registry hash mismatch: session has '{}', current is '{}'. \
                 Tools have changed since the session was saved; cannot resume.",
                self.tool_registry_hash, current_hash
            ))
        }
    }

    /// Return the transcript messages.
    pub fn messages(&self) -> &[Message] {
        &self.transcript
    }

    /// Consume self and return the transcript.
    pub fn into_transcript(self) -> Vec<Message> {
        self.transcript
    }
}

// ---------------------------------------------------------------------------
// Tool spec hashing
// ---------------------------------------------------------------------------

/// Compute a BLAKE3 hash of the tool registry specs.
///
/// The hash is computed over a deterministic JSON representation of the
/// specs, sorted by tool name. This ensures that the same set of tools
/// always produces the same hash, regardless of registration order.
///
/// Used by both [`SessionFile`] (legacy `.json` resume) and the
/// JSONL session meta's `tool_registry_hash` field (g151) so that
/// `recursive resume` can refuse to load a session whose tool
/// inventory has drifted.
pub fn hash_tool_specs(specs: &[ToolSpec]) -> String {
    use std::collections::BTreeMap;

    let mut map: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    for spec in specs {
        let value = serde_json::json!({
            "description": spec.description,
            "parameters": spec.parameters,
        });
        map.insert(spec.name.clone(), value);
    }
    // BTreeMap<String, serde_json::Value> is always serializable; the error
    // branch is unreachable in practice. If it is somehow reached, log it and
    // return a sentinel that callers can detect and handle explicitly rather
    // than silently comparing equal to every other hash.
    let canonical = serde_json::to_string(&map).unwrap_or_else(|e| {
        tracing::error!(error = %e, "hash_tool_specs: unreachable serialization failure — drift detection may be compromised");
        String::new()
    });
    let hash = blake3::hash(canonical.as_bytes());
    hash.to_hex().to_string()
}

// ---------------------------------------------------------------------------
// Session path utilities
// ---------------------------------------------------------------------------

/// Default session output path for a given workspace.
/// Returns `~/.recursive/workspaces/<ws-hash>/sessions/<timestamp>-<goal-prefix>.json`.
pub fn default_session_path(workspace: &Path, goal: &str) -> PathBuf {
    let ts = filesystem_safe_timestamp();
    // Sanitise the goal prefix for use in a filename
    let prefix: String = goal
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
        .take(40)
        .collect();
    let prefix = if prefix.is_empty() {
        "unnamed".to_string()
    } else {
        prefix
    };
    let dir = crate::paths::user_sessions_dir(workspace)
        .unwrap_or_else(|_| workspace.join(".recursive").join("sessions"));
    dir.join(format!("{}-{}.json", ts, prefix))
}

/// List all session files in a workspace's session directory.
pub fn list_sessions(workspace: &Path) -> std::io::Result<Vec<PathBuf>> {
    let dir = match crate::paths::user_sessions_dir(workspace) {
        Ok(d) => d,
        Err(_) => workspace.join(".recursive").join("sessions"),
    };
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut sessions = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            sessions.push(path);
        }
    }
    sessions.sort();
    Ok(sessions)
}

// ---------------------------------------------------------------------------
// Timestamp utilities
// ---------------------------------------------------------------------------

/// RFC3339 timestamp safe for use in path components on all platforms.
/// Colons in the time portion are replaced with hyphens (Windows forbids `:`).
pub(crate) fn filesystem_safe_timestamp() -> String {
    chrono_lite_now().replace(':', "-")
}

/// RFC3339 UTC timestamp using `chrono`. Format:
/// "YYYY-MM-DDTHH:MM:SSZ".
pub(crate) fn chrono_lite_now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

pub(crate) fn epoch_day_to_ymd(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

// ---------------------------------------------------------------------------
// Workspace slug
// ---------------------------------------------------------------------------

/// Convert an absolute workspace path into a filesystem-safe slug.
///
/// - Replaces `/` with `-`
/// - Strips leading `-` (from the root `/`)
/// - Truncates to 80 characters
pub(crate) fn workspace_slug(workspace: &Path) -> String {
    let abs = if workspace.is_absolute() {
        workspace.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(workspace)
    };

    // Only keep characters that are valid in a session_id:
    // ASCII alphanumeric, '-', '_', '.'.  Everything else (path
    // separators, Windows drive colons, 8.3 tildes like RUNNER~1,
    // spaces, non-ASCII) becomes '-'.
    let s: String = abs
        .to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    // Strip leading dashes (from root slash / drive letter)
    let s = s.trim_start_matches('-').to_string();
    // Truncate to 80 chars (safe for multibyte)
    if s.len() > 80 {
        crate::truncate_str(&s, 80).to_string()
    } else {
        s
    }
}

// ---------------------------------------------------------------------------
// UsageMeta — per-message token usage
// ---------------------------------------------------------------------------

/// Token usage for one or more LLM API calls, as reported by the provider.
///
/// Used both per-message (`TranscriptEntry.usage`) and as a cumulative
/// session total stored in `SessionMeta.cost` (g156).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct UsageMeta {
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// Prompt-cache creation tokens (Anthropic / OpenAI).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u32>,
    /// Prompt-cache read tokens (Anthropic / OpenAI).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u32>,
    /// Reasoning/thinking tokens (DeepSeek R1, o1, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
}

impl UsageMeta {
    /// Accumulate another `UsageMeta` into self.
    pub fn accumulate(&mut self, other: &UsageMeta) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_creation_tokens = Some(
            self.cache_creation_tokens.unwrap_or(0) + other.cache_creation_tokens.unwrap_or(0),
        );
        self.cache_read_tokens =
            Some(self.cache_read_tokens.unwrap_or(0) + other.cache_read_tokens.unwrap_or(0));
        self.reasoning_tokens =
            Some(self.reasoning_tokens.unwrap_or(0) + other.reasoning_tokens.unwrap_or(0));
    }

    /// Convert from `crate::llm::TokenUsage` (g156 integration point).
    pub fn from_token_usage(tu: &crate::llm::TokenUsage) -> Self {
        UsageMeta {
            input_tokens: tu.prompt_tokens,
            output_tokens: tu.completion_tokens,
            cache_creation_tokens: if tu.cache_miss_tokens > 0 {
                Some(tu.cache_miss_tokens)
            } else {
                None
            },
            cache_read_tokens: if tu.cache_hit_tokens > 0 {
                Some(tu.cache_hit_tokens)
            } else {
                None
            },
            // Goal 273: pass through reasoning tokens so they can be
            // summed into SessionCost and priced (treated as output
            // by `ModelPricing::cost_usd`).
            reasoning_tokens: Some(tu.reasoning_tokens),
        }
    }

    /// Returns true if both `input_tokens` and `output_tokens` are zero.
    pub fn is_zero(&self) -> bool {
        self.input_tokens == 0 && self.output_tokens == 0
    }
}

// ---------------------------------------------------------------------------
// SessionCost — cumulative token cost for a session
// ---------------------------------------------------------------------------

/// Cumulative token cost for a session, stored in `.meta.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionCost {
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    #[serde(default)]
    pub total_cache_creation_tokens: u64,
    #[serde(default)]
    pub total_cache_read_tokens: u64,
    /// Goal 273: reasoning / thinking tokens summed across the
    /// session. Priced at the model's output rate (see
    /// `ModelPricing::cost_usd`).
    #[serde(default)]
    pub total_reasoning_tokens: u64,
}

impl SessionCost {
    pub fn accumulate(&mut self, usage: &UsageMeta) {
        self.total_input_tokens += usage.input_tokens as u64;
        self.total_output_tokens += usage.output_tokens as u64;
        self.total_cache_creation_tokens += usage.cache_creation_tokens.unwrap_or(0) as u64;
        self.total_cache_read_tokens += usage.cache_read_tokens.unwrap_or(0) as u64;
        self.total_reasoning_tokens += usage.reasoning_tokens.unwrap_or(0) as u64;
    }
}

// ---------------------------------------------------------------------------
// SessionMeta
// ---------------------------------------------------------------------------

/// `schema_version` written into a `SessionMeta` whose on-disk
/// representation predates the field. Pre-Goal-269 session files
/// have no `schema_version`; defaulting to 1 (the current value)
/// keeps them loadable.
fn default_schema_version() -> u32 {
    1
}

/// Metadata for a JSONL session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    /// Schema version of the persisted `SessionMeta`. Bumped
    /// whenever a non-backward-compatible field is added (e.g.
    /// dropping a `#[serde(default)]` attribute). The on-disk
    /// format is read by `SessionReader::load_meta`, which checks
    /// this field and refuses to load a session with a
    /// `schema_version` it does not understand.
    ///
    /// Defaults to `1` for pre-Goal-269 session files (the field
    /// was introduced in Goal 269, 2026-06-11).
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub session_id: String,
    pub goal: String,
    pub model: String,
    pub provider: String,
    pub created_at: String,
    pub updated_at: String,
    pub message_count: u64,
    /// Lifecycle state of the session. Defaults to `Active` for
    /// `.meta.json` files written before Goal 276 (no `status`
    /// key) and degrades to `Active` for unknown string values
    /// (caught by `SessionStatus::Active`'s `#[serde(other)]` arm).
    #[serde(default)]
    pub status: SessionStatus,
    /// BLAKE3 hash of the tool registry specs at session creation
    /// time. Used by `recursive resume` (g151) to refuse loading a
    /// session whose tool inventory has drifted. `None` for
    /// pre-g151 sessions; resume tolerates the absence with a
    /// warning rather than an abort.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_registry_hash: Option<String>,
    /// First user message in this session, truncated to 200 chars (g157).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_prompt: Option<String>,
    /// Most recent user message, truncated to 200 chars (g157).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prompt: Option<String>,
    /// Cumulative token usage for this session (g156).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<SessionCost>,
    /// Resolved provider preset id (e.g. "deepseek") at session creation
    /// time. `None` for pre-preset-config sessions or when the user did
    /// not configure a preset. Optional + skipped on serialize so old
    /// session files round-trip cleanly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// Optional human-readable display name for this session, set via `--name`.
    /// Shown in the /resume picker and `sessions list` output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

// ---------------------------------------------------------------------------
// ExportedTranscript
// ---------------------------------------------------------------------------

/// A portable exported transcript for sharing and analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportedTranscript {
    pub version: u32,
    pub session_id: String,
    pub model: String,
    pub goal: String,
    pub created_at: String,
    /// Status at the moment of export. Mirrors `SessionMeta::status`
    /// — the same enum is used in both structs to keep the
    /// wire shape consistent for external SDK consumers.
    pub status: SessionStatus,
    pub messages: Vec<TranscriptEntry>,
    pub message_count: u64,
}

impl ExportedTranscript {
    /// Build an ExportedTranscript from a session directory.
    pub fn from_session_dir(session_dir: &Path) -> std::io::Result<Self> {
        let meta = SessionReader::load_meta(session_dir)?;
        let entries = SessionReader::load_transcript(session_dir)?;
        Ok(Self {
            version: 1,
            session_id: meta.session_id,
            model: meta.model,
            goal: meta.goal,
            created_at: meta.created_at,
            status: meta.status,
            messages: entries.clone(),
            message_count: entries.len() as u64,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests for mod.rs types
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Message, Role};

    #[test]
    fn session_round_trip() {
        let goal = "fix the bug".to_string();
        let model = "gpt-4o-mini".to_string();
        let provider = "openai".to_string();
        let tool_specs = vec![
            ToolSpec {
                name: "Read".into(),
                description: "Read a file".into(),
                parameters: serde_json::json!({"type":"object"}),
            },
            ToolSpec {
                name: "Write".into(),
                description: "Write a file".into(),
                parameters: serde_json::json!({"type":"object"}),
            },
        ];
        let transcript = vec![
            Message::system("You are a helpful assistant.".to_string()),
            Message::user("fix the bug".to_string()),
            Message::assistant("Let me look at the code.".to_string()),
        ];

        let session = SessionFile::new(
            goal.clone(),
            model.clone(),
            provider.clone(),
            &tool_specs,
            2,
            transcript.clone(),
        );

        let tmp = tempfile::NamedTempFile::new().unwrap();
        session.write_to(tmp.path()).unwrap();

        let restored = SessionFile::read_from(tmp.path()).unwrap();
        assert_eq!(restored.schema_version, SESSION_SCHEMA_VERSION);
        assert_eq!(restored.goal, goal);
        assert_eq!(restored.model, model);
        assert_eq!(restored.provider, provider);
        assert_eq!(restored.steps_consumed, 2);
        assert_eq!(restored.transcript.len(), 3);
        assert_eq!(
            restored.transcript[0].content,
            "You are a helpful assistant."
        );
        assert_eq!(restored.transcript[1].content, "fix the bug");
        assert_eq!(restored.transcript[2].content, "Let me look at the code.");
    }

    #[test]
    fn resume_validates_tool_registry_hash() {
        let tool_specs = vec![ToolSpec {
            name: "Read".into(),
            description: "Read a file".into(),
            parameters: serde_json::json!({"type":"object"}),
        }];
        let session = SessionFile::new(
            "test".into(),
            "model".into(),
            "provider".into(),
            &tool_specs,
            0,
            vec![],
        );

        // Same specs should validate
        assert!(session.validate_tool_registry(&tool_specs).is_ok());

        // Different specs should fail
        let different_specs = vec![ToolSpec {
            name: "Write".into(),
            description: "Write a file".into(),
            parameters: serde_json::json!({"type":"object"}),
        }];
        assert!(session.validate_tool_registry(&different_specs).is_err());
    }

    #[test]
    #[cfg_attr(target_os = "windows", ignore)]
    fn session_list_finds_files_in_workspace() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let ws = tmp.path();

        // No sessions dir yet
        let sessions = list_sessions(ws).unwrap();
        assert!(sessions.is_empty());

        // Create a session file
        let session = SessionFile::new(
            "test".into(),
            "model".into(),
            "provider".into(),
            &[],
            0,
            vec![],
        );
        let path = default_session_path(ws, "test");
        session.write_to(&path).unwrap();

        let sessions = list_sessions(ws).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].extension().and_then(|e| e.to_str()),
            Some("json")
        );
    }

    #[test]
    fn resume_continues_from_seeded_transcript() {
        let transcript = vec![
            Message::system("sys".to_string()),
            Message::user("original goal".to_string()),
            Message::assistant("partial work".to_string()),
        ];
        let session = SessionFile::new(
            "original goal".into(),
            "model".into(),
            "provider".into(),
            &[],
            1,
            transcript.clone(),
        );

        // The transcript should be preserved exactly
        assert_eq!(session.messages().len(), 3);
        assert_eq!(session.messages()[0].content, "sys");
        assert_eq!(session.messages()[1].content, "original goal");
        assert_eq!(session.messages()[2].content, "partial work");

        // into_transcript should give back the messages
        let restored = session.into_transcript();
        assert_eq!(restored.len(), 3);
    }

    #[test]
    fn round_trip_with_tool_calls() {
        use crate::llm::ToolCall;

        let tool_calls = vec![
            ToolCall {
                id: "call_001".to_string(),
                name: "Read".to_string(),
                arguments: serde_json::json!({"path": "/tmp/foo.rs"}),
            },
            ToolCall {
                id: "call_002".to_string(),
                name: "Write".to_string(),
                arguments: serde_json::json!({"path": "/tmp/bar.rs", "content": "fn main() {}"}),
            },
        ];

        let transcript = vec![
            Message::system("You are an agent.".to_string()),
            Message::user("refactor the code".to_string()),
            Message::assistant_with_tool_calls(
                "I'll read the file first.".to_string(),
                tool_calls.clone(),
            ),
            Message::tool_result("call_001", "fn main() { println!(\"hello\"); }"),
        ];

        let session = SessionFile::new(
            "refactor".into(),
            "gpt-4o".into(),
            "openai".into(),
            &[],
            3,
            transcript,
        );

        let tmp = tempfile::NamedTempFile::new().unwrap();
        session.write_to(tmp.path()).unwrap();

        let restored = SessionFile::read_from(tmp.path()).unwrap();
        assert_eq!(restored.transcript.len(), 4);

        // Verify the assistant message with tool_calls is preserved
        let assistant_msg = &restored.transcript[2];
        assert_eq!(assistant_msg.role, Role::Assistant);
        assert_eq!(assistant_msg.content, "I'll read the file first.");
        assert_eq!(assistant_msg.tool_calls.len(), 2);
        assert_eq!(assistant_msg.tool_calls[0].id, "call_001");
        assert_eq!(assistant_msg.tool_calls[0].name, "Read");
        assert_eq!(
            assistant_msg.tool_calls[0].arguments,
            serde_json::json!({"path": "/tmp/foo.rs"})
        );
        assert_eq!(assistant_msg.tool_calls[1].id, "call_002");
        assert_eq!(assistant_msg.tool_calls[1].name, "Write");
        assert_eq!(
            assistant_msg.tool_calls[1].arguments,
            serde_json::json!({"path": "/tmp/bar.rs", "content": "fn main() {}"})
        );

        // Verify the tool result message
        let tool_msg = &restored.transcript[3];
        assert_eq!(tool_msg.role, Role::Tool);
        assert_eq!(tool_msg.tool_call_id, Some("call_001".to_string()));
        assert_eq!(tool_msg.content, "fn main() { println!(\"hello\"); }");
    }

    #[test]
    fn read_from_nonexistent_file() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let bogus_path = tmp.path().join("does_not_exist.json");

        let result = SessionFile::read_from(&bogus_path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn read_from_corrupt_json() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "this is not valid json {{{garbage").unwrap();

        let result = SessionFile::read_from(tmp.path());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn validate_tool_registry_mismatch() {
        let original_specs = vec![
            ToolSpec {
                name: "Read".into(),
                description: "Read a file".into(),
                parameters: serde_json::json!({"type":"object"}),
            },
            ToolSpec {
                name: "Write".into(),
                description: "Write a file".into(),
                parameters: serde_json::json!({"type":"object"}),
            },
        ];

        let session = SessionFile::new(
            "test".into(),
            "model".into(),
            "provider".into(),
            &original_specs,
            0,
            vec![],
        );

        // Validate against a completely different set of tools
        let different_specs = vec![ToolSpec {
            name: "execute_command".into(),
            description: "Run a shell command".into(),
            parameters: serde_json::json!({"type":"object","properties":{"cmd":{"type":"string"}}}),
        }];

        let result = session.validate_tool_registry(&different_specs);
        assert!(result.is_err());
        let err_msg = result.unwrap_err();
        assert!(
            err_msg.contains("mismatch"),
            "Expected error to contain 'mismatch', got: {err_msg}"
        );
    }

    #[test]
    fn default_session_path_sanitizes_special_chars() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let ws = tmp.path();

        // Goal with spaces, slashes, unicode, and other special chars
        let goal = "fix bug/issue #42 — with spëcial chars™ 日本語";
        let path = default_session_path(ws, goal);

        // Extract just the filename (without the .json extension)
        let filename = path.file_stem().unwrap().to_str().unwrap();

        // The filename format is "{timestamp}-{sanitized_goal}".
        // The timestamp is filesystem-safe (colons replaced with hyphens).
        // We verify the goal-derived suffix: strip the timestamp prefix
        // (everything up to and including the "Z-" separator).
        let goal_suffix = filename
            .find("Z-")
            .map(|i| &filename[i + 2..])
            .expect("filename should contain Z- separator between timestamp and goal");

        // The goal suffix should contain only alphanumeric (unicode-aware), underscore, or dash
        for ch in goal_suffix.chars() {
            assert!(
                ch.is_alphanumeric() || ch == '_' || ch == '-',
                "Unexpected character '{}' (U+{:04X}) in goal suffix: {}",
                ch,
                ch as u32,
                goal_suffix
            );
        }

        // Spaces, slashes, #, —, ™ should all be stripped
        assert!(!goal_suffix.contains(' '));
        assert!(!goal_suffix.contains('/'));
        assert!(!goal_suffix.contains('#'));
        assert!(!goal_suffix.contains('™'));
        assert!(!goal_suffix.contains('—'));

        // The path should still end inside a sessions/ directory
        // (now lives under the user data dir, not the workspace).
        let parent = path.parent().expect("session path has parent");
        assert_eq!(
            parent.file_name().and_then(|n| n.to_str()),
            Some("sessions"),
            "expected sessions dir as parent, got {}",
            path.display()
        );
        // And should have .json extension
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("json"));
    }

    #[test]
    fn filesystem_safe_timestamp_has_no_colons() {
        let ts = filesystem_safe_timestamp();
        assert!(!ts.contains(':'));
        assert!(ts.ends_with('Z'));
    }

    #[test]
    fn workspace_slug_matches_expected() {
        // Absolute path
        let p = Path::new("/home/user/projects/my-app");
        let slug = workspace_slug(p);
        assert!(!slug.starts_with('-'));
        assert!(slug.contains("home-user-projects-my-app"));
        assert!(slug.len() <= 80);

        // Relative path
        let p = Path::new(".");
        let slug = workspace_slug(p);
        assert!(!slug.is_empty());
        assert!(slug.len() <= 80);
    }

    // ── chrono_lite_now / epoch_day_to_ymd ──────────────────────────────────

    #[test]
    fn epoch_day_to_ymd_unix_epoch() {
        // Day 0 = 1970-01-01
        assert_eq!(epoch_day_to_ymd(0), (1970, 1, 1));
    }

    #[test]
    fn epoch_day_to_ymd_known_dates() {
        // 2024-01-01 = day 19723 since epoch
        assert_eq!(epoch_day_to_ymd(19723), (2024, 1, 1));
        // 2000-02-29 (leap day) = day 11016
        assert_eq!(epoch_day_to_ymd(11016), (2000, 2, 29));
        // 2100-03-01 (2100 is NOT a leap year, so 2100-02-29 doesn't exist)
        // 2100-01-01 = day 47482
        assert_eq!(epoch_day_to_ymd(47482), (2100, 1, 1));
    }

    #[test]
    fn chrono_lite_now_format() {
        let ts = chrono_lite_now();
        // Must match YYYY-MM-DDTHH:MM:SSZ
        assert_eq!(ts.len(), 20, "unexpected length: {ts}");
        assert_eq!(&ts[4..5], "-", "missing first dash: {ts}");
        assert_eq!(&ts[7..8], "-", "missing second dash: {ts}");
        assert_eq!(&ts[10..11], "T", "missing T separator: {ts}");
        assert_eq!(&ts[13..14], ":", "missing first colon: {ts}");
        assert_eq!(&ts[16..17], ":", "missing second colon: {ts}");
        assert_eq!(&ts[19..20], "Z", "missing Z suffix: {ts}");
        // All digit fields must parse as numbers
        let year: u32 = ts[0..4].parse().expect("year");
        let month: u32 = ts[5..7].parse().expect("month");
        let day: u32 = ts[8..10].parse().expect("day");
        let hour: u32 = ts[11..13].parse().expect("hour");
        let min: u32 = ts[14..16].parse().expect("minute");
        let sec: u32 = ts[17..19].parse().expect("second");
        assert!(year >= 2024, "year looks wrong: {year}");
        assert!((1..=12).contains(&month), "month out of range: {month}");
        assert!((1..=31).contains(&day), "day out of range: {day}");
        assert!(hour < 24, "hour out of range: {hour}");
        assert!(min < 60, "minute out of range: {min}");
        assert!(sec < 60, "second out of range: {sec}");
    }

    // ── Goal 276: SessionStatus enum + backward-compatibility surface ─────

    #[test]
    fn session_status_serializes_lowercase() {
        // The lowercase string is part of the wire contract — external
        // SDK consumers parse `status: "completed"` as a known state.
        // A drift here would silently break their pipelines.
        for (variant, expected) in [
            (SessionStatus::Active, "active"),
            (SessionStatus::Completed, "completed"),
            (SessionStatus::Crashed, "crashed"),
            (SessionStatus::Interrupted, "interrupted"),
            (SessionStatus::Paused, "paused"),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, format!("\"{expected}\""), "mismatch for {variant:?}");
        }
    }

    #[test]
    fn session_status_display_matches_serde() {
        // The hand-written Display impl is what CLI output and format
        // strings see. The derived serde impl is what the on-disk
        // JSON sees. They must agree — if a future contributor adds
        // a variant or renames one, this test catches the drift.
        for variant in [
            SessionStatus::Active,
            SessionStatus::Completed,
            SessionStatus::Crashed,
            SessionStatus::Interrupted,
            SessionStatus::Paused,
        ] {
            let from_display = format!("{variant}");
            let from_serde = serde_json::to_string(&variant)
                .unwrap()
                .trim_matches('"')
                .to_string();
            assert_eq!(
                from_display, from_serde,
                "Display and serde disagree for {variant:?}"
            );
        }
    }

    #[test]
    fn session_status_legacy_aliases_round_trip() {
        // Pre-Goal-276 writer emitted `"success"` for completed runs and
        // `"incomplete"` for everything else; the migrate path also
        // accepts `"cancelled"`. The aliases must read back as the
        // correct modern variant — silent loss of "success" -> "active"
        // would be a corruption bug visible only after a real
        // production write.
        let cases = [
            ("success", SessionStatus::Completed),
            ("incomplete", SessionStatus::Crashed),
            ("cancelled", SessionStatus::Interrupted),
            ("completed", SessionStatus::Completed),
            ("crashed", SessionStatus::Crashed),
            ("interrupted", SessionStatus::Interrupted),
            ("paused", SessionStatus::Paused),
        ];
        for (legacy_str, expected) in cases {
            let parsed: SessionStatus = serde_json::from_str(&format!("\"{legacy_str}\""))
                .unwrap_or_else(|e| panic!("legacy alias {legacy_str:?} did not deserialize: {e}"));
            assert_eq!(parsed, expected, "wrong mapping for {legacy_str:?}");
        }
    }

    #[test]
    fn session_meta_unknown_status_deserializes_to_active() {
        // Old on-disk `.meta.json` files (or files written by a future
        // build) may contain a `status` value this build doesn't
        // recognise — e.g. `"stalled"` from a future hang-detection
        // feature. The `#[serde(other)]` catch-all on `Active` must
        // convert that to `Active` instead of refusing to load the
        // session.
        let json = serde_json::json!({
            "session_id": "future",
            "goal": "future session",
            "model": "gpt-4o-mini",
            "provider": "openai",
            "created_at": "2099-01-01T00:00:00Z",
            "updated_at": "2099-01-01T00:00:00Z",
            "message_count": 0,
            "status": "stalled"
        });
        let restored: SessionMeta = serde_json::from_value(json)
            .expect("unknown status string must not break SessionMeta deserialization");
        assert_eq!(
            restored.status,
            SessionStatus::Active,
            "unknown status should map to Active via #[serde(other)]"
        );
    }

    #[test]
    fn session_meta_missing_status_field_defaults_to_active() {
        // The `#[serde(default)]` on the `status` field covers the case
        // where the key is absent (e.g. a `.meta.json` written before
        // the field existed). This is distinct from the
        // unknown-string case above (which `#[serde(other)]` handles).
        let json = serde_json::json!({
            "session_id": "missing-status",
            "goal": "no status key",
            "model": "gpt-4o-mini",
            "provider": "openai",
            "created_at": "2099-01-01T00:00:00Z",
            "updated_at": "2099-01-01T00:00:00Z",
            "message_count": 0
            // NB: no `status` key
        });
        let restored: SessionMeta = serde_json::from_value(json)
            .expect("missing status field must not break SessionMeta deserialization");
        assert_eq!(
            restored.status,
            SessionStatus::Active,
            "missing status key should default to Active via #[serde(default)]"
        );
    }

    #[test]
    fn session_meta_status_active_round_trip() {
        // Build a SessionMeta with the new status enum, serialize, then
        // deserialize, and confirm the variant is preserved.
        let meta = SessionMeta {
            status: SessionStatus::Active,
            ..meta_for_test(SUPPORTED_SESSION_SCHEMA_VERSION)
        };
        let json = serde_json::to_string(&meta).expect("serialize SessionMeta");
        let restored: SessionMeta = serde_json::from_str(&json).expect("deserialize SessionMeta");
        assert_eq!(restored.status, SessionStatus::Active);
    }

    #[test]
    fn for_finish_maps_exhaustively() {
        // Every `FinishReason` variant must map to a `SessionStatus`.
        // If a new `FinishReason` is added without updating
        // `SessionStatus::for_finish`, the compiler will surface the
        // non-exhaustive match (no `_` arm) — this test just
        // double-checks the currently-known mapping is correct.
        use crate::agent::FinishReason;
        assert_eq!(
            SessionStatus::for_finish(&FinishReason::NoMoreToolCalls),
            SessionStatus::Completed
        );
        assert_eq!(
            SessionStatus::for_finish(&FinishReason::BudgetExceeded),
            SessionStatus::Crashed
        );
        assert_eq!(
            SessionStatus::for_finish(&FinishReason::ProviderStop("rate_limited".into())),
            SessionStatus::Crashed
        );
        assert_eq!(
            SessionStatus::for_finish(&FinishReason::Stuck {
                repeated_call: "Read".into(),
                repeats: 5
            }),
            SessionStatus::Crashed
        );
        assert_eq!(
            SessionStatus::for_finish(&FinishReason::TranscriptLimit {
                chars: 100_000,
                limit: 80_000
            }),
            SessionStatus::Crashed
        );
        assert_eq!(
            SessionStatus::for_finish(&FinishReason::Cancelled),
            SessionStatus::Interrupted
        );
        assert_eq!(
            SessionStatus::for_finish(&FinishReason::PermissionDenialLimit),
            SessionStatus::Crashed
        );
    }

    // -----------------------------------------------------------------------
    // SessionMeta schema_version tests (Goal 269)
    // -----------------------------------------------------------------------

    /// Build a `SessionMeta` with the minimum required fields populated
    /// for tests that care only about the schema-version behaviour.
    /// `schema_version` is intentionally set via the helper, not
    /// hard-coded, so changing the supported version only requires
    /// updating `SUPPORTED_SESSION_SCHEMA_VERSION` in one place.
    fn meta_for_test(schema_version: u32) -> SessionMeta {
        SessionMeta {
            schema_version,
            session_id: "20260611T120000Z-test".into(),
            goal: "schema version test".into(),
            model: "gpt-4o-mini".into(),
            provider: "openai".into(),
            created_at: "2026-06-11T12:00:00Z".into(),
            updated_at: "2026-06-11T12:00:00Z".into(),
            message_count: 0,
            status: SessionStatus::Active,
            tool_registry_hash: None,
            first_prompt: None,
            last_prompt: None,
            cost: None,
            preset: None,
            name: None,
        }
    }

    #[test]
    fn test_session_meta_round_trip() {
        // Goal 269 — the `schema_version` field is preserved across
        // a serialize/deserialize round-trip.
        let original = meta_for_test(SUPPORTED_SESSION_SCHEMA_VERSION);
        let json = serde_json::to_string(&original).expect("serialize SessionMeta");
        let restored: SessionMeta = serde_json::from_str(&json).expect("deserialize SessionMeta");
        assert_eq!(restored.schema_version, SUPPORTED_SESSION_SCHEMA_VERSION);
        assert_eq!(restored.schema_version, 1);
        assert_eq!(restored.session_id, original.session_id);
        assert_eq!(restored.goal, original.goal);
    }

    #[test]
    fn test_session_meta_default_schema_version() {
        // Goal 269 — pre-Goal-269 `.meta.json` files omit
        // `schema_version`. The `#[serde(default)]` attribute must
        // fill in the current value so they remain loadable.
        let legacy_json = serde_json::json!({
            "session_id": "20260610T120000Z-legacy",
            "goal": "legacy session",
            "model": "gpt-4o-mini",
            "provider": "openai",
            "created_at": "2026-06-10T12:00:00Z",
            "updated_at": "2026-06-10T12:00:00Z",
            "message_count": 0,
            "status": "completed"
        });
        // The key assertion: no `schema_version` key, but deserialization
        // still succeeds and the field is populated by the helper.
        assert!(
            legacy_json.get("schema_version").is_none(),
            "test fixture must omit schema_version to exercise the default"
        );
        let restored: SessionMeta =
            serde_json::from_value(legacy_json).expect("deserialize legacy SessionMeta");
        assert_eq!(restored.schema_version, default_schema_version());
        assert_eq!(restored.schema_version, 1);
    }

    #[test]
    fn test_load_rejects_future_schema_version() {
        // Goal 269 — `SessionReader::load_meta` must refuse to load a
        // session whose `schema_version` is newer than this build
        // supports. The error message must surface both the found
        // and the supported values, so the user can tell which build
        // produced the session.
        let tmp = tempfile::tempdir().expect("tempdir");
        let session_dir = tmp.path();
        std::fs::create_dir_all(session_dir).expect("mkdir session dir");
        let future = serde_json::json!({
            "schema_version": 999,
            "session_id": "2099-01-01T00:00:00Z-future",
            "goal": "future session",
            "model": "gpt-4o-mini",
            "provider": "openai",
            "created_at": "2099-01-01T00:00:00Z",
            "updated_at": "2099-01-01T00:00:00Z",
            "message_count": 0,
            "status": "active"
        });
        let meta_path = session_dir.join(".meta.json");
        std::fs::write(
            &meta_path,
            serde_json::to_string_pretty(&future).expect("serialize future meta"),
        )
        .expect("write future .meta.json");

        let err = SessionReader::load_meta(session_dir)
            .expect_err("load_meta must reject future schema_version");
        let msg = err.to_string();
        // The `crate::error::Error::SchemaTooNew` display format we
        // registered must reach the caller, even after the conversion
        // to `std::io::Error::InvalidData`.
        assert!(
            msg.contains("schema_version=999"),
            "error must include found version; got: {msg}"
        );
        assert!(
            msg.contains("supported up to 1"),
            "error must include supported version; got: {msg}"
        );
    }

    /// Goal 273: SessionCost tracks reasoning tokens across accumulate calls.
    #[test]
    fn session_cost_tracks_reasoning_tokens() {
        let mut cost = SessionCost::default();
        let usage_a = UsageMeta {
            reasoning_tokens: Some(1000),
            ..Default::default()
        };
        let usage_b = UsageMeta {
            reasoning_tokens: Some(500),
            ..Default::default()
        };
        cost.accumulate(&usage_a);
        cost.accumulate(&usage_b);
        assert_eq!(cost.total_reasoning_tokens, 1500);
    }
}
