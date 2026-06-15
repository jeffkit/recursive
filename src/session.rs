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

use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

use crate::event::{AgentEvent, EventSink};
use crate::llm::ToolSpec;
use crate::message::Message;

/// Current schema version for session files.
/// Increment when the format changes in a breaking way.
const SESSION_SCHEMA_VERSION: u32 = 1;

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

/// RFC3339 timestamp safe for use in path components on all platforms.
/// Colons in the time portion are replaced with hyphens (Windows forbids `:`).
fn filesystem_safe_timestamp() -> String {
    chrono_lite_now().replace(':', "-")
}

/// RFC3339 UTC timestamp using `chrono`. Format:
/// "YYYY-MM-DDTHH:MM:SSZ".
fn chrono_lite_now() -> String {
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
// JSONL session persistence (Goal 107)
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

/// `schema_version` written into a `SessionMeta` whose on-disk
/// representation predates the field. Pre-Goal-269 session files
/// have no `schema_version`; defaulting to 1 (the current value)
/// keeps them loadable.
fn default_schema_version() -> u32 {
    1
}

/// Maximum `schema_version` this build of the binary can interpret.
/// `SessionReader::load_meta` rejects anything strictly greater
/// than this. Bump this constant (and the helper above) when
/// making a non-backward-compatible change to `SessionMeta`.
const SUPPORTED_SESSION_SCHEMA_VERSION: u32 = 1;

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
    /// Build an  from a session directory.
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

/// A single JSONL line representing one message in the transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptEntry {
    /// Stable UUID v4 for this message (g155). Empty string for pre-g155 entries.
    #[serde(default)]
    pub uuid: String,
    /// UUID of the parent message in the conversation chain (g155).
    /// `None` for the root message (first message in a session or chain branch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_uuid: Option<String>,
    /// For tool-result messages, the UUID of the assistant message that issued
    /// the tool call (g155). Enables correct attribution in multi-agent trees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_tool_assistant_uuid: Option<String>,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<crate::llm::ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Token usage for this message (non-None for assistant messages, g156).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageMeta>,
    pub timestamp: String,
    /// Goal-153: per-call audit metadata. Only populated on `role: "tool"`
    /// messages (i.e. tool results). Never sent to providers — see
    /// [`entry_to_message`] which drops this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit: Option<crate::tools::AuditMeta>,
}

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

/// A compact-boundary system entry written to the JSONL when cross-turn
/// compaction fires (g157). Parsed by `SessionReader::load_transcript`
/// to skip pre-boundary messages on resume.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CompactBoundaryEntry {
    #[serde(rename = "type")]
    entry_type: String,
    subtype: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turn: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compacted_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary_uuid: Option<String>,
    timestamp: String,
}

/// Convert a persisted [`TranscriptEntry`] into a runtime [`Message`].
///
/// Drops persistence-only fields (`id`, `parent_id`, `timestamp`,
/// and — once g153 lands — `audit`). The result is what
/// `run_resumed` expects as its `seed` argument and what provider
/// adapters serialise onto the wire.
///
/// This is the **isolation point** between the persisted shape
/// (`TranscriptEntry`) and the LLM wire shape (`Message`):
/// provider adapters never see persistence-only fields. An unknown
/// `role` string maps to `Role::User` defensively so a corrupted
/// transcript can't panic the resume handler — in practice this
/// never fires because the writer only ever emits the four known
/// roles.
pub fn entry_to_message(entry: TranscriptEntry) -> Message {
    use crate::message::Role;
    let role = match entry.role.as_str() {
        "system" => Role::System,
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        _ => Role::User,
    };
    Message {
        role,
        content: entry.content,
        tool_calls: entry.tool_calls,
        tool_call_id: entry.tool_call_id,
        reasoning_content: entry.reasoning_content,
        is_compaction_summary: false,
    }
}
/// Writer for appending messages to a JSONL session file.
///
/// Opens (or creates) a `.jsonl` file in append mode and writes one
/// JSON object per line. A companion `.meta.json` file tracks session
/// metadata and is updated on `finish()`.
pub struct SessionWriter {
    session_id: String,
    session_dir: PathBuf,
    writer: BufWriter<std::fs::File>,
    message_count: u64,
    /// UUID of the last-appended message; used as `parent_uuid` for the
    /// next message in the chain (g155).
    last_uuid: Option<String>,
    /// Accumulated token usage across all appended messages (g156).
    cumulative_usage: UsageMeta,
    /// First user prompt, truncated to 200 chars (g157).
    first_prompt: Option<String>,
    /// Most recent user prompt, truncated to 200 chars (g157).
    last_prompt: Option<String>,
    /// Held for the lifetime of the writer so a second
    /// `SessionWriter::open_existing` (or `create`) on the same
    /// `session_dir` is refused. Cleaned up on `Drop`.
    _lock: Option<SessionLock>,
    /// Optional human-readable display name for this session.
    name: Option<String>,
}

impl SessionWriter {
    /// Create a new session in the given workspace directory.
    ///
    /// The session directory is `<workspace>/.recursive/sessions/<workspace-slug>/<session-id>/`.
    /// The `.jsonl` file and `.meta.json` are placed inside that directory.
    ///
    /// Equivalent to `create_with_tools(workspace, goal, model, provider, &[])`;
    /// no `tool_registry_hash` is stamped in the meta. Prefer
    /// `create_with_tools` whenever the caller has a `ToolRegistry`,
    /// so that `recursive resume` (g151) can detect tool drift.
    pub fn create(
        workspace: &Path,
        goal: &str,
        model: &str,
        provider: &str,
    ) -> std::io::Result<Self> {
        Self::create_with_tools(workspace, goal, model, provider, &[], None)
    }

    /// Create a new session, stamping a BLAKE3 hash of `tool_specs`
    /// into `.meta.json` as `tool_registry_hash`. The hash is what
    /// `recursive resume` validates against the current registry —
    /// if they differ, resume aborts (g151).
    ///
    /// Pass `&[]` for `tool_specs` if the caller has no registry
    /// (e.g. tests, ad-hoc tools), in which case the hash is `None`
    /// and resume will warn but not abort.
    ///
    /// `preset` is the resolved provider preset id (e.g. "deepseek")
    /// from the active `Config`. Pass `None` for pre-preset-config
    /// callers or when the user did not opt in; the field is
    /// `Option<String>` and `skip_serializing_if = "Option::is_none"`
    /// keeps the on-disk format clean.
    pub fn create_with_tools(
        workspace: &Path,
        goal: &str,
        model: &str,
        provider: &str,
        tool_specs: &[ToolSpec],
        preset: Option<&str>,
    ) -> std::io::Result<Self> {
        let slug = workspace_slug(workspace);
        let session_id = format!("{}-{}", filesystem_safe_timestamp(), slug);
        // Sessions live under the per-user data dir, not the project,
        // so they don't pollute the user's `git status`.
        let sessions_root = crate::paths::user_sessions_dir(workspace)
            .map_err(|e| std::io::Error::other(format!("user_sessions_dir: {e}")))?;
        let session_dir = sessions_root.join(&slug).join(&session_id);

        std::fs::create_dir_all(&session_dir)?;

        // Acquire the per-session lock before opening any files for
        // writing — guards against two `recursive resume <id>`
        // invocations clobbering the same transcript.
        let lock = SessionLock::acquire(&session_dir)?;

        let jsonl_path = session_dir.join("transcript.jsonl");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&jsonl_path)?;

