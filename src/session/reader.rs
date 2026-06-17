//! Session reader for loading transcripts and metadata from JSONL session files.
//!
//! Split from `session.rs` during the Goal 221 module refactor.

use std::io::BufRead;
use std::path::{Path, PathBuf};

use super::orphan::OrphanToolCall;
use super::serialize::{entry_to_message, CompactBoundaryEntry, TranscriptEntry};
use super::SessionMeta;

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
    pub fn load_messages(session_dir: &Path) -> std::io::Result<Vec<crate::message::Message>> {
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
        if meta.schema_version > super::SUPPORTED_SESSION_SCHEMA_VERSION {
            let err = crate::error::Error::SchemaTooNew {
                session_id: meta.session_id.clone(),
                found: meta.schema_version,
                supported: super::SUPPORTED_SESSION_SCHEMA_VERSION,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolSpec;
    use crate::message::{Message, Role};
    use crate::session::{hash_tool_specs, SessionStatus, SessionWriter};

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
}
