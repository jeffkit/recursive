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
    let ts = chrono_lite_now();
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
        // The timestamp is fixed-format (YYYY-MM-DDTHH:MM:SSZ) and contains colons.
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
}