        let now = chrono_lite_now();
        let tool_registry_hash = if tool_specs.is_empty() {
            None
        } else {
            Some(hash_tool_specs(tool_specs))
        };
        let meta = SessionMeta {
            schema_version: SUPPORTED_SESSION_SCHEMA_VERSION,
            session_id: session_id.clone(),
            goal: goal.to_string(),
            model: model.to_string(),
            provider: provider.to_string(),
            created_at: now.clone(),
            updated_at: now.clone(),
            message_count: 0,
            status: SessionStatus::Active,
            tool_registry_hash,
            first_prompt: None,
            last_prompt: None,
            cost: None,
            preset: preset.map(|s| s.to_string()),
            name: None,
        };

        // Write initial meta file (atomic: temp + rename to prevent corruption).
        let meta_path = session_dir.join(".meta.json");
        let meta_json = serde_json::to_string_pretty(&meta)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        crate::atomic::atomic_write(&meta_path, meta_json.as_bytes())?;

        Ok(Self {
            session_id,
            session_dir,
            writer: BufWriter::new(file),
            message_count: 0,
            last_uuid: None,
            cumulative_usage: UsageMeta::default(),
            first_prompt: None,
            last_prompt: None,
            _lock: Some(lock),
            name: None,
        })
    }

    /// Re-open an existing session directory for appending.
    ///
    /// Reads the existing `.meta.json` to recover `message_count`,
    /// `created_at`, `goal`, `model`, and `provider`. The
    /// `tool_registry_hash` is **not** re-validated here — the
    /// caller (typically the `Cmd::Resume` handler) must have done
    /// that already.
    ///
    /// Acquires `SessionLock::acquire(session_dir)` so a second
    /// resume on the same session is refused while this writer is
    /// alive.
    ///
    /// Continues the `msg_NNN` sequence from where the existing
    /// transcript left off (so the first `append()` after
    /// `open_existing` does not collide with prior messages).
    pub fn open_existing(session_dir: &Path) -> std::io::Result<Self> {
        let meta = SessionReader::load_meta(session_dir)?;

        let lock = SessionLock::acquire(session_dir)?;

        let jsonl_path = session_dir.join("transcript.jsonl");

        // Recover last_uuid from the last message entry in the JSONL so the
        // UUID chain can continue from where the previous run left off (g155).
        let last_uuid = read_last_message_uuid(&jsonl_path);

        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&jsonl_path)?;

        Ok(Self {
            session_id: meta.session_id,
            session_dir: session_dir.to_path_buf(),
            writer: BufWriter::new(file),
            message_count: meta.message_count,
            last_uuid,
            cumulative_usage: UsageMeta::default(),
            first_prompt: meta.first_prompt,
            last_prompt: meta.last_prompt,
            _lock: Some(lock),
            name: meta.name,
        })
    }

    /// Append a message to the JSONL file.
    ///
    /// Returns the UUID assigned to this message entry. Bumps
    /// `SessionMeta.updated_at` on every `assistant` or `user`
    /// message (skipping per-tool-result writes is an
    /// optimisation — a turn with N tool calls would otherwise
    /// rewrite the meta file 2N times). The "most-recent" shortcut
    /// in `recursive resume` (g151) relies on `updated_at` being
    /// live for crashed sessions.
    ///
    /// `parent_uuid_override` — if `Some`, this UUID is used as `parent_uuid`
    /// for the new entry instead of `self.last_uuid`. Use this for subagent
    /// messages that branch off a specific parent agent message (g155).
    ///
    /// `usage` — token usage to attach to this entry (non-None for assistant
    /// messages, g156).
    pub fn append(
        &mut self,
        msg: &Message,
        parent_uuid_override: Option<&str>,
        usage: Option<&UsageMeta>,
    ) -> std::io::Result<String> {
        self.append_with_audit(msg, None, parent_uuid_override, usage)
    }

    /// Append a message with optional audit metadata (Goal 153).
    /// `audit` should only be `Some` for `Role::Tool` messages.
    pub fn append_with_audit(
        &mut self,
        msg: &Message,
        audit: Option<crate::tools::AuditMeta>,
        parent_uuid_override: Option<&str>,
        usage: Option<&UsageMeta>,
    ) -> std::io::Result<String> {
        self.message_count += 1;
        let msg_id = format!("msg_{:03}", self.message_count);

        let parent_id = if self.message_count > 1 {
            Some(format!("msg_{:03}", self.message_count - 1))
        } else {
            None
        };

        // g155: generate a stable UUID for this entry.
        let new_uuid = Uuid::new_v4().to_string();
        let parent_uuid = parent_uuid_override
            .map(|s| s.to_string())
            .or_else(|| self.last_uuid.clone());

        // g155: for tool results, track which assistant message issued the call.
        let source_tool_assistant_uuid = if matches!(msg.role, crate::message::Role::Tool) {
            // The parent_uuid points to the assistant that issued this tool call.
            parent_uuid.clone()
        } else {
            None
        };

        let role_str = match msg.role {
            crate::message::Role::System => "system",
            crate::message::Role::User => "user",
            crate::message::Role::Assistant => "assistant",
            crate::message::Role::Tool => "tool",
        };

        // g157: track first/last user prompt in memory (written to meta on bump).
        if matches!(msg.role, crate::message::Role::User) {
            let prompt: String = msg.content.chars().take(200).collect();
            if self.first_prompt.is_none() {
                self.first_prompt = Some(prompt.clone());
                // Auto-populate `name` from the first user message when no
                // explicit --name was set.  Truncate to 60 visible chars so
                // the session list stays readable.  `name` can be overridden
                // later with `SessionWriter::set_name`.
                if self.name.is_none() {
                    let title: String = prompt.chars().take(60).collect();
                    self.name = Some(title);
                }
            }
            self.last_prompt = Some(prompt);
        }

        // g156: accumulate usage if provided.
        if let Some(u) = usage {
            self.cumulative_usage.accumulate(u);
        }

        let entry = TranscriptEntry {
            uuid: new_uuid.clone(),
            parent_uuid,
            source_tool_assistant_uuid,
            id: msg_id,
            parent_id,
            role: role_str.to_string(),
            content: msg.content.clone(),
            tool_calls: msg.tool_calls.clone(),
            tool_call_id: msg.tool_call_id.clone(),
            reasoning_content: msg.reasoning_content.clone(),
            usage: usage.cloned(),
            timestamp: chrono_lite_now(),
            audit,
        };

        let line = serde_json::to_string(&entry)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;

        // g155: advance the chain pointer.
        self.last_uuid = Some(new_uuid.clone());

        // Bump updated_at on user/assistant messages so the
        // "most-recent" shortcut on resume picks crashed sessions
        // correctly. Skip on tool results to avoid 2N writes per
        // tool-call-heavy turn.
        if matches!(
            msg.role,
            crate::message::Role::User | crate::message::Role::Assistant
        ) {
            let _ = self.bump_updated_at();
        }

        Ok(new_uuid)
    }

    /// Write a compact_boundary system entry directly to the JSONL (g157).
    ///
    /// Called by `SessionPersistenceSink` when it receives a
    /// `AgentEvent::CompactionBoundary` event. The entry is written outside
    /// the normal `append` flow so it does not increment `message_count` or
    /// advance the UUID chain.
    pub fn write_compact_boundary(
        &mut self,
        turn: u32,
        compacted_count: usize,
        summary_uuid: Option<&str>,
    ) -> std::io::Result<()> {
        let entry = CompactBoundaryEntry {
            entry_type: "system".to_string(),
            subtype: "compact_boundary".to_string(),
            turn: Some(turn),
            compacted_count: Some(compacted_count),
            summary_uuid: summary_uuid.map(|s| s.to_string()),
            timestamp: chrono_lite_now(),
        };
        let line = serde_json::to_string(&entry)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()
    }

    /// Update `updated_at`, `message_count`, `first_prompt`, and `last_prompt`
    /// in `.meta.json`, preserving everything else (goal, model, status, tool hash).
    /// Best-effort: errors are returned but `append()` swallows them
    /// so a transient meta-write failure does not abort the run.
    fn bump_updated_at(&self) -> std::io::Result<()> {
        let meta_path = self.session_dir.join(".meta.json");
        let bytes = std::fs::read(&meta_path)?;
        let mut meta: SessionMeta = serde_json::from_slice(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        meta.updated_at = chrono_lite_now();
        meta.message_count = self.message_count;
        // g157: persist first/last prompt so session picker can show them
        // without reading the full JSONL.
        if self.first_prompt.is_some() {
            meta.first_prompt = self.first_prompt.clone();
        }
        if self.last_prompt.is_some() {
            meta.last_prompt = self.last_prompt.clone();
        }
        // Persist auto-generated or explicitly set display name on every bump
        // so the session list shows the title as soon as the first message lands.
        if self.name.is_some() && meta.name.is_none() {
            meta.name = self.name.clone();
        }
        let json = serde_json::to_string_pretty(&meta)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        crate::atomic::atomic_write(&meta_path, json.as_bytes())
    }

    /// Finalise the session: flush the writer and update the meta file
    /// with the final message count, status, prompts, and cumulative cost.
    ///
    /// `status` is a [`SessionStatus`] enum value. Callers that have a
    /// `FinishReason` in hand should prefer
    /// `SessionStatus::for_finish(&reason)` so the mapping stays
    /// exhaustive — adding a new `FinishReason` variant will not
    /// silently fall back to `Crashed`.
    pub fn finish(&mut self, status: SessionStatus) -> std::io::Result<()> {
        self.writer.flush()?;

        // Read-modify-write so we preserve fields we don't own here
        // (notably `tool_registry_hash`).
        let meta_path = self.session_dir.join(".meta.json");
        let bytes = std::fs::read(&meta_path)?;
        let mut meta: SessionMeta = serde_json::from_slice(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        meta.updated_at = chrono_lite_now();
        meta.message_count = self.message_count;
        meta.status = status;
        // g157: final prompt snapshot.
        if self.first_prompt.is_some() {
            meta.first_prompt = self.first_prompt.clone();
        }
        if self.last_prompt.is_some() {
            meta.last_prompt = self.last_prompt.clone();
        }
        // g156: write cumulative cost.
        if !self.cumulative_usage.is_zero() {
            let mut cost = meta.cost.take().unwrap_or_default();
            cost.accumulate(&self.cumulative_usage);
            meta.cost = Some(cost);
        }
        // Persist the display name if one was set.
        if self.name.is_some() {
            meta.name = self.name.clone();
        }

        let meta_json = serde_json::to_string_pretty(&meta)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        crate::atomic::atomic_write(&meta_path, meta_json.as_bytes())
    }

    /// Set an optional human-readable display name for this session.
    /// The name is persisted to `.meta.json` on the next `finish()` call.
    pub fn set_name(&mut self, name: impl Into<String>) {
        self.name = Some(name.into());
    }

    /// Return the session ID.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Return the session directory path.
    pub fn session_dir(&self) -> &Path {
        &self.session_dir
    }

    /// Return the number of messages written so far.
    pub fn message_count(&self) -> u64 {
        self.message_count
    }

    /// Return the UUID of the last appended message (g155).
    pub fn last_uuid(&self) -> Option<&str> {
        self.last_uuid.as_deref()
    }
}

/// Read the UUID of the last message-type (TranscriptEntry) line in a JSONL
/// file. Skips compact_boundary system entries. Returns `None` if the file
/// is empty, unreadable, or all entries lack a UUID (pre-g155 files).
fn read_last_message_uuid(jsonl_path: &Path) -> Option<String> {
    let file = std::fs::File::open(jsonl_path).ok()?;
    let reader = std::io::BufReader::new(file);
    let mut last = None;
    for line in reader.lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<TranscriptEntry>(&line) {
            if !entry.uuid.is_empty() {
                last = Some(entry.uuid);
            }
        }
    }
    last
}

