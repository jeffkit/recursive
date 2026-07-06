//! Session writer for appending messages to JSONL session files and
//! the `SessionPersistenceSink` event-sink bridge.
//!
//! Split from `session.rs` during the Goal 221 module refactor.

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

use super::lifecycle::{read_last_message_uuid, SessionLock};
use super::serialize::{CompactBoundaryEntry, TranscriptEntry};
use super::{
    chrono_lite_now, hash_tool_specs, workspace_slug, SessionMeta, SessionStatus, UsageMeta,
};

use crate::event::{AgentEvent, EventSink};
use crate::llm::ToolSpec;
use crate::message::Message;

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
        let session_id = format!("{}-{}", super::filesystem_safe_timestamp(), slug);
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
            schema_version: super::SUPPORTED_SESSION_SCHEMA_VERSION,
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
        let meta = super::reader::SessionReader::load_meta(session_dir)?;

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
    use crate::session::SessionReader;

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

    // -- SessionPersistenceSink tests --------------------------------------

    #[test]
    fn session_writer_accessor_methods_return_correct_values() {
        // kills function-level replacements for session_id(), session_dir(),
        // message_count(), and last_uuid()
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let writer = SessionWriter::create(tmp.path(), "goal", "gpt-4o", "openai").unwrap();

        // session_id must be non-empty
        assert!(
            !writer.session_id().is_empty(),
            "session_id must be non-empty"
        );
        // session_dir must end with the session_id directory
        assert!(writer.session_dir().exists());
        assert!(writer.session_dir().ends_with(writer.session_id()));
        // initial message_count must be 0
        assert_eq!(writer.message_count(), 0, "initial message_count must be 0");
        // initial last_uuid must be None
        assert!(
            writer.last_uuid().is_none(),
            "last_uuid must be None initially"
        );
    }

    #[test]
    fn write_compact_boundary_appends_line_to_transcript() {
        // kills function-level replacement of write_compact_boundary
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let mut writer = SessionWriter::create(tmp.path(), "g", "gpt-4o", "openai").unwrap();
        let session_dir = writer.session_dir().to_path_buf();
        writer
            .write_compact_boundary(5, 10, Some("uuid-summary-1"))
            .unwrap();
        let transcript_path = session_dir.join("transcript.jsonl");
        let content = std::fs::read_to_string(&transcript_path).unwrap();
        assert!(
            content.contains("compact_boundary"),
            "transcript must contain 'compact_boundary' marker"
        );
        assert!(
            content.contains("uuid-summary-1"),
            "transcript must contain the summary UUID"
        );
    }

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
}
