//! Session reader for loading transcripts and metadata from JSONL session files.
//!
//! Split from `session.rs` during the Goal 221 module refactor.

use std::io::BufRead;
use std::path::{Path, PathBuf};

use super::orphan::OrphanToolCall;
use super::serialize::{entry_to_message, CompactBoundaryEntry, LoadedEntry, TranscriptEntry};
use super::SessionMeta;

/// Read every transcript entry, plus the index of the first row after the
/// last g157 `compact_boundary` declaration (`None` when there is none).
///
/// The declaration handling itself lives in
/// [`crate::compact::rebuild_declared_seed`]: this is only the JSONL parse,
/// shared by the display path ([`SessionReader::load_transcript`], which
/// cuts) and the model-seed path ([`SessionReader::load_messages`], which
/// hands both the rows and the index to the canonical rebuild).
fn read_transcript(session_dir: &Path) -> std::io::Result<(Vec<TranscriptEntry>, Option<usize>)> {
    let jsonl_path = session_dir.join("transcript.jsonl");
    let file = std::fs::File::open(&jsonl_path)?;
    let reader = std::io::BufReader::new(file);

    let mut entries: Vec<TranscriptEntry> = Vec::new();
    // Index just after the last compact_boundary we saw.
    let mut after_boundary = None;

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
                after_boundary = Some(entries.len());
                continue;
            }
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
    Ok((entries, after_boundary))
}

/// Reader for loading sessions from JSONL files.
pub struct SessionReader;

impl SessionReader {
    /// Load all transcript entries from a session directory.
    ///
    /// If the JSONL contains a `compact_boundary` system entry (g157), all
    /// entries **before** the last such boundary are discarded—they were
    /// already summarised and the summary is the first entry after the
    /// boundary. This makes resume `O(post-compaction size)`.
    ///
    /// The cut is all this method does. The first entry after the boundary
    /// is left as stored; marking it `is_compaction_summary` when a
    /// pre-goal-413 writer persisted no bit is
    /// [`crate::compact::rebuild_declared_seed`], the one shared rebuild —
    /// so the declaration rule cannot drift between readers.
    pub fn load_transcript(session_dir: &Path) -> std::io::Result<Vec<TranscriptEntry>> {
        let (mut entries, after_boundary) = read_transcript(session_dir)?;
        Ok(entries.split_off(after_boundary.unwrap_or(0)))
    }

    /// Load the **full** on-disk history without discarding messages
    /// folded away by cross-turn compaction.
    ///
    /// Unlike [`load_transcript`](Self::load_transcript) / [`load_messages`](Self::load_messages)
    /// (which drop everything before the last `compact_boundary` so the
    /// model seed matches the post-compaction context), this keeps every
    /// line and surfaces each compaction as a
    /// [`LoadedEntry::CompactBoundary`] marker. Use this for
    /// **user-facing** paths — TUI session resume, `session show`,
    /// `session export` — where the user expects to see the whole
    /// conversation, not just the post-compaction tail.
    ///
    /// The model / resume-seed paths keep using `load_messages`; do not
    /// feed `load_full_history` output into the runtime transcript.
    pub fn load_full_history(session_dir: &Path) -> std::io::Result<Vec<LoadedEntry>> {
        let jsonl_path = session_dir.join("transcript.jsonl");
        let file = std::fs::File::open(&jsonl_path)?;
        let reader = std::io::BufReader::new(file);

        let mut out: Vec<LoadedEntry> = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            // g157: detect compact_boundary system entries before trying to
            // parse as TranscriptEntry (they have a different shape).
            if let Ok(sys) = serde_json::from_str::<CompactBoundaryEntry>(&line) {
                if sys.entry_type == "system" && sys.subtype == "compact_boundary" {
                    out.push(LoadedEntry::CompactBoundary {
                        turn: sys.turn,
                        removed: sys.compacted_count.unwrap_or(0),
                    });
                    continue;
                }
            }
            match serde_json::from_str::<TranscriptEntry>(&line) {
                Ok(entry) => out.push(LoadedEntry::Message(Box::new(entry))),
                Err(e) => {
                    // Skip corrupt lines gracefully (mirrors load_transcript).
                    eprintln!(
                        "warning: skipping corrupt line in {}: {}",
                        jsonl_path.display(),
                        e
                    );
                    continue;
                }
            }
        }
        Ok(out)
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

    /// Collect the tool names referenced by any assistant `tool_calls` in
    /// the transcript (post-compaction tail, same window the resume seed
    /// and orphan scan see). Used by the resume path to classify tool
    /// drift as "all referenced tools still exist" vs "some vanished".
    pub fn load_referenced_tool_names(session_dir: &Path) -> std::io::Result<Vec<String>> {
        let entries = Self::load_transcript(session_dir)?;
        let mut names = std::collections::BTreeSet::new();
        for e in &entries {
            for tc in &e.tool_calls {
                names.insert(tc.name.clone());
            }
        }
        Ok(names.into_iter().collect())
    }