/// Truncate `transcript.jsonl` (and the session's `.meta.json`
/// `message_count`) so that only the messages from turns
/// `0..cutoff_turn` survive.
///
/// "Turn N" is defined as the N-th non-system, non-tool user message
/// in the transcript (0-indexed). The system prompt (if any) and any
/// seed messages preceding the first user turn are always preserved.
///
/// Used by `recursive sessions rewind --to-turn N` to keep transcript
/// state in sync with the workspace state restored from a checkpoint.
pub fn truncate_transcript_to_turn(
    session_dir: &Path,
    cutoff_turn: usize,
) -> std::io::Result<TruncateStats> {
    let jsonl_path = session_dir.join("transcript.jsonl");
    if !jsonl_path.exists() {
        return Ok(TruncateStats {
            kept: 0,
            dropped: 0,
        });
    }

    // Stream-read so we don't load the whole transcript into memory.
    let file = std::fs::File::open(&jsonl_path)?;
    let reader = std::io::BufReader::new(file);

    let tmp_path = jsonl_path.with_extension("jsonl.rewind-tmp");
    let tmp = std::fs::File::create(&tmp_path)?;
    let mut writer = BufWriter::new(tmp);

    let mut user_seen = 0usize;
    let mut kept = 0u64;
    let mut dropped = 0u64;
    let mut stop = false;

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        if stop {
            dropped += 1;
            continue;
        }

        // Peek role without full deserialisation.
        let role = serde_json::from_str::<serde_json::Value>(&line)
            .ok()
            .and_then(|v| v.get("role").and_then(|r| r.as_str()).map(str::to_string));

        let is_turn_boundary = matches!(role.as_deref(), Some("user"));
        if is_turn_boundary {
            if user_seen >= cutoff_turn {
                // This user message starts the turn we're rewinding;
                // drop it and everything after.
                stop = true;
                dropped += 1;
                continue;
            }
            user_seen += 1;
        }

        writer.write_all(line.as_bytes())?;
        writer.write_all(b"\n")?;
        kept += 1;
    }
    writer.flush()?;
    drop(writer);

    std::fs::rename(&tmp_path, &jsonl_path)?;

    // Update .meta.json message_count if present.
    let meta_path = session_dir.join(".meta.json");
    if meta_path.exists() {
        if let Ok(bytes) = std::fs::read(&meta_path) {
            if let Ok(mut meta) = serde_json::from_slice::<SessionMeta>(&bytes) {
                meta.message_count = kept;
                meta.updated_at = chrono_lite_now();
                if let Ok(json) = serde_json::to_string_pretty(&meta) {
                    let _ = crate::atomic::atomic_write(&meta_path, json.as_bytes());
                }
            }
        }
    }

    Ok(TruncateStats { kept, dropped })
}

