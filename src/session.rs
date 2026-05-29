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

use crate::llm::ToolSpec;
use crate::message::Message;

/// Current schema version for session files.
/// Increment when the format changes in a breaking way.
const SESSION_SCHEMA_VERSION: u32 = 1;

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
        }
    }

    /// Write the session to a JSON file at `path`.
    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, json)
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
fn hash_tool_specs(specs: &[ToolSpec]) -> String {
    use std::collections::BTreeMap;

    let mut map: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    for spec in specs {
        let value = serde_json::json!({
            "description": spec.description,
            "parameters": spec.parameters,
        });
        map.insert(spec.name.clone(), value);
    }
    let canonical = serde_json::to_string(&map).unwrap_or_default();
    let hash = blake3::hash(canonical.as_bytes());
    hash.to_hex().to_string()
}

/// Default session output path for a given workspace.
/// Returns `<workspace>/.recursive/sessions/<timestamp>-<goal-prefix>.json`.
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
    workspace
        .join(".recursive")
        .join("sessions")
        .join(format!("{}-{}.json", ts, prefix))
}

/// List all session files in a workspace's session directory.
pub fn list_sessions(workspace: &Path) -> std::io::Result<Vec<PathBuf>> {
    let dir = workspace.join(".recursive").join("sessions");
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

// Tiny RFC3339-ish timestamp without pulling in `chrono`. Format:
// "YYYY-MM-DDTHH:MM:SSZ" using UTC.
fn chrono_lite_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let day = secs / 86_400;
    let sec_of_day = secs % 86_400;
    let (h, m, s) = (sec_of_day / 3600, (sec_of_day / 60) % 60, sec_of_day % 60);
    let (y, mo, d) = epoch_day_to_ymd(day as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

fn epoch_day_to_ymd(z: i64) -> (i64, u32, u32) {
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

/// Metadata for a JSONL session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_id: String,
    pub goal: String,
    pub model: String,
    pub provider: String,
    pub created_at: String,
    pub updated_at: String,
    pub message_count: u64,
    pub status: String,
}

/// A portable exported transcript for sharing and analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportedTranscript {
    pub version: u32,
    pub session_id: String,
    pub model: String,
    pub goal: String,
    pub created_at: String,
    pub status: String,
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
    pub timestamp: String,
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
    created_at: String,
    goal: String,
    model: String,
    provider: String,
}

impl SessionWriter {
    /// Create a new session in the given workspace directory.
    ///
    /// The session directory is `<workspace>/.recursive/sessions/<workspace-slug>/<session-id>/`.
    /// The `.jsonl` file and `.meta.json` are placed inside that directory.
    pub fn create(
        workspace: &Path,
        goal: &str,
        model: &str,
        provider: &str,
    ) -> std::io::Result<Self> {
        let slug = workspace_slug(workspace);
        let session_id = format!("{}-{}", filesystem_safe_timestamp(), slug);
        let session_dir = workspace
            .join(".recursive")
            .join("sessions")
            .join(&slug)
            .join(&session_id);

        std::fs::create_dir_all(&session_dir)?;

        let jsonl_path = session_dir.join("transcript.jsonl");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&jsonl_path)?;

        let now = chrono_lite_now();
        let meta = SessionMeta {
            session_id: session_id.clone(),
            goal: goal.to_string(),
            model: model.to_string(),
            provider: provider.to_string(),
            created_at: now.clone(),
            updated_at: now.clone(),
            message_count: 0,
            status: "active".to_string(),
        };

        // Write initial meta file
        let meta_path = session_dir.join(".meta.json");
        let meta_json = serde_json::to_string_pretty(&meta)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(&meta_path, meta_json)?;

        Ok(Self {
            session_id,
            session_dir,
            writer: BufWriter::new(file),
            message_count: 0,
            created_at: now,
            goal: goal.to_string(),
            model: model.to_string(),
            provider: provider.to_string(),
        })
    }

    /// Append a message to the JSONL file.
    ///
    /// Returns the assigned message ID (e.g. `msg_001`).
    pub fn append(&mut self, msg: &Message) -> std::io::Result<String> {
        self.message_count += 1;
        let msg_id = format!("msg_{:03}", self.message_count);

        let parent_id = if self.message_count > 1 {
            Some(format!("msg_{:03}", self.message_count - 1))
        } else {
            None
        };

        let role_str = match msg.role {
            crate::message::Role::System => "system",
            crate::message::Role::User => "user",
            crate::message::Role::Assistant => "assistant",
            crate::message::Role::Tool => "tool",
        };

        let entry = TranscriptEntry {
            id: msg_id,
            parent_id,
            role: role_str.to_string(),
            content: msg.content.clone(),
            tool_calls: msg.tool_calls.clone(),
            tool_call_id: msg.tool_call_id.clone(),
            reasoning_content: msg.reasoning_content.clone(),
            timestamp: chrono_lite_now(),
        };

        let line = serde_json::to_string(&entry)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;

        Ok(format!("msg_{:03}", self.message_count))
    }

    /// Finalise the session: flush the writer and update the meta file
    /// with the final message count and status.
    pub fn finish(&mut self, status: &str) -> std::io::Result<()> {
        self.writer.flush()?;

        let meta = SessionMeta {
            session_id: self.session_id.clone(),
            goal: self.goal.clone(),
            model: self.model.clone(),
            provider: self.provider.clone(),
            created_at: self.created_at.clone(),
            updated_at: chrono_lite_now(),
            message_count: self.message_count,
            status: status.to_string(),
        };

        let meta_path = self.session_dir.join(".meta.json");
        let meta_json = serde_json::to_string_pretty(&meta)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(&meta_path, meta_json)
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
                    let _ = std::fs::write(&meta_path, json);
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

/// Reader for loading sessions from JSONL files.
pub struct SessionReader;

impl SessionReader {
    /// Load all transcript entries from a session directory.
    pub fn load_transcript(session_dir: &Path) -> std::io::Result<Vec<TranscriptEntry>> {
        let jsonl_path = session_dir.join("transcript.jsonl");
        let file = std::fs::File::open(&jsonl_path)?;
        let reader = std::io::BufReader::new(file);

        let mut entries = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<TranscriptEntry>(&line) {
                Ok(entry) => entries.push(entry),
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
        Ok(entries)
    }

    /// Load the session metadata from a session directory.
    pub fn load_meta(session_dir: &Path) -> std::io::Result<SessionMeta> {
        let meta_path = session_dir.join(".meta.json");
        let bytes = std::fs::read(&meta_path)?;
        serde_json::from_slice(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// List all session directories for a given workspace.
    ///
    /// Returns a list of session directories sorted by name (which is
    /// timestamp-prefixed, so chronological).
    pub fn list_sessions(workspace: &Path) -> std::io::Result<Vec<PathBuf>> {
        let base = workspace.join(".recursive").join("sessions");
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

    let s: String = abs
        .to_string_lossy()
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' => '-',
            c if c.is_control() => '-',
            c => c,
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
                name: "read_file".into(),
                description: "Read a file".into(),
                parameters: serde_json::json!({"type":"object"}),
            },
            ToolSpec {
                name: "write_file".into(),
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
            name: "read_file".into(),
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
            name: "write_file".into(),
            description: "Write a file".into(),
            parameters: serde_json::json!({"type":"object"}),
        }];
        assert!(session.validate_tool_registry(&different_specs).is_err());
    }

    #[test]
    #[cfg_attr(target_os = "windows", ignore)]
    fn session_list_finds_files_in_workspace() {
        let tmp = tempfile::tempdir().unwrap();
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
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "/tmp/foo.rs"}),
            },
            ToolCall {
                id: "call_002".to_string(),
                name: "write_file".to_string(),
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
        assert_eq!(assistant_msg.tool_calls[0].name, "read_file");
        assert_eq!(
            assistant_msg.tool_calls[0].arguments,
            serde_json::json!({"path": "/tmp/foo.rs"})
        );
        assert_eq!(assistant_msg.tool_calls[1].id, "call_002");
        assert_eq!(assistant_msg.tool_calls[1].name, "write_file");
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
        let tmp = tempfile::tempdir().unwrap();
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
                name: "read_file".into(),
                description: "Read a file".into(),
                parameters: serde_json::json!({"type":"object"}),
            },
            ToolSpec {
                name: "write_file".into(),
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
        let tmp = tempfile::tempdir().unwrap();
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

        // The path should still be under the sessions directory
        assert!(path.starts_with(ws.join(".recursive").join("sessions")));
        // And should have .json extension
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("json"));
    }

    // -----------------------------------------------------------------------
    // JSONL session tests (Goal 107)
    // -----------------------------------------------------------------------

    #[test]
    fn session_writer_creates_meta_and_jsonl() {
        let tmp = tempfile::tempdir().unwrap();
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
        assert_eq!(meta.status, "active");

        writer.finish("completed").unwrap();

        let meta = SessionReader::load_meta(&session_dir).unwrap();
        assert_eq!(meta.message_count, 0);
        assert_eq!(meta.status, "completed");
    }

    #[test]
    fn session_writer_appends_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();

        let mut writer = SessionWriter::create(ws, "test", "gpt-4o", "openai").unwrap();

        let id1 = writer.append(&Message::user("hello")).unwrap();
        assert_eq!(id1, "msg_001");

        let id2 = writer.append(&Message::assistant("hi there")).unwrap();
        assert_eq!(id2, "msg_002");

        let session_dir = writer.session_dir().to_path_buf();
        writer.finish("completed").unwrap();

        // Load and verify
        let entries = SessionReader::load_transcript(&session_dir).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "msg_001");
        assert_eq!(entries[0].parent_id, None);
        assert_eq!(entries[0].role, "user");
        assert_eq!(entries[0].content, "hello");

        assert_eq!(entries[1].id, "msg_002");
        assert_eq!(entries[1].parent_id, Some("msg_001".to_string()));
        assert_eq!(entries[1].role, "assistant");
        assert_eq!(entries[1].content, "hi there");
    }

    #[test]
    fn session_reader_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();

        let mut writer = SessionWriter::create(ws, "round trip", "gpt-4o", "openai").unwrap();

        writer.append(&Message::system("You are a bot.")).unwrap();
        writer.append(&Message::user("do something")).unwrap();
        writer.append(&Message::assistant("I will do it.")).unwrap();

        let session_dir = writer.session_dir().to_path_buf();
        writer.finish("completed").unwrap();

        let entries = SessionReader::load_transcript(&session_dir).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].role, "system");
        assert_eq!(entries[1].role, "user");
        assert_eq!(entries[2].role, "assistant");
    }

    #[test]
    fn session_writer_finish_updates_meta() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();

        let mut writer = SessionWriter::create(ws, "meta test", "gpt-4o", "openai").unwrap();
        writer.append(&Message::user("msg1")).unwrap();
        writer.append(&Message::assistant("msg2")).unwrap();
        let session_dir = writer.session_dir().to_path_buf();
        writer.finish("completed").unwrap();

        let meta = SessionReader::load_meta(&session_dir).unwrap();
        assert_eq!(meta.message_count, 2);
        assert_eq!(meta.status, "completed");
    }

    #[test]
    fn list_sessions_finds_sessions() {
        let tmp = tempfile::tempdir().unwrap();
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
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();

        let mut writer = SessionWriter::create(ws, "crash test", "gpt-4o", "openai").unwrap();
        writer.append(&Message::user("good line")).unwrap();
        let session_dir = writer.session_dir().to_path_buf();
        writer.finish("crashed").unwrap();

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
        let dir = tempfile::tempdir().unwrap();
        let mut w = SessionWriter::create(dir.path(), "g", "m", "p").unwrap();
        // Sequence: system, user(turn 0), assistant, user(turn 1),
        // assistant, user(turn 2), assistant.
        w.append(&Message::system("sys".to_string())).unwrap();
        w.append(&Message::user("u0".to_string())).unwrap();
        w.append(&Message::assistant("a0".to_string())).unwrap();
        w.append(&Message::user("u1".to_string())).unwrap();
        w.append(&Message::assistant("a1".to_string())).unwrap();
        w.append(&Message::user("u2".to_string())).unwrap();
        w.append(&Message::assistant("a2".to_string())).unwrap();
        w.finish("done").unwrap();

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
        let dir = tempfile::tempdir().unwrap();
        let mut w = SessionWriter::create(dir.path(), "g", "m", "p").unwrap();
        w.append(&Message::system("sys".to_string())).unwrap();
        w.append(&Message::user("u0".to_string())).unwrap();
        w.append(&Message::assistant("a0".to_string())).unwrap();
        w.finish("done").unwrap();
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
        let dir = tempfile::tempdir().unwrap();
        // No session created → no transcript.jsonl. Should not panic.
        let stats = truncate_transcript_to_turn(dir.path(), 5).unwrap();
        assert_eq!(stats.kept, 0);
        assert_eq!(stats.dropped, 0);
    }
}