    /// Load the transcript and convert each `TranscriptEntry` to a
    /// runtime [`Message`]. Persistence-only fields (`id`,
    /// `parent_id`, `uuid`, `parent_uuid`, `timestamp`, `usage`)
    /// are dropped here. The result is what `run_resumed` expects
    /// as its `seed` argument.
    ///
    /// The stored rebuilt system prompt is **dropped** (the runtime
    /// assembles its own, and a seed carrying a second one would double
    /// it). A compaction summary is also a `Role::System` row but is real
    /// conversation state, so it stays — as the seed head.
    ///
    /// Issue #168: assistant messages with no text and no tool calls (written
    /// by pre-fix binaries when an interrupt hit during the tool_call phase)
    /// are filtered out here. They carry no information and — worse — poison
    /// the session: replayed to Anthropic they are an empty text block, which
    /// is a permanent 400. Only that exact shape is dropped, so a
    /// tool_call-carrying assistant and every `Role::Tool` result survive and
    /// invariant #8 keeps holding.
    /// Goal 413: the stored→seed framing (stored prompt dropped, orphan tool
    /// results dropped, compaction summary kept as the head) is
    /// [`crate::compact::rebuild_declared_seed`], the canonical rebuild — this
    /// method is a thin JSONL adapter over it, not a framing of its own.
    pub fn load_messages(session_dir: &Path) -> std::io::Result<Vec<crate::message::Message>> {
        let (entries, after_boundary) = read_transcript(session_dir)?;
        let messages: Vec<crate::message::Message> =
            entries.into_iter().map(entry_to_message).collect();
        Ok(
            crate::compact::rebuild_declared_seed(&messages, after_boundary)
                .map_or_else(Vec::new, |rebuilt| rebuilt.seed),
        )
    }

    /// Load a session as the **replacement transcript** of an already-built
    /// runtime.
    ///
    /// [`Self::load_messages`] returns a builder *seed*: the stored prompt is
    /// dropped because [`crate::runtime::AgentRuntimeBuilder::build`]
    /// reassembles its own. A caller that instead hands the rows to
    /// [`crate::runtime::AgentRuntime::set_transcript`] replaces the whole
    /// transcript — including the prompt — so the runtime's own leading
    /// system prompt is re-attached here. Without it the resumed run goes out
    /// promptless (goal 413 review fix); with no prompt to keep, that
    /// degradation is logged rather than silent.
    pub fn load_seed_for_existing_runtime(
        session_dir: &Path,
        runtime_transcript: &[crate::message::Message],
    ) -> std::io::Result<Vec<crate::message::Message>> {
        use crate::message::Role;
        let seed = Self::load_messages(session_dir)?;
        let prompt_len = runtime_transcript
            .iter()
            .take_while(|m| m.role == Role::System && !m.is_compaction_summary)
            .count();
        if prompt_len == 0 {
            tracing::warn!(
                session = %session_dir.display(),
                "resuming into an already-built runtime with no system prompt; \
                 the resumed transcript will run without one"
            );
            return Ok(seed);
        }
        let mut out = Vec::with_capacity(prompt_len + seed.len());
        out.extend_from_slice(&runtime_transcript[..prompt_len]);
        out.extend(seed);
        Ok(out)
    }