/// Stats returned by [`truncate_transcript_to_turn`].
#[derive(Debug, Clone, Copy)]
pub struct TruncateStats {
    pub kept: u64,
    pub dropped: u64,
}

// ---------------------------------------------------------------------------
// Session lock (Goal 151)
// ---------------------------------------------------------------------------
//
// Implementation lives in `crate::session_lock`. Re-exported here so external
// callers using `recursive::session::{SessionLock, SessionLockBusy}` keep
// working unchanged.
pub use crate::session_lock::{SessionLock, SessionLockBusy};

/// Reader for loading sessions from JSONL files.
pub struct SessionReader;

impl SessionReader {
    /// Load all transcript entries from a session directory.
    ///
    /// If the JSONL contains a `compact_boundary` system entry (g157), all
    /// entries **before** the last such boundary are discarded—they were
    /// already summarised and the summary is the first entry after the
    /// boundary. This makes resume `O(post-compaction size)`.
    pub fn load_transcript(session_dir: &Path) -> std::io::Result<Vec<TranscriptEntry>> {
        let jsonl_path = session_dir.join("transcript.jsonl");
        let file = std::fs::File::open(&jsonl_path)?;
        let reader = std::io::BufReader::new(file);

        let mut all_entries: Vec<TranscriptEntry> = Vec::new();
        // Index of the line immediately after the last compact_boundary we saw.
        let mut boundary_after: usize = 0;

        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            // g157: detect compact_boundary system entries before trying to
            // parse as TranscriptEntry (they have a different shape).
            if let Ok(sys) = serde_json::from_str::<CompactBoundaryEntry>(&line) {
                if sys.entry_type == "system" && sys.subtype == "compact_boundary" {
                    // Everything before this boundary is superseded; restart.
                    boundary_after = all_entries.len();
                    continue;
                }
            }
            match serde_json::from_str::<TranscriptEntry>(&line) {
                Ok(entry) => all_entries.push(entry),
                Err(e) => {
                    // Skip corrupt lines gracefully
                    eprintln!(
                        "warning: skipping corrupt line in {}: {}",
                        jsonl_path.display(),
                        e
                    );
                    continue;
                }
            }
        }
        // Discard pre-boundary entries (g157).
        Ok(all_entries.split_off(boundary_after))
    }

    /// Load the transcript and build a UUID → `TranscriptEntry` index
    /// alongside the ordered vec. Enables O(1) lookup by UUID (g155).
    pub fn load_transcript_indexed(
        session_dir: &Path,
    ) -> std::io::Result<(
        Vec<TranscriptEntry>,
        std::collections::HashMap<String, TranscriptEntry>,
    )> {
        let entries = Self::load_transcript(session_dir)?;
        let mut index = std::collections::HashMap::with_capacity(entries.len());
        for entry in &entries {
            if !entry.uuid.is_empty() {
                index.insert(entry.uuid.clone(), entry.clone());
            }
        }
        Ok((entries, index))
    }

    /// Load the transcript and convert each `TranscriptEntry` to a
    /// runtime [`Message`]. Persistence-only fields (`id`,
    /// `parent_id`, `uuid`, `parent_uuid`, `timestamp`, `usage`)
    /// are dropped here. The result is what `run_resumed` expects
    /// as its `seed` argument.
    ///
    /// The `system` role is **kept** in the returned vec; callers
    /// that want to rebuild the system prompt from `Config` can
    /// filter it out manually.
    pub fn load_messages(session_dir: &Path) -> std::io::Result<Vec<Message>> {
        let entries = Self::load_transcript(session_dir)?;
        Ok(entries.into_iter().map(entry_to_message).collect())
    }

    /// Goal-153: scan the transcript for "orphan" tool calls — tool_calls
    /// in the last assistant message that have no matching `tool` reply.
    ///
    /// Returns an empty vec when the transcript is clean (no orphans).
    /// Returns the orphan descriptions when one or more tool calls from the
    /// last assistant message have no corresponding `tool` result message.
    ///
    /// This is the **detection** side of durable execution; the *handling*
    /// (skip / redo / abort) is done by the caller (`cmd_resume`).
    ///
    /// `registry` is used to determine `side_effect_at_call` for orphans
    /// (their `AuditMeta` was never written because the process died before
    /// the call returned).
    pub fn scan_orphan_tool_calls(
        session_dir: &Path,
        registry: &crate::tools::ToolRegistry,
    ) -> std::io::Result<Vec<OrphanToolCall>> {
        let entries = Self::load_transcript(session_dir)?;
        if entries.is_empty() {
            return Ok(Vec::new());
        }

        // Find the last assistant message with tool_calls.
        let last_assistant = entries
            .iter()
            .enumerate()
            .rev()
            .find(|(_, e)| e.role == "assistant" && !e.tool_calls.is_empty());

        let Some((asst_idx, asst_entry)) = last_assistant else {
            return Ok(Vec::new());
        };

        // Collect the set of tool_call_ids that have a matching tool result
        // at any position *after* the assistant message.
        let answered: std::collections::HashSet<String> = entries[asst_idx + 1..]
            .iter()
            .filter(|e| e.role == "tool")
            .filter_map(|e| e.tool_call_id.clone())
            .collect();

        let mut orphans = Vec::new();
        for tc in &asst_entry.tool_calls {
            if !answered.contains(&tc.id) {
                let side_effect = registry
                    .get(&tc.name)
                    .map(|t| t.side_effect_class())
                    .unwrap_or(crate::tools::ToolSideEffect::External);
                let args_hash = {
                    let canonical = tc.arguments.to_string();
                    let hash = blake3::hash(canonical.as_bytes());
                    hash.to_hex().to_string()
                };
                orphans.push(OrphanToolCall {
                    assistant_msg_id: asst_entry.id.clone(),
                    tool_call_id: tc.id.clone(),
                    tool_name: tc.name.clone(),
                    args_hash,
                    side_effect_at_call: side_effect,
                });
            }
        }
        Ok(orphans)
    }

    /// Load the session metadata from a session directory.
    ///
    /// Refuses to deserialize a session whose `schema_version` is
    /// newer than this build supports (Goal 269). Pre-Goal-269
    /// session files have no `schema_version` field and load
    /// successfully via the `#[serde(default)]` on the struct.
    pub fn load_meta(session_dir: &Path) -> std::io::Result<SessionMeta> {
        let meta_path = session_dir.join(".meta.json");
        let bytes = std::fs::read(&meta_path)?;
        let meta: SessionMeta = serde_json::from_slice(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if meta.schema_version > SUPPORTED_SESSION_SCHEMA_VERSION {
            let err = crate::error::Error::SchemaTooNew {
                session_id: meta.session_id.clone(),
                found: meta.schema_version,
                supported: SUPPORTED_SESSION_SCHEMA_VERSION,
            };
            tracing::warn!("{}", err);
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                err.to_string(),
            ));
        }
        Ok(meta)
    }

    /// List all session directories for a given workspace.
    ///
    /// Returns a list of session directories sorted by name (which is
    /// timestamp-prefixed, so chronological).
    pub fn list_sessions(workspace: &Path) -> std::io::Result<Vec<PathBuf>> {
        let base = match crate::paths::user_sessions_dir(workspace) {
            Ok(d) => d,
            Err(_) => workspace.join(".recursive").join("sessions"),
        };
        if !base.is_dir() {
            return Ok(Vec::new());
        }

        let mut sessions = Vec::new();
        // Iterate workspace slugs
        for entry in std::fs::read_dir(&base)? {
            let entry = entry?;
            let slug_dir = entry.path();
            if !slug_dir.is_dir() {
                continue;
            }
            // Iterate session IDs within each slug
            for session_entry in std::fs::read_dir(&slug_dir)? {
                let session_entry = session_entry?;
                let session_dir = session_entry.path();
                if session_dir.is_dir() && session_dir.join(".meta.json").is_file() {
                    sessions.push(session_dir);
                }
            }
        }
        sessions.sort();
        Ok(sessions)
    }

    /// List all session directories sorted by `.meta.json`
    /// `updated_at` descending (most recently active first).
    ///
    /// Used by `recursive resume` (g151) to pick the most-recent
    /// session when no ID is given. Tiebreaks: when two sessions
    /// share the same `updated_at` string (RFC3339 has 1-second
    /// granularity, so ties happen during fast tests), fall back
    /// to `transcript.jsonl` mtime, then session_id lexicographically.
    ///
    /// Sessions whose `.meta.json` cannot be read are silently
    /// excluded — they're either being created or corrupted.
    pub fn list_sessions_sorted_by_updated_at(
        workspace: &Path,
    ) -> std::io::Result<Vec<(PathBuf, SessionMeta)>> {
        let dirs = Self::list_sessions(workspace)?;

        let mut entries: Vec<(PathBuf, SessionMeta, std::time::SystemTime)> = Vec::new();
        for dir in dirs {
            let meta = match Self::load_meta(&dir) {
                Ok(m) => m,
                Err(_) => continue,
            };
            // Tiebreak: mtime of transcript.jsonl. Falls back to
            // UNIX_EPOCH if the file doesn't exist or stat fails.
            let mtime = std::fs::metadata(dir.join("transcript.jsonl"))
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            entries.push((dir, meta, mtime));
        }

        entries.sort_by(|a, b| {
            // Primary: updated_at desc (lexicographic on RFC3339
            // is chronological, so reverse comparison sorts desc).
            b.1.updated_at
                .cmp(&a.1.updated_at)
                // Secondary: mtime desc.
                .then_with(|| b.2.cmp(&a.2))
                // Tertiary: session_id asc (deterministic).
                .then_with(|| a.1.session_id.cmp(&b.1.session_id))
        });

        Ok(entries.into_iter().map(|(p, m, _)| (p, m)).collect())
    }

    /// List all session directories across all workspaces under a base path.
    pub fn list_all_sessions(base: &Path) -> std::io::Result<Vec<PathBuf>> {
        let sessions_dir = base.join(".recursive").join("sessions");
        if !sessions_dir.is_dir() {
            return Ok(Vec::new());
        }

        let mut sessions = Vec::new();
        for entry in std::fs::read_dir(&sessions_dir)? {
            let entry = entry?;
            let slug_dir = entry.path();
            if !slug_dir.is_dir() {
                continue;
            }
            for session_entry in std::fs::read_dir(&slug_dir)? {
                let session_entry = session_entry?;
                let session_dir = session_entry.path();
                if session_dir.is_dir() && session_dir.join(".meta.json").is_file() {
                    sessions.push(session_dir);
                }
            }
        }
        sessions.sort();
        Ok(sessions)
    }
}

/// Convert an absolute workspace path into a filesystem-safe slug.
///
/// - Replaces `/` with `-`
/// - Strips leading `-` (from the root `/`)
/// - Truncates to 80 characters
fn workspace_slug(workspace: &Path) -> String {
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
// SessionPersistenceSink
// ---------------------------------------------------------------------------

/// An [`EventSink`] that persists every [`AgentEvent::MessageAppended`] event
/// to the session transcript file.
///
/// Wraps an `Arc<Mutex<SessionWriter>>` and calls
/// [`SessionWriter::append`] on every `MessageAppended` event. All other
/// event variants are silently ignored.
///
/// Persistence failures are non-fatal for the agent run but are logged at
/// `error` level because a missing line on disk would silently break
/// downstream orphan detection (g153).
///
/// # Locking notes
///
/// `SessionWriter` is not `Send` across `.await` points, so we keep the
/// mutex non-`async` (`std::sync::Mutex`). The critical section inside
/// `emit` is purely synchronous I/O — one `serde_json::to_string` +
/// `write_all` + `flush` per message — matching every other consumer of
/// `Arc<Mutex<SessionWriter>>` in the codebase.
pub struct SessionPersistenceSink {
    writer: Arc<std::sync::Mutex<SessionWriter>>,
}

impl SessionPersistenceSink {
    /// Create a new `SessionPersistenceSink` backed by the given writer.
    pub fn new(writer: Arc<std::sync::Mutex<SessionWriter>>) -> Self {
        Self { writer }
    }
}

#[async_trait::async_trait]
impl EventSink for SessionPersistenceSink {
    async fn emit(&self, event: AgentEvent) {
        match event {
            AgentEvent::MessageAppended { message, usage } => {
                let result = {
                    match self.writer.lock() {
                        Ok(mut w) => w.append_with_audit(&message, None, None, usage.as_ref()),
                        Err(poisoned) => {
                            let mut w = poisoned.into_inner();
                            w.append_with_audit(&message, None, None, usage.as_ref())
                        }
                    }
                };
                if let Err(e) = result {
                    tracing::error!("session persistence: failed to append message: {e}");
                }
            }
            AgentEvent::MessageAppendedWithAudit { message, audit } => {
                // Goal 153: tool result with audit metadata.
                let result = {
                    match self.writer.lock() {
                        Ok(mut w) => w.append_with_audit(&message, Some(audit), None, None),
                        Err(poisoned) => {
                            let mut w = poisoned.into_inner();
                            w.append_with_audit(&message, Some(audit), None, None)
                        }
                    }
                };
                if let Err(e) = result {
                    tracing::error!("session persistence: failed to append audited message: {e}");
                }
            }
            AgentEvent::CompactionBoundary {
                turn,
                compacted_count,
                summary_uuid,
            } => {
                // g157: write a compact_boundary system entry so resume can
                // skip the pre-compaction messages.
                let result = match self.writer.lock() {
                    Ok(mut w) => {
                        w.write_compact_boundary(turn, compacted_count, summary_uuid.as_deref())
                    }
                    Err(poisoned) => {
                        let mut w = poisoned.into_inner();
                        w.write_compact_boundary(turn, compacted_count, summary_uuid.as_deref())
                    }
                };
                if let Err(e) = result {
                    tracing::error!("session persistence: failed to write compact_boundary: {e}");
                }
            }
            _ => {}
        }
    }
}

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

    // -----------------------------------------------------------------------
    // JSONL session tests (Goal 107)
    // -----------------------------------------------------------------------

    #[test]
    fn session_writer_creates_meta_and_jsonl() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let ws = tmp.path();

        let mut writer = SessionWriter::create(ws, "test goal", "gpt-4o", "openai").unwrap();

        let session_dir = writer.session_dir().to_path_buf();
        assert!(session_dir.join("transcript.jsonl").is_file());
        assert!(session_dir.join(".meta.json").is_file());

        // Verify meta
        let meta = SessionReader::load_meta(&session_dir).unwrap();
        assert_eq!(meta.goal, "test goal");
        assert_eq!(meta.model, "gpt-4o");
        assert_eq!(meta.provider, "openai");
        assert_eq!(meta.message_count, 0);
        assert_eq!(meta.status, SessionStatus::Active);

        writer.finish(SessionStatus::Completed).unwrap();

        let meta = SessionReader::load_meta(&session_dir).unwrap();
        assert_eq!(meta.message_count, 0);
        assert_eq!(meta.status, SessionStatus::Completed);
    }

    #[test]
    fn session_writer_appends_lines() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let ws = tmp.path();

        let mut writer = SessionWriter::create(ws, "test", "gpt-4o", "openai").unwrap();

        let id1 = writer.append(&Message::user("hello"), None, None).unwrap();
        // append now returns a UUID v4 (g155); just verify it's unique and non-empty
        assert_eq!(id1.len(), 36, "uuid should be 36 chars");

        let id2 = writer
            .append(&Message::assistant("hi there"), None, None)
            .unwrap();
        assert_eq!(id2.len(), 36);
        assert_ne!(id1, id2, "each message gets a unique uuid");

        let session_dir = writer.session_dir().to_path_buf();
        writer.finish(SessionStatus::Completed).unwrap();

        // Load and verify — sequential id/parent_id are still written (g155 compat)
        let entries = SessionReader::load_transcript(&session_dir).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "msg_001");
        assert_eq!(entries[0].parent_id, None);
        assert_eq!(entries[0].parent_uuid, None, "root has no parent_uuid");
        assert!(!entries[0].uuid.is_empty(), "uuid must be present");
        assert_eq!(entries[0].role, "user");
        assert_eq!(entries[0].content, "hello");

        assert_eq!(entries[1].id, "msg_002");
        assert_eq!(entries[1].parent_id, Some("msg_001".to_string()));
        assert_eq!(
            entries[1].parent_uuid,
            Some(entries[0].uuid.clone()),
            "parent_uuid points to first entry"
        );
        assert_eq!(entries[1].role, "assistant");
        assert_eq!(entries[1].content, "hi there");
    }

    #[test]
    fn session_reader_round_trips() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let ws = tmp.path();

        let mut writer = SessionWriter::create(ws, "round trip", "gpt-4o", "openai").unwrap();

        writer
            .append(&Message::system("You are a bot."), None, None)
            .unwrap();
        writer
            .append(&Message::user("do something"), None, None)
            .unwrap();
        writer
            .append(&Message::assistant("I will do it."), None, None)
            .unwrap();

        let session_dir = writer.session_dir().to_path_buf();
        writer.finish(SessionStatus::Completed).unwrap();

        let entries = SessionReader::load_transcript(&session_dir).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].role, "system");
        assert_eq!(entries[1].role, "user");
        assert_eq!(entries[2].role, "assistant");
    }

    #[test]
    fn session_writer_finish_updates_meta() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let ws = tmp.path();

        let mut writer = SessionWriter::create(ws, "meta test", "gpt-4o", "openai").unwrap();
        writer.append(&Message::user("msg1"), None, None).unwrap();
        writer
            .append(&Message::assistant("msg2"), None, None)
            .unwrap();
        let session_dir = writer.session_dir().to_path_buf();
        writer.finish(SessionStatus::Completed).unwrap();

        let meta = SessionReader::load_meta(&session_dir).unwrap();
        assert_eq!(meta.message_count, 2);
        assert_eq!(meta.status, SessionStatus::Completed);
    }

    /// Preset-config goal: the `preset` field is recorded on
    /// `SessionMeta` so a future reader can see "this run was on
    /// deepseek" without re-deriving from `api_base`. Verifies the
    /// round-trip: create with `Some("deepseek")`, reload, assert
    /// the field is preserved. Also confirms the default (None)
    /// path is not serialized at all (`skip_serializing_if`) so
    /// pre-preset-config session files stay byte-compat on read.
    #[test]
    fn session_meta_preserves_preset_field() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let ws = tmp.path();

        // With a preset — should round-trip. The writer is dropped
        // before the second writer is created so the per-session
        // lock is released; otherwise the second `create_with_tools`
        // would fail with SessionLockBusy when both session_ids
        // collide on the same second.
        let (_session_dir, preset_after) = {
            let mut writer = SessionWriter::create_with_tools(
                ws,
                "preset run",
                "deepseek-chat",
                "openai",
                &[],
                Some("deepseek"),
            )
            .unwrap();
            let dir = writer.session_dir().to_path_buf();
            writer.finish(SessionStatus::Completed).unwrap();
            let meta = SessionReader::load_meta(&dir).unwrap();
            (dir, meta.preset)
        };
        assert_eq!(preset_after.as_deref(), Some("deepseek"));

        // Without a preset — `None` is kept on read, and the JSON
        // should not include a `preset` key (skip_serializing_if).
        let writer2 =
            SessionWriter::create_with_tools(ws, "no preset run", "gpt-4o", "openai", &[], None)
                .unwrap();
        let session_dir2 = writer2.session_dir().to_path_buf();
        drop(writer2);
        let meta2 = SessionReader::load_meta(&session_dir2).unwrap();
        assert!(meta2.preset.is_none());
        let raw = std::fs::read_to_string(session_dir2.join(".meta.json")).unwrap();
        assert!(
            !raw.contains("\"preset\""),
            "preset key should be absent when None, got: {raw}"
        );
    }

    /// `name` is auto-filled from the first user prompt when no explicit
    /// `--name` was supplied.
    #[test]
    fn session_name_autofills_from_first_prompt() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let ws = tmp.path();

        let mut writer = SessionWriter::create(ws, "test goal", "gpt-4o", "openai").unwrap();
        let session_dir = writer.session_dir().to_path_buf();

        // No --name set; name should be None before any message.
        assert!(writer.name.is_none());

        // After the first user message the writer should have auto-populated name.
        writer
            .append(&Message::user("hello world".to_string()), None, None)
            .unwrap();
        assert_eq!(writer.name.as_deref(), Some("hello world"));

        writer.finish(SessionStatus::Completed).unwrap();

        // name should be persisted in meta.json.
        let meta = SessionReader::load_meta(&session_dir).unwrap();
        assert_eq!(meta.name.as_deref(), Some("hello world"));
    }

    /// When --name is set explicitly, auto-fill must NOT overwrite it.
    #[test]
    fn explicit_name_not_overwritten_by_autofill() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let ws = tmp.path();

        let mut writer = SessionWriter::create(ws, "test goal", "gpt-4o", "openai").unwrap();
        writer.set_name("my custom name".to_string());
        let session_dir = writer.session_dir().to_path_buf();

        writer
            .append(&Message::user("hello world".to_string()), None, None)
            .unwrap();
        // Auto-fill should not replace the explicitly set name.
        assert_eq!(writer.name.as_deref(), Some("my custom name"));

        writer.finish(SessionStatus::Completed).unwrap();

        let meta = SessionReader::load_meta(&session_dir).unwrap();
        assert_eq!(meta.name.as_deref(), Some("my custom name"));
    }

    /// Long prompts are truncated to 60 visible chars in the auto-filled name.
    #[test]
    fn autofill_name_truncates_long_prompt() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let ws = tmp.path();

        let long_prompt = "a".repeat(100);
        let mut writer = SessionWriter::create(ws, "test goal", "gpt-4o", "openai").unwrap();
        writer
            .append(&Message::user(long_prompt.clone()), None, None)
            .unwrap();
        writer.finish(SessionStatus::Completed).unwrap();

        let session_dir = writer.session_dir().to_path_buf();
        let meta = SessionReader::load_meta(&session_dir).unwrap();
        assert_eq!(
            meta.name.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa") // 60 'a's
        );
    }

    #[test]
    fn list_sessions_finds_sessions() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let ws = tmp.path();

        // No sessions yet
        let sessions = SessionReader::list_sessions(ws).unwrap();
        assert!(sessions.is_empty());

        // Create one session
        let writer = SessionWriter::create(ws, "session1", "gpt-4o", "openai").unwrap();
        let dir1 = writer.session_dir().to_path_buf();
        drop(writer);

        let sessions = SessionReader::list_sessions(ws).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0], dir1);
    }

    #[test]
    fn crash_partial_line_skipped() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let ws = tmp.path();

        let mut writer = SessionWriter::create(ws, "crash test", "gpt-4o", "openai").unwrap();
        writer
            .append(&Message::user("good line"), None, None)
            .unwrap();
        let session_dir = writer.session_dir().to_path_buf();
        writer.finish(SessionStatus::Crashed).unwrap();

        // Append a corrupt line manually
        use std::io::Write;
        let jsonl_path = session_dir.join("transcript.jsonl");
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&jsonl_path)
            .unwrap();
        writeln!(f, "this is not json").unwrap();
        drop(f);

        // Should still load the good line
        let entries = SessionReader::load_transcript(&session_dir).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "good line");
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

    #[test]
    fn truncate_transcript_to_turn_drops_at_user_boundary() {
        let dir = crate::test_util::IsolatedWorkspace::new();
        let mut w = SessionWriter::create(dir.path(), "g", "m", "p").unwrap();
        // Sequence: system, user(turn 0), assistant, user(turn 1),
        // assistant, user(turn 2), assistant.
        w.append(&Message::system("sys".to_string()), None, None)
            .unwrap();
        w.append(&Message::user("u0".to_string()), None, None)
            .unwrap();
        w.append(&Message::assistant("a0".to_string()), None, None)
            .unwrap();
        w.append(&Message::user("u1".to_string()), None, None)
            .unwrap();
        w.append(&Message::assistant("a1".to_string()), None, None)
            .unwrap();
        w.append(&Message::user("u2".to_string()), None, None)
            .unwrap();
        w.append(&Message::assistant("a2".to_string()), None, None)
            .unwrap();
        w.finish(SessionStatus::Completed).unwrap();

        let session_dir = w.session_dir().to_path_buf();

        // Rewind to turn 1 → keep system + u0 + a0; drop u1 onwards.
        let stats = truncate_transcript_to_turn(&session_dir, 1).unwrap();
        assert_eq!(stats.kept, 3);
        assert_eq!(stats.dropped, 4);

        let entries = SessionReader::load_transcript(&session_dir).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].role, "system");
        assert_eq!(entries[1].role, "user");
        assert_eq!(entries[1].content, "u0");
        assert_eq!(entries[2].role, "assistant");
        assert_eq!(entries[2].content, "a0");

        // Meta should reflect the new count.
        let meta = SessionReader::load_meta(&session_dir).unwrap();
        assert_eq!(meta.message_count, 3);
    }

    #[test]
    fn truncate_transcript_to_zero_drops_all_turns_keeps_system() {
        let dir = crate::test_util::IsolatedWorkspace::new();
        let mut w = SessionWriter::create(dir.path(), "g", "m", "p").unwrap();
        w.append(&Message::system("sys".to_string()), None, None)
            .unwrap();
        w.append(&Message::user("u0".to_string()), None, None)
            .unwrap();
        w.append(&Message::assistant("a0".to_string()), None, None)
            .unwrap();
        w.finish(SessionStatus::Completed).unwrap();
        let session_dir = w.session_dir().to_path_buf();

        let stats = truncate_transcript_to_turn(&session_dir, 0).unwrap();
        assert_eq!(stats.kept, 1, "system message should remain");
        assert_eq!(stats.dropped, 2);

        let entries = SessionReader::load_transcript(&session_dir).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].role, "system");
    }

    #[test]
    fn truncate_transcript_missing_file_is_noop() {
        let dir = crate::test_util::IsolatedWorkspace::new();
        // No session created → no transcript.jsonl. Should not panic.
        let stats = truncate_transcript_to_turn(dir.path(), 5).unwrap();
        assert_eq!(stats.kept, 0);
        assert_eq!(stats.dropped, 0);
    }

    // ---------------------------------------------------------------
    // Goal 151: resume by ID — new test coverage
    // ---------------------------------------------------------------

    #[test]
    fn load_messages_drops_persistence_fields() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let ws = tmp.path();
        let mut writer = SessionWriter::create(ws, "g151 test", "model", "openai").unwrap();
        let session_dir = writer.session_dir().to_path_buf();

        writer
            .append(&Message::user("hello".to_string()), None, None)
            .unwrap();
        writer
            .append(&Message::assistant("hi back".to_string()), None, None)
            .unwrap();
        writer.finish(SessionStatus::Completed).unwrap();
        drop(writer);

        // load_messages strips id / parent_id / timestamp.
        let msgs = SessionReader::load_messages(&session_dir).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[0].content, "hello");
        assert_eq!(msgs[1].role, Role::Assistant);
        assert_eq!(msgs[1].content, "hi back");

        // Confirm the persisted entries actually had the fields we
        // claim to drop, so this test isn't a no-op.
        let entries = SessionReader::load_transcript(&session_dir).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "msg_001");
        assert!(!entries[0].timestamp.is_empty());
    }

    #[test]
    fn meta_round_trip_with_tool_registry_hash() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let ws = tmp.path();

        let specs = vec![ToolSpec {
            name: "Read".into(),
            description: "Read".into(),
            parameters: serde_json::json!({"type":"object"}),
        }];
        let writer =
            SessionWriter::create_with_tools(ws, "with hash", "model", "openai", &specs, None)
                .unwrap();
        let session_dir = writer.session_dir().to_path_buf();
        drop(writer);

        let meta = SessionReader::load_meta(&session_dir).unwrap();
        let hash = meta
            .tool_registry_hash
            .as_ref()
            .expect("expected hash to be Some(_)");
        assert_eq!(*hash, hash_tool_specs(&specs));
    }

    #[test]
    fn meta_round_trip_old_format_no_hash() {
        // Synthesise a `.meta.json` that lacks the `tool_registry_hash`
        // field (representing a pre-g151 session record). Reload and
        // confirm it parses cleanly with `tool_registry_hash: None`.
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let session_dir = tmp
            .path()
            .join(".recursive")
            .join("sessions")
            .join("legacy");
        std::fs::create_dir_all(&session_dir).unwrap();

        let raw = r#"{
  "session_id": "legacy-id",
  "goal": "old goal",
  "model": "model",
  "provider": "openai",
  "created_at": "2020-01-01T00:00:00Z",
  "updated_at": "2020-01-01T00:00:00Z",
  "message_count": 0,
  "status": "active"
}"#;
        std::fs::write(session_dir.join(".meta.json"), raw).unwrap();
        std::fs::write(session_dir.join("transcript.jsonl"), "").unwrap();

        let meta = SessionReader::load_meta(&session_dir).unwrap();
        assert_eq!(meta.session_id, "legacy-id");
        assert!(meta.tool_registry_hash.is_none());
    }

    #[test]
    fn append_bumps_updated_at() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let ws = tmp.path();
        let mut writer = SessionWriter::create(ws, "bump", "model", "openai").unwrap();
        let session_dir = writer.session_dir().to_path_buf();

        let meta_before = SessionReader::load_meta(&session_dir).unwrap();
        // Sleep a hair past the 1-sec timestamp granularity so the
        // RFC3339 string actually changes. chrono_lite_now() rounds
        // to the second.
        std::thread::sleep(std::time::Duration::from_millis(1100));

        writer
            .append(&Message::user("ping".to_string()), None, None)
            .unwrap();

        let meta_after = SessionReader::load_meta(&session_dir).unwrap();
        assert_ne!(
            meta_before.updated_at, meta_after.updated_at,
            "expected updated_at to advance after append; before={} after={}",
            meta_before.updated_at, meta_after.updated_at
        );
    }

    #[test]
    fn open_existing_continues_msg_numbering() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let ws = tmp.path();
        let mut writer = SessionWriter::create(ws, "resume-num", "model", "openai").unwrap();
        let session_dir = writer.session_dir().to_path_buf();

        writer
            .append(&Message::user("u1".to_string()), None, None)
            .unwrap();
        writer
            .append(&Message::assistant("a1".to_string()), None, None)
            .unwrap();
        writer
            .append(&Message::user("u2".to_string()), None, None)
            .unwrap();
        // Drop the writer WITHOUT calling finish() — the lock file is
        // released on Drop, but we never marked the session done.
        drop(writer);

        // Re-open and append more.
        let mut writer2 = SessionWriter::open_existing(&session_dir).unwrap();
        let id = writer2
            .append(&Message::assistant("a2".to_string()), None, None)
            .unwrap();
        // append now returns a UUID; just verify it's non-empty
        assert!(!id.is_empty(), "expected non-empty UUID from append");
        drop(writer2);

        let entries = SessionReader::load_transcript(&session_dir).unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].id, "msg_001");
        assert_eq!(entries[1].id, "msg_002");
        assert_eq!(entries[2].id, "msg_003");
        assert_eq!(entries[3].id, "msg_004");
        assert_eq!(entries[3].parent_id.as_deref(), Some("msg_003"));
    }

    #[test]
    fn lock_alive_pid_blocks_acquire() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let session_dir = tmp.path().join("session-A");
        std::fs::create_dir_all(&session_dir).unwrap();

        // First acquire succeeds; lock file now holds OUR pid.
        let lock = SessionLock::acquire(&session_dir).unwrap();

        // Second acquire by the same process: pid is alive (it's
        // us!), so it must refuse.
        let err = SessionLock::acquire(&session_dir).expect_err("second acquire should fail");
        // Match the inner SessionLockBusy via Display.
        assert!(
            err.to_string()
                .contains(&format!("pid {}", std::process::id())),
            "expected error to mention our pid {}, got: {}",
            std::process::id(),
            err
        );

        drop(lock);
    }

    // Other lock tests (dead-pid recovery, cross-host abort, drop release)
    // live in `crate::session_lock` because they poke at the implementation
    // internals (`SentinelInfo`, `SESSION_LOCK_FILE`, `current_hostname`).

    // -- SessionPersistenceSink tests --------------------------------------

    fn make_isolated_writer() -> (
        crate::test_util::IsolatedWorkspace,
        Arc<std::sync::Mutex<SessionWriter>>,
    ) {
        let ws = crate::test_util::IsolatedWorkspace::new();
        let writer = SessionWriter::create(ws.path(), "test goal", "gpt-4o", "openai").unwrap();
        (ws, Arc::new(std::sync::Mutex::new(writer)))
    }

    /// `SessionPersistenceSink` appends a message with all fields to disk,
    /// and the round-trip preserves content, tool_calls, and reasoning_content.
    #[tokio::test]
    async fn message_appended_round_trips_through_sink() {
        use crate::event::{AgentEvent, EventSink};
        use crate::llm::ToolCall as LlmToolCall;

        let (_ws, sw) = make_isolated_writer();
        let sink = SessionPersistenceSink::new(sw.clone());

        let tc = LlmToolCall {
            id: "call_1".into(),
            name: "my_tool".into(),
            arguments: serde_json::json!({"x": 1}),
        };
        let msg = Message {
            role: Role::Assistant,
            content: "response text".into(),
            tool_calls: vec![tc],
            tool_call_id: None,
            reasoning_content: Some("I thought about it".into()),
            is_compaction_summary: false,
        };
        sink.emit(AgentEvent::MessageAppended {
            message: msg.clone(),
            usage: None,
        })
        .await;

        // Other events are silently ignored.
        sink.emit(AgentEvent::PlanConfirmed).await;

        let session_dir = sw.lock().unwrap().session_dir().to_path_buf();
        drop(sink);
        drop(sw);

        let transcript = SessionReader::load_messages(&session_dir).unwrap();
        assert_eq!(transcript.len(), 1, "exactly one message written");
        let loaded = &transcript[0];
        assert_eq!(loaded.content, "response text");
        assert_eq!(
            loaded.reasoning_content.as_deref(),
            Some("I thought about it")
        );
        assert_eq!(loaded.tool_calls.len(), 1);
        assert_eq!(loaded.tool_calls[0].name, "my_tool");
    }

    /// A poisoned mutex is recovered gracefully: subsequent `emit` calls
    /// still append and do not panic.
    #[tokio::test]
    async fn sink_recovers_from_poisoned_mutex() {
        use crate::event::{AgentEvent, EventSink};

        let (_ws, sw) = make_isolated_writer();
        let session_dir = sw.lock().unwrap().session_dir().to_path_buf();

        // Poison the mutex by panicking inside a lock guard on another thread.
        let sw2 = sw.clone();
        let _ = std::panic::catch_unwind(move || {
            let _guard = sw2.lock().unwrap();
            panic!("intentional poison");
        });
        assert!(sw.is_poisoned(), "mutex must be poisoned after the panic");

        let sink = SessionPersistenceSink::new(sw);
        // This must not panic even though the mutex is poisoned.
        sink.emit(AgentEvent::MessageAppended {
            message: Message::user("after poison"),
            usage: None,
        })
        .await;

        let transcript = SessionReader::load_messages(&session_dir).unwrap();
        assert_eq!(transcript.len(), 1);
        assert_eq!(transcript[0].content, "after poison");
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
}