    /// Load the resume seed ([`Self::load_messages`]) with the results of
    /// orphan tool calls spliced in.
    ///
    /// A session whose process died mid-tool-call ends on an assistant
    /// message whose `tool_calls` have no matching `tool` result. That seed
    /// is not sendable to a provider — a `tool_use` without a `tool_result`
    /// is an HTTP 400 — so every unpaired call must be answered *before* the
    /// run starts: with [`ORPHAN_SKIPPED_RESULT`](super::orphan::ORPHAN_SKIPPED_RESULT)
    /// (`--orphans=skip`) or with the output of a re-execution
    /// (`--orphans=redo`).
    ///
    /// `answers` pairs a tool-call id with the content that stands in for
    /// its result. The answers are inserted, in the order given, right after
    /// the last `tool` message that follows the issuing assistant message
    /// (or directly after the assistant message when the crash left no
    /// result at all), so the seeded transcript stays one contiguous
    /// tool-call/tool-result chain (invariant #8).
    ///
    /// An empty `answers` is exactly [`Self::load_messages`].
    ///
    /// Issue #168 review: the splice index is computed by
    /// [`crate::session::splice_orphan_results`] **on the filtered message
    /// vector** — the same vector it splices into. An earlier revision of this
    /// fix filtered the messages but kept the insert index from the raw
    /// `TranscriptEntry` list, which panicked
    /// (`range end index 5 out of range for slice of length 4`) on a legacy
    /// transcript that carried an empty assistant row plus an orphan tool
    /// call, and would otherwise have placed the synthetic result after later
    /// turns (breaking invariant #8) even when the indices happened to fit.
    pub fn load_messages_with_orphan_results(
        session_dir: &Path,
        answers: &[(String, String)],
    ) -> std::io::Result<Vec<crate::message::Message>> {
        let (entries, after_boundary) = read_transcript(session_dir)?;
        let messages: Vec<_> = entries.into_iter().map(entry_to_message).collect();
        let messages = crate::compact::rebuild_declared_seed(&messages, after_boundary)
            .map_or_else(Vec::new, |rebuilt| rebuilt.seed);
        Ok(super::orphan::splice_orphan_results(messages, answers))
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
    /// the call returned). Tools missing from the current registry fall back
    /// to `External`; callers that need to distinguish "known read-only"
    /// from "not registered" can check `registry.get(name).is_some()`.
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
                    call: tc.clone(),
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

    /// Issue #134 W2: a `run_code` turn is an ordinary assistant/tool pair
    /// in the transcript (the PTC lifecycle events deliberately never
    /// persist), so the reader must rebuild the resume seed with the pairing
    /// intact — a regression nail against any future transcript/event change
    /// silently swallowing the run_code step (invariant #8).
    #[test]
    fn run_code_turns_survive_resume_without_swallowing() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let ws = tmp.path();

        let mut writer = SessionWriter::create(ws, "run_code turn", "gpt-4o", "openai").unwrap();
        writer
            .append(&Message::system("You are a bot."), None, None)
            .unwrap();
        writer
            .append(&Message::user("sum the doubles"), None, None)
            .unwrap();
        // The run_code step: one assistant tool_call, one protocol-shaped
        // result — exactly what the tool result looks like on disk.
        writer
            .append(
                &Message::assistant_with_tool_calls(
                    "Running the batch.",
                    vec![crate::message::ToolCall {
                        id: "call_ptc_1".into(),
                        name: "RunCode".into(),
                        arguments: serde_json::json!({"code": "return 1;"}),
                    }],
                ),
                None,
                None,
            )
            .unwrap();
        writer
            .append(
                &Message::tool_result(
                    "call_ptc_1",
                    "status=ok calls=3 duration_ms=12\nsandbox: runtime=/usr/bin/node",
                ),
                None,
                None,
            )
            .unwrap();
        writer
            .append(&Message::assistant("The total is 6."), None, None)
            .unwrap();
        // A later turn after the run_code step — nothing after it may be
        // swallowed either.
        writer.append(&Message::user("thanks"), None, None).unwrap();

        let session_dir = writer.session_dir().to_path_buf();
        writer.finish(SessionStatus::Completed).unwrap();

        let messages = SessionReader::load_messages(&session_dir).unwrap();
        // The stored system prompt is dropped (builder reassembles it);
        // every message of the run_code turn and everything after survives.
        let roles: Vec<Role> = messages.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            vec![
                Role::User,
                Role::Assistant,
                Role::Tool,
                Role::Assistant,
                Role::User,
            ],
            "the run_code turn and the turns after it must survive the seed rebuild"
        );
        assert_eq!(messages[1].tool_calls.len(), 1);
        assert_eq!(messages[1].tool_calls[0].name, "RunCode");
        assert_eq!(
            messages[2].tool_call_id.as_deref(),
            Some("call_ptc_1"),
            "the protocol result stays glued to its call"
        );
        assert!(messages[2].content.starts_with("status=ok"));
        assert_pairing(&messages);
    }

    /// Invariant #8 checker for the splice tests: every `Role::Tool` message
    /// must be preceded by an assistant whose `tool_calls` contains its id,
    /// and every tool call in that assistant must be answered.
    fn assert_pairing(messages: &[crate::message::Message]) {
        for (i, m) in messages.iter().enumerate() {
            if m.role != Role::Tool {
                continue;
            }
            let id = m
                .tool_call_id
                .as_deref()
                .unwrap_or_else(|| panic!("message [{i}] is Tool without tool_call_id"));
            let parent = messages[..i]
                .iter()
                .rev()
                .find(|p| p.role == Role::Assistant)
                .unwrap_or_else(|| panic!("message [{i}] Tool({id}) has no preceding assistant"));
            assert!(
                parent.tool_calls.iter().any(|tc| tc.id == id),
                "message [{i}] Tool({id}) is not paired with its assistant: {messages:?}",
            );
        }
        for (i, m) in messages.iter().enumerate() {
            if m.role != Role::Assistant {
                continue;
            }
            for tc in &m.tool_calls {
                assert!(
                    messages[i + 1..].iter().any(|r| {
                        r.role == Role::Tool && r.tool_call_id.as_deref() == Some(tc.id.as_str())
                    }),
                    "assistant [{i}] tool_call {} is left unanswered: {messages:?}",
                    tc.id,
                );
            }
        }
    }

    #[test]
    fn legacy_empty_assistant_is_filtered_on_load() {
        // Issue #168 (spec test `legacy_empty_assistant_is_filtered_on_load`):
        // sessions written before the fix already carry
        // `{role: "assistant", content: ""}` (no tool_calls) rows on disk.
        // They must be filtered on load, otherwise the poisoned session can
        // never be reopened and every later request 400s. Only that exact
        // shape is dropped — tool results and tool_call-carrying assistants
        // must survive (invariant #8).
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let ws = tmp.path();
        let mut writer = SessionWriter::create(ws, "g168 legacy", "model", "anthropic").unwrap();
        let session_dir = writer.session_dir().to_path_buf();

        writer
            .append(&Message::user("hello".to_string()), None, None)
            .unwrap();
        writer
            .append(&Message::assistant("".to_string()), None, None)
            .unwrap();
        writer
            .append(
                &Message::assistant_with_tool_calls(
                    "".to_string(),
                    vec![crate::llm::ToolCall {
                        id: "call_1".to_string(),
                        name: "Read".to_string(),
                        arguments: serde_json::json!({"path": "x"}),
                    }],
                ),
                None,
                None,
            )
            .unwrap();
        writer
            .append(
                &Message::tool_result("call_1".to_string(), "file body".to_string()),
                None,
                None,
            )
            .unwrap();
        writer.finish(SessionStatus::Completed).unwrap();
        drop(writer);

        // The bad row really is on disk (the test would be vacuous otherwise).
        let entries = SessionReader::load_transcript(&session_dir).unwrap();
        assert_eq!(entries.len(), 4, "the legacy empty row must be persisted");
        assert_eq!(entries[1].role, "assistant");
        assert!(entries[1].content.is_empty());
        assert!(entries[1].tool_calls.is_empty());

        let msgs = SessionReader::load_messages(&session_dir).unwrap();
        assert_eq!(
            msgs.len(),
            3,
            "the legacy empty assistant must be filtered on load: {msgs:?}",
        );
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[1].role, Role::Assistant);
        assert_eq!(msgs[1].tool_calls.len(), 1, "tool_calls must survive");
        assert_eq!(msgs[2].role, Role::Tool, "tool result must survive (#8)");

        // The resume seed with orphan answers goes through the same filter.
        // NOTE: this MUST pass a non-empty `answers` slice to reach the splice
        // branch — an empty one takes the early return above and would leave
        // the insertion-index arithmetic untested (that is exactly how the
        // index-on-unfiltered-entries regression got through).
        let reopened = SessionReader::load_messages_with_orphan_results(&session_dir, &[]).unwrap();
        assert_eq!(reopened.len(), 3);

        let spliced = SessionReader::load_messages_with_orphan_results(
            &session_dir,
            &[("call_1".to_string(), "repaired".to_string())],
        )
        .unwrap();
        assert!(
            spliced
                .iter()
                .any(|m| m.role == Role::Tool && m.tool_call_id.as_deref() == Some("call_1")),
            "the synthetic tool result must be spliced in: {spliced:?}",
        );
        assert_pairing(&spliced);
    }

    /// Issue #168 review regression — the reviewer's falsifier, verbatim shape.
    ///
    /// `load_messages_with_orphan_results` computed the splice index on the
    /// **unfiltered** `entries` but spliced into the **filtered** `messages`,
    /// so a legacy transcript carrying an empty assistant row plus a trailing
    /// orphan tool call panicked with
    /// `range end index 5 out of range for slice of length 4`. That is the
    /// real `recursive resume --orphans=skip|redo` path
    /// (`crates/recursive-cli/src/cli/resume.rs`), and the empty `answers`
    /// test above never reached it.
    ///
    /// Also pins *position*: the synthetic result must land immediately after
    /// the last real tool result (or immediately after the issuing assistant
    /// when there is none), never after later user/assistant turns — otherwise
    /// invariant #8 is broken even without a panic.
    #[test]
    fn legacy_empty_assistant_plus_orphan_tool_call_splices_at_the_filtered_index() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let ws = tmp.path();
        let mut writer = SessionWriter::create(ws, "g168 splice", "model", "anthropic").unwrap();
        let session_dir = writer.session_dir().to_path_buf();

        writer
            .append(&Message::user("hello".to_string()), None, None)
            .unwrap();
        writer
            .append(&Message::assistant("".to_string()), None, None)
            .unwrap();
        writer
            .append(
                &Message::assistant_with_tool_calls(
                    "".to_string(),
                    vec![crate::llm::ToolCall {
                        id: "call_1".to_string(),
                        name: "Read".to_string(),
                        arguments: serde_json::json!({"path": "x"}),
                    }],
                ),
                None,
                None,
            )
            .unwrap();
        writer.finish(SessionStatus::Completed).unwrap();
        drop(writer);

        // JSONL has 3 rows (user, empty assistant, assistant+tool_call); the
        // filtered message seed has 2. The insert index must be computed on
        // the latter.
        assert_eq!(
            SessionReader::load_transcript(&session_dir).unwrap().len(),
            3
        );
        let messages = SessionReader::load_messages(&session_dir).unwrap();
        assert_eq!(messages.len(), 2, "empty assistant filtered: {messages:?}");

        let spliced = SessionReader::load_messages_with_orphan_results(
            &session_dir,
            &[(
                "call_1".to_string(),
                "[interrupted: no result recorded]".to_string(),
            )],
        )
        .expect("must not panic and must not index past the filtered vector");

        assert_eq!(
            spliced.len(),
            3,
            "user + assistant(tool_call) + synthetic tool result: {spliced:?}",
        );
        assert_eq!(spliced[0].role, Role::User);
        assert_eq!(spliced[1].role, Role::Assistant);
        assert_eq!(spliced[1].tool_calls.len(), 1);
        assert_eq!(
            spliced[2].role,
            Role::Tool,
            "the synthetic result must sit directly after its issuing assistant, \
             not after a later turn: {spliced:?}",
        );
        assert_eq!(spliced[2].tool_call_id.as_deref(), Some("call_1"));
        assert_pairing(&spliced);
    }

    /// Issue #168 review — the *position* half of the splice fix, where the
    /// buggy index happens to fit and therefore does **not** panic.
    ///
    /// Transcript: `user, <empty assistant>, assistant(tool_call), tool,
    /// user`. Filtering drops one row, so the raw-entry index (4) is still
    /// within the filtered vector (len 4) and `splice(4..4)` silently appends
    /// the synthetic result *after the trailing user turn* — invariant #8
    /// broken with no panic to show for it. The answer must land at index 3,
    /// immediately after the existing result batch.
    #[test]
    fn legacy_empty_assistant_plus_orphan_answer_lands_before_later_turns() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let ws = tmp.path();
        let mut writer = SessionWriter::create(ws, "g168 position", "model", "anthropic").unwrap();
        let session_dir = writer.session_dir().to_path_buf();

        writer
            .append(&Message::user("hello".to_string()), None, None)
            .unwrap();
        writer
            .append(&Message::assistant("".to_string()), None, None)
            .unwrap();
        writer
            .append(
                &Message::assistant_with_tool_calls(
                    "".to_string(),
                    vec![crate::llm::ToolCall {
                        id: "call_1".to_string(),
                        name: "Read".to_string(),
                        arguments: serde_json::json!({"path": "x"}),
                    }],
                ),
                None,
                None,
            )
            .unwrap();
        writer
            .append(
                &Message::tool_result("call_1".to_string(), "old output".to_string()),
                None,
                None,
            )
            .unwrap();
        writer
            .append(&Message::user("next question".to_string()), None, None)
            .unwrap();
        writer.finish(SessionStatus::Completed).unwrap();
        drop(writer);

        assert_eq!(
            SessionReader::load_transcript(&session_dir).unwrap().len(),
            5,
            "5 raw rows, one of which is filtered",
        );
        let messages = SessionReader::load_messages(&session_dir).unwrap();
        assert_eq!(messages.len(), 4, "filtered seed: {messages:?}");

        let spliced = SessionReader::load_messages_with_orphan_results(
            &session_dir,
            &[("call_1".to_string(), "repaired".to_string())],
        )
        .expect("must not panic");

        assert_eq!(
            spliced.len(),
            5,
            "one synthetic result is added: {spliced:?}",
        );
        assert_eq!(spliced[0].role, Role::User);
        assert_eq!(spliced[1].role, Role::Assistant);
        assert_eq!(spliced[2].role, Role::Tool);
        assert_eq!(spliced[2].content, "old output");
        assert_eq!(
            spliced[3].role,
            Role::Tool,
            "the answer must join the result batch before the later user turn: {spliced:?}",
        );
        assert_eq!(spliced[3].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(spliced[3].content, "repaired");
        assert_eq!(
            spliced[4].role,
            Role::User,
            "the later user turn must stay last: {spliced:?}",
        );
        assert_pairing(&spliced);
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

    #[test]
    fn list_sessions_sorted_by_updated_at_returns_one_session_with_meta() {
        // kills function-level replacement of list_sessions_sorted_by_updated_at
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let ws = tmp.path();

        // Create one session
        let w1 = SessionWriter::create(ws, "sort-test-goal", "gpt-4o", "openai").unwrap();
        drop(w1);

        let sorted = SessionReader::list_sessions_sorted_by_updated_at(ws).unwrap();
        assert_eq!(sorted.len(), 1, "must find 1 session");
        // Second tuple element is SessionMeta — its session_id must be non-empty
        assert!(!sorted[0].1.session_id.is_empty());
    }

    #[test]
    fn list_all_sessions_returns_empty_for_no_sessions() {
        // kills function-level replacement of list_all_sessions
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let ws = tmp.path();
        let sessions = SessionReader::list_all_sessions(ws).unwrap();
        assert!(sessions.is_empty(), "must be empty when no sessions exist");
    }

    #[test]
    fn scan_orphan_tool_calls_returns_empty_for_no_assistant_with_tool_calls() {
        // kills `let Some(...) = last_assistant else { return Ok(Vec::new()) }` guard removal
        use crate::message::Message;
        use crate::session::{SessionStatus, SessionWriter};

        let tmp = crate::test_util::IsolatedWorkspace::new();
        let mut w = SessionWriter::create(tmp.path(), "g", "m", "p").unwrap();
        // Only user+assistant (no tool_calls) messages — last_assistant returns None
        w.append(&Message::user("hi".to_string()), None, None)
            .unwrap();
        w.append(&Message::assistant("hello".to_string()), None, None)
            .unwrap();
        w.finish(SessionStatus::Completed).unwrap();

        let reg = crate::tools::ToolRegistry::local();
        let orphans = SessionReader::scan_orphan_tool_calls(w.session_dir(), &reg).unwrap();
        assert!(
            orphans.is_empty(),
            "no assistant tool_calls → no orphans; got: {orphans:?}"
        );
    }

    #[test]
    fn scan_orphan_tool_calls_returns_empty_when_all_answered() {
        // kills `if !answered.contains(&tc.id)` guard removal mutation
        use crate::message::Message;
        use crate::session::{SessionStatus, SessionWriter};

        let tmp = crate::test_util::IsolatedWorkspace::new();
        let mut w = SessionWriter::create(tmp.path(), "g", "m", "p").unwrap();

        // Build: user → assistant with tool_call → tool result
        let mut asst = Message::assistant("calling tool".to_string());
        asst.tool_calls = vec![crate::llm::ToolCall {
            id: "tc-001".to_string(),
            name: "Read".to_string(),
            arguments: serde_json::json!({"path": "foo.txt"}),
        }];

        let tool_result = Message {
            role: crate::message::Role::Tool,
            content: "file contents".to_string(),
            tool_calls: vec![],
            tool_call_id: Some("tc-001".to_string()),
            reasoning_content: None,
            is_compaction_summary: false,
        };

        w.append(&Message::user("do something".to_string()), None, None)
            .unwrap();
        w.append(&asst, None, None).unwrap();
        w.append(&tool_result, None, None).unwrap();
        w.finish(SessionStatus::Completed).unwrap();

        let reg = crate::tools::ToolRegistry::local();
        let orphans = SessionReader::scan_orphan_tool_calls(w.session_dir(), &reg).unwrap();
        assert!(
            orphans.is_empty(),
            "all tool calls answered → no orphans; got: {orphans:?}"
        );
    }

    #[test]
    fn scan_orphan_tool_calls_detects_unanswered_call() {
        // kills `if !answered.contains(&tc.id)` guard-flip mutation
        use crate::message::Message;
        use crate::session::{SessionStatus, SessionWriter};

        let tmp = crate::test_util::IsolatedWorkspace::new();
        let mut w = SessionWriter::create(tmp.path(), "g", "m", "p").unwrap();

        // Build: user → assistant with tool_call — no tool result follows
        let mut asst = Message::assistant("calling tool".to_string());
        asst.tool_calls = vec![crate::llm::ToolCall {
            id: "tc-orphan".to_string(),
            name: "Read".to_string(),
            arguments: serde_json::json!({"path": "missing.txt"}),
        }];

        w.append(&Message::user("do something".to_string()), None, None)
            .unwrap();
        w.append(&asst, None, None).unwrap();
        // No tool result appended!
        w.finish(SessionStatus::Completed).unwrap();

        let reg = crate::tools::ToolRegistry::local();
        let orphans = SessionReader::scan_orphan_tool_calls(w.session_dir(), &reg).unwrap();
        assert_eq!(
            orphans.len(),
            1,
            "one unanswered tool call must be reported as orphan"
        );
        assert_eq!(orphans[0].tool_call_id, "tc-orphan");
        assert_eq!(orphans[0].tool_name, "Read");
    }

    #[test]
    fn load_referenced_tool_names_collects_every_assistant_call() {
        use crate::message::Message;
        use crate::session::{SessionStatus, SessionWriter};

        let tmp = crate::test_util::IsolatedWorkspace::new();
        let mut w = SessionWriter::create(tmp.path(), "g", "m", "p").unwrap();

        let mut a1 = Message::assistant("one".to_string());
        a1.tool_calls = vec![crate::llm::ToolCall {
            id: "c1".to_string(),
            name: "Bash".to_string(),
            arguments: serde_json::json!({}),
        }];
        let mut a2 = Message::assistant("two".to_string());
        a2.tool_calls = vec![
            crate::llm::ToolCall {
                id: "c2".to_string(),
                name: "Bash".to_string(),
                arguments: serde_json::json!({}),
            },
            crate::llm::ToolCall {
                id: "c3".to_string(),
                name: "Read".to_string(),
                arguments: serde_json::json!({}),
            },
        ];

        w.append(&Message::user("go".to_string()), None, None)
            .unwrap();
        w.append(&a1, None, None).unwrap();
        w.append(&a2, None, None).unwrap();
        w.finish(SessionStatus::Completed).unwrap();

        let names = SessionReader::load_referenced_tool_names(w.session_dir()).unwrap();
        assert_eq!(
            names,
            vec!["Bash".to_string(), "Read".to_string()],
            "must be deduplicated and sorted"
        );
    }

    #[test]
    fn load_referenced_tool_names_empty_without_tool_calls() {
        use crate::message::Message;
        use crate::session::{SessionStatus, SessionWriter};

        let tmp = crate::test_util::IsolatedWorkspace::new();
        let mut w = SessionWriter::create(tmp.path(), "g", "m", "p").unwrap();
        w.append(&Message::user("hi".to_string()), None, None)
            .unwrap();
        w.append(&Message::assistant("hello".to_string()), None, None)
            .unwrap();
        w.finish(SessionStatus::Completed).unwrap();

        let names = SessionReader::load_referenced_tool_names(w.session_dir()).unwrap();
        assert!(
            names.is_empty(),
            "no tool_calls → empty list, got {names:?}"
        );
    }

    // Goal: TUI resume / session show should display the full conversation,
    // not just the post-compaction tail. `load_full_history` keeps every
    // message and surfaces each compact_boundary as a `CompactBoundary`
    // marker, while `load_messages` (model seed) keeps stripping them.
    #[test]
    fn load_full_history_includes_pre_compact_messages_and_boundaries() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let ws = tmp.path();
        let mut w = SessionWriter::create(ws, "full history", "model", "openai").unwrap();
        let session_dir = w.session_dir().to_path_buf();

        // Pre-compact messages (these get folded at runtime, but stay on disk).
        w.append(&Message::user("turn 1".to_string()), None, None)
            .unwrap();
        w.append(&Message::assistant("reply 1".to_string()), None, None)
            .unwrap();
        w.append(&Message::user("turn 2".to_string()), None, None)
            .unwrap();
        w.append(&Message::assistant("reply 2".to_string()), None, None)
            .unwrap();
        // Boundary: folds the 4 messages above into a summary.
        w.write_compact_boundary(3, 4, None).unwrap();
        // Post-compact summary + a fresh exchange.
        w.append(
            &Message::system("summary of turn 1-2".to_string()),
            None,
            None,
        )
        .unwrap();
        w.append(&Message::user("turn 3".to_string()), None, None)
            .unwrap();
        w.append(&Message::assistant("reply 3".to_string()), None, None)
            .unwrap();
        w.finish(SessionStatus::Completed).unwrap();

        // load_full_history keeps everything: 7 messages + 1 boundary marker.
        let loaded = SessionReader::load_full_history(&session_dir).unwrap();
        assert_eq!(loaded.len(), 8, "all messages + the boundary marker");

        // The boundary sits between the folded messages and the summary.
        let boundary_idx = loaded
            .iter()
            .position(
                |e| matches!(e, LoadedEntry::CompactBoundary { removed, .. } if *removed == 4),
            )
            .expect("a CompactBoundary marker with removed=4");
        assert_eq!(boundary_idx, 4, "boundary follows the 4 folded messages");

        // Messages before and after the boundary are all present.
        let pre = &loaded[..boundary_idx];
        assert_eq!(pre.len(), 4);
        assert!(pre.iter().all(|e| matches!(e, LoadedEntry::Message(_))));

        let post = &loaded[boundary_idx + 1..];
        assert_eq!(post.len(), 3);
        assert!(post.iter().all(|e| matches!(e, LoadedEntry::Message(_))));

        // The summary is the first message after the boundary.
        if let LoadedEntry::Message(m) = &post[0] {
            assert_eq!(m.role, "system");
            assert_eq!(m.content, "summary of turn 1-2");
        } else {
            panic!("expected a Message after the boundary");
        }

        // Model seed path: the `compact_boundary`-based skip in
        // `load_transcript` still drops the 4 folded rows, and the row it
        // marks as that boundary's summary leads the seed. The summary here
        // carries no persisted marker bit (raw `SessionWriter::append` stores
        // what it is given), so the canonical rebuild backfills it from the
        // boundary line (goal 413) — the seed is the post-compaction
        // conversation state, minus the stored prompt the runtime rebuilds.
        let seed = SessionReader::load_messages(&session_dir).unwrap();
        assert_eq!(
            seed.len(),
            3,
            "load_messages keeps the summary and the post-compaction tail"
        );
        assert!(
            seed[0].is_compaction_summary,
            "the boundary summary row must stay marked as conversation state"
        );
        assert_eq!(seed[0].content, "summary of turn 1-2");
    }

    /// Goal 413 review fix: `load_messages` is a *builder seed* (the stored
    /// prompt is dropped for `RuntimeBuilder::build`), so replacing an
    /// already-built runtime's transcript with it wholesale would lose that
    /// runtime's own prompt. `load_seed_for_existing_runtime` re-attaches it.
    #[test]
    fn load_seed_for_existing_runtime_keeps_the_runtime_prompt() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let ws = tmp.path();
        let mut w = SessionWriter::create(ws, "resume", "model", "openai").unwrap();
        let session_dir = w.session_dir().to_path_buf();
        w.append(&Message::user("hello".to_string()), None, None)
            .unwrap();
        w.append(&Message::assistant("hi".to_string()), None, None)
            .unwrap();
        w.finish(SessionStatus::Completed).unwrap();

        let runtime_transcript = vec![Message::system("RUNTIME PROMPT".to_string())];
        let seed = SessionReader::load_seed_for_existing_runtime(&session_dir, &runtime_transcript)
            .unwrap();
        let contents: Vec<&str> = seed.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(
            contents,
            vec!["RUNTIME PROMPT", "hello", "hi"],
            "a replacement transcript must keep the runtime's own prompt"
        );
    }

    #[test]
    fn load_seed_for_existing_runtime_keeps_a_compaction_summary_after_the_prompt() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let ws = tmp.path();
        let mut w = SessionWriter::create(ws, "resume-compacted", "model", "openai").unwrap();
        let session_dir = w.session_dir().to_path_buf();
        w.append(&Message::user("old".to_string()), None, None)
            .unwrap();
        w.write_compact_boundary(1, 1, None).unwrap();
        w.append(
            &Message::system("[compacted: old]".to_string()).with_compaction_summary(),
            None,
            None,
        )
        .unwrap();
        w.append(&Message::user("new".to_string()), None, None)
            .unwrap();
        w.finish(SessionStatus::Completed).unwrap();

        let runtime_transcript = vec![Message::system("RUNTIME PROMPT".to_string())];
        let seed = SessionReader::load_seed_for_existing_runtime(&session_dir, &runtime_transcript)
            .unwrap();
        let contents: Vec<&str> = seed.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(
            contents,
            vec!["RUNTIME PROMPT", "[compacted: old]", "new"],
            "prompt first, then the canonical rebuilt post-compaction state"
        );
        assert!(seed[1].is_compaction_summary);
    }

    #[test]
    fn load_full_history_no_boundary_returns_all_messages() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let ws = tmp.path();
        let mut w = SessionWriter::create(ws, "no compact", "model", "openai").unwrap();
        let session_dir = w.session_dir().to_path_buf();
        w.append(&Message::user("hi".to_string()), None, None)
            .unwrap();
        w.append(&Message::assistant("hello".to_string()), None, None)
            .unwrap();
        w.finish(SessionStatus::Completed).unwrap();

        let loaded = SessionReader::load_full_history(&session_dir).unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(
            loaded.iter().all(|e| matches!(e, LoadedEntry::Message(_))),
            "no CompactBoundary markers when the session never compacted"
        );
    }

    // ── load_messages_with_orphan_results ────────────────────────────────────

    /// Build `user → assistant(tool_calls) → [tool results]`, i.e. the shape a
    /// session has after a crash mid-tool-call.
    fn crash_session(answers_written: &[&str]) -> (crate::test_util::IsolatedWorkspace, PathBuf) {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let mut w = SessionWriter::create(tmp.path(), "g", "m", "p").unwrap();
        let mut asst = Message::assistant("calling".to_string());
        asst.tool_calls = vec![
            crate::llm::ToolCall {
                id: "tc-1".to_string(),
                name: "Read".to_string(),
                arguments: serde_json::json!({"path": "a.txt"}),
            },
            crate::llm::ToolCall {
                id: "tc-2".to_string(),
                name: "Bash".to_string(),
                arguments: serde_json::json!({"command": "ls"}),
            },
        ];
        w.append(&Message::user("go".to_string()), None, None)
            .unwrap();
        w.append(&asst, None, None).unwrap();
        for id in answers_written {
            w.append(&Message::tool_result(*id, "recorded"), None, None)
                .unwrap();
        }
        let dir = w.session_dir().to_path_buf();
        drop(w);
        (tmp, dir)
    }

    #[test]
    fn load_messages_with_orphan_results_empty_answers_is_plain_load() {
        let (_tmp, dir) = crash_session(&[]);
        let plain = SessionReader::load_messages(&dir).unwrap();
        let spliced = SessionReader::load_messages_with_orphan_results(&dir, &[]).unwrap();
        assert_eq!(plain, spliced);
    }

    #[test]
    fn load_messages_with_orphan_results_answers_every_unpaired_call() {
        let (_tmp, dir) = crash_session(&[]);
        // Crash left no result at all → answers go straight after the assistant.
        let spliced = SessionReader::load_messages_with_orphan_results(
            &dir,
            &[
                ("tc-1".to_string(), "read output".to_string()),
                ("tc-2".to_string(), "shell output".to_string()),
            ],
        )
        .unwrap();

        assert_eq!(spliced.len(), 4, "user + assistant + 2 synthetic results");
        assert_eq!(spliced[2].role, crate::message::Role::Tool);
        assert_eq!(spliced[2].tool_call_id.as_deref(), Some("tc-1"));
        assert_eq!(spliced[2].content, "read output");
        assert_eq!(spliced[3].tool_call_id.as_deref(), Some("tc-2"));
        assert_eq!(spliced[3].content, "shell output");
    }

    #[test]
    fn load_messages_with_orphan_results_follows_a_partial_result_batch() {
        // tc-1's result made it to disk before the crash; the answer for
        // tc-2 must land *after* it, not between the assistant and it —
        // otherwise the seeded chain is `assistant, tool(2), tool(1)`.
        let (_tmp, dir) = crash_session(&["tc-1"]);
        let spliced = SessionReader::load_messages_with_orphan_results(
            &dir,
            &[("tc-2".to_string(), "shell output".to_string())],
        )
        .unwrap();

        assert_eq!(spliced.len(), 4);
        assert_eq!(spliced[2].tool_call_id.as_deref(), Some("tc-1"));
        assert_eq!(spliced[2].content, "recorded");
        assert_eq!(spliced[3].tool_call_id.as_deref(), Some("tc-2"));
        assert_eq!(spliced[3].content, "shell output");
    }

    #[test]
    fn load_messages_with_orphan_results_appends_without_an_assistant_call() {
        // No assistant tool_calls anywhere → nothing to pair against, so the
        // answers trail the transcript (and never get dropped).
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let mut w = SessionWriter::create(tmp.path(), "g", "m", "p").unwrap();
        w.append(&Message::user("hi".to_string()), None, None)
            .unwrap();
        w.append(&Message::assistant("hello".to_string()), None, None)
            .unwrap();
        let dir = w.session_dir().to_path_buf();
        drop(w);

        let spliced = SessionReader::load_messages_with_orphan_results(
            &dir,
            &[("tc-1".to_string(), "late".to_string())],
        )
        .unwrap();
        assert_eq!(spliced.len(), 3);
        assert_eq!(spliced[2].tool_call_id.as_deref(), Some("tc-1"));
    }
}
