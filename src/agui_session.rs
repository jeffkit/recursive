//! AG-UI threads ARE sessions (issue #57).
//!
//! Before #57 the `/agui` endpoint persisted thread state with raw
//! `std::fs::write` into a flat `<sessions>/agui-<sanitized-thread>/`
//! directory: no `.meta.json`, no cost, no uuid chain — invisible to
//! every consumer of `SessionReader::list_sessions` (`recursive
//! sessions list`, `episodic_recall`, `episodic_recall_summary`, the
//! resume picker), i.e. cross-session memory silently failed for
//! AG-UI users.
//!
//! This module maps an AG-UI thread onto the documented session
//! layout instead of keeping a second, weaker format:
//!
//! - **Directory**: `<sessions>/<workspace-slug>/agui-<blake3-16>/` —
//!   the session id is a BLAKE3 prefix of the raw thread id, so
//!   distinct thread ids cannot collide the way the old `-`-mangling
//!   sanitizer collapsed `"tenantA/user1/conv1"` and
//!   `"tenantA/user1-conv1"` onto the same directory.
//! - **Format**: transcript lines are written by
//!   [`crate::session::SessionWriter`] (uuid chain, `msg_NNN` ids,
//!   timestamps, `.meta.json`, per-session `SessionLock`), so the
//!   thread is readable by every native session consumer with zero
//!   changes to those consumers.
//! - **Cost**: run usage lands in `.meta.json` (`cost` totals via the
//!   writer, `cost_usd` + `cost.json` via [`crate::cost::CostTracker`]).
//! - **Legacy threads**: pre-#57 flat `agui-<sanitized>/` directories
//!   are migrated (copied + rewritten into `TranscriptEntry` lines +
//!   metadata synthesized) the first time the thread is resolved.
//!
//! Still local-disk backed: routing through `StorageBackend` so
//! Redis/S3 deployments work is the #56 layering follow-up.

use std::path::{Path, PathBuf};

use crate::llm::TokenUsage;
use crate::message::{Message, Role};
use crate::session::{SessionMeta, SessionReader, SessionStatus, SessionWriter, UsageMeta};

/// Files a pre-#57 flat AG-UI thread directory can carry. All of them
/// move into the native session layout on migration.
const LEGACY_THREAD_FILES: [&str; 3] =
    ["transcript.jsonl", "checkpoints.jsonl", ".interrupts.json"];

/// Deterministic session id for an AG-UI thread.
///
/// `agui-` + the first 16 hex chars of the BLAKE3 hash of the raw
/// thread id. Pure function of the input, so resume lookups derive
/// the same directory without a registry; the hash makes distinct
/// `(tenant, user, conversation)` thread ids map to distinct
/// directories (the old sanitiser mapped them onto one). The value
/// satisfies the checkpoint module's `validate_session_id` rules
/// (alnum/-/_/. only, no leading dot, no `..`), so it doubles as the
/// shadow-git checkpoint chain id.
pub fn thread_session_key(thread_id: &str) -> String {
    let hash = blake3::hash(thread_id.as_bytes());
    format!("agui-{}", &hash.to_hex()[..16])
}

/// The pre-#57 directory name for a thread (kept only for legacy
/// migration lookups). All disallowed chars became `-`, which is why
/// distinct thread ids could collide.
pub fn legacy_thread_dir_name(thread_id: &str) -> String {
    format!("agui-{}", legacy_sanitize_thread_id(thread_id))
}

/// The old lossy thread-id sanitiser. Path-traversal safe (that part
/// was always correct) but collision-prone: every character outside
/// `[A-Za-z0-9._-]` becomes `-`.
pub fn legacy_sanitize_thread_id(thread: &str) -> String {
    let mut out: String = thread
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    // Drop a leading dot so we don't produce a hidden dir.
    while out.starts_with('.') {
        out.replace_range(..1, "-");
    }
    // Collapse `..` so we don't produce ref-traversal sequences.
    while out.contains("..") {
        out = out.replace("..", "-.");
    }
    if out.is_empty() {
        out.push_str("default");
    }
    out
}

/// Native session directory for a thread:
/// `<sessions>/<workspace-slug>/<thread_session_key>/`.
pub fn session_dir(workspace: &Path, thread_id: &str) -> Option<PathBuf> {
    let base = crate::paths::user_sessions_dir(workspace).ok()?;
    Some(
        base.join(crate::session::workspace_slug(workspace))
            .join(thread_session_key(thread_id)),
    )
}

/// The pre-#57 flat directory for a thread:
/// `<sessions>/agui-<sanitized-thread>/`.
pub fn legacy_session_dir(workspace: &Path, thread_id: &str) -> Option<PathBuf> {
    let base = crate::paths::user_sessions_dir(workspace).ok()?;
    Some(base.join(legacy_thread_dir_name(thread_id)))
}

/// Resolve the session directory for a thread, migrating a pre-#57
/// flat thread into the native layout first when one exists.
///
/// The directory is not created on disk — callers that write create
/// it (`persist_run`, checkpoint wiring); readers just get `None`
/// -free paths and treat a missing directory as "no state".
pub fn resolve_session_dir(workspace: &Path, thread_id: &str) -> Option<PathBuf> {
    let dir = session_dir(workspace, thread_id)?;
    if !dir.join("transcript.jsonl").is_file() {
        migrate_legacy_dir(workspace, thread_id, &dir);
    }
    Some(dir)
}

/// One AG-UI run's persist request.
pub struct RunRecord<'a> {
    pub workspace: &'a Path,
    pub thread_id: &'a str,
    /// Messages this run added — NOT the seeded history. Appending the
    /// seed back would duplicate the transcript on resume runs.
    pub messages: &'a [Message],
    /// Run goal, used when the session's `.meta.json` is first created.
    pub goal: &'a str,
    pub model: &'a str,
    pub provider: &'a str,
    pub preset: Option<&'a str>,
    pub status: SessionStatus,
    /// Why the run stopped when that was a failure (issue #111), taken
    /// from `SessionStatus::for_finish`. `None` for a run that
    /// completed, was interrupted, or has no reason to record.
    pub finish_reason: Option<String>,
    /// Failure text when the run aborted with an error rather than a
    /// finish reason (issue #111).
    pub error: Option<String>,
    /// Total token usage of the run (`None` when the run failed before
    /// any completed LLM call — nothing to bill).
    pub usage: Option<TokenUsage>,
    pub llm_latency_ms: u64,
}

/// Append one AG-UI run to the thread's native session.
///
/// Opens (or creates) the thread's session directory and appends
/// `messages` through [`SessionWriter`] — uuid chain, `.meta.json`
/// bookkeeping and the per-session [`crate::session::SessionLock`]
/// all come from the writer, so a thread is indistinguishable from a
/// CLI session on disk. Cost: usage totals accumulate into
/// `.meta.json`'s `cost` across runs, and a `cost.json` +
/// `cost_usd`/`total_tokens`/... block (via
/// [`crate::cost::CostTracker`]) is written per billed run.
///
/// Returns the session directory on success.
pub fn persist_run(record: RunRecord<'_>) -> std::io::Result<PathBuf> {
    let dir = resolve_session_dir(record.workspace, record.thread_id)
        .ok_or_else(|| std::io::Error::other("cannot resolve AG-UI session directory"))?;
    std::fs::create_dir_all(&dir)?;

    let mut writer = SessionWriter::open_or_create(
        &dir,
        record.goal,
        record.model,
        record.provider,
        record.preset,
    )?;
    // The server may have been started with a different model than the
    // session was created with — `.meta.json` should name the model the
    // latest activity actually used (pricing reads it).
    writer.update_identity(record.model, record.provider, record.preset);
    for msg in record.messages {
        writer.append(msg, None, None)?;
    }
    if let Some(usage) = record.usage {
        writer.add_usage(&UsageMeta::from_token_usage(&usage));
    }
    writer.finish_with_details(record.status, record.finish_reason, record.error)?;

    if let Some(usage) = record.usage {
        let mut tracker = crate::cost::CostTracker::new(dir.clone(), record.model, record.provider);
        tracker.record_usage(usage, record.llm_latency_ms);
        tracker.finish()?;
    }
    Ok(dir)
}

/// Make a resume's client-tool results durable on disk.
///
/// The resume path splices the payload into the *seeded in-memory*
/// transcript, but runs append to the session — without this rewrite the
/// on-disk tool result would keep the deny-marker text and a later
/// resume of the same thread would re-seed the wrong history.
///
/// `replaced` entries rewrite the existing tool result whose
/// `tool_call_id` matches; `injected` entries (cancelled interrupts with
/// no prior tool result) are appended as proper tool entries through the
/// writer. Both are best-effort: a failed rewrite logs nothing here and
/// the caller treats resume persistence as advisory.
pub fn apply_resume_tool_results(
    dir: &Path,
    replaced: &[(String, String)],
    injected: &[(String, String)],
) {
    if !replaced.is_empty() {
        let path = dir.join("transcript.jsonl");
        if let Ok(content) = std::fs::read_to_string(&path) {
            let mut out = String::with_capacity(content.len());
            let mut changed = false;
            for line in content.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<crate::session::TranscriptEntry>(line) {
                    Ok(mut entry) => {
                        let payload = replaced
                            .iter()
                            .find(|(id, _)| entry.tool_call_id.as_deref() == Some(id.as_str()));
                        match payload {
                            Some((_, payload)) if entry.content != *payload => {
                                entry.content = payload.clone();
                                changed = true;
                            }
                            _ => {}
                        }
                        if let Ok(json) = serde_json::to_string(&entry) {
                            out.push_str(&json);
                            out.push('\n');
                            continue;
                        }
                        out.push_str(line);
                        out.push('\n');
                    }
                    // Not a native entry (legacy line, junk) — keep as-is.
                    Err(_) => {
                        out.push_str(line);
                        out.push('\n');
                    }
                }
            }
            if changed {
                let _ = crate::atomic::atomic_write(&path, out.as_bytes());
            }
        }
    }

    if !injected.is_empty() {
        let Ok(meta) = SessionReader::load_meta(dir) else {
            return;
        };
        if let Ok(mut writer) = SessionWriter::open_or_create(
            dir,
            &meta.goal,
            &meta.model,
            &meta.provider,
            meta.preset.as_deref(),
        ) {
            for (tool_call_id, content) in injected {
                let msg = crate::message::Message::tool_result(tool_call_id, content);
                let _ = writer.append(&msg, None, None);
            }
            // Keep the lifecycle status — and the failure detail (issue
            // #111) — the session already had.
            let _ = writer.finish_with_details(
                meta.status,
                meta.finish_reason.clone(),
                meta.error.clone(),
            );
        }
    }
}

// ── Legacy migration ────────────────────────────────────────────────────

/// Copy a pre-#57 flat thread into the native layout (best-effort).
///
/// Copies `transcript.jsonl` / `checkpoints.jsonl` / `.interrupts.json`,
/// rewrites the raw `Message`-shaped transcript lines into
/// `TranscriptEntry` lines (`load_transcript` skips raw Message lines
/// as corrupt — legacy history would stay invisible to
/// `episodic_recall` even with a `.meta.json`), and synthesizes a
/// `.meta.json` so the thread shows up in `sessions list` immediately.
/// The legacy directory is left in place: it is invisible to session
/// listing (no meta) and old binaries can still find it.
fn migrate_legacy_dir(workspace: &Path, thread_id: &str, new_dir: &Path) {
    let Some(legacy) = legacy_session_dir(workspace, thread_id) else {
        return;
    };
    if legacy == new_dir || !legacy.join("transcript.jsonl").is_file() {
        return;
    }
    if std::fs::create_dir_all(new_dir).is_err() {
        return;
    }
    for name in LEGACY_THREAD_FILES {
        let from = legacy.join(name);
        let to = new_dir.join(name);
        if from.is_file() && !to.exists() {
            let _ = std::fs::copy(&from, &to);
        }
    }
    rewrite_legacy_transcript(&new_dir.join("transcript.jsonl"));
    synthesize_meta_if_missing(new_dir, thread_id);
}

/// Rewrite raw `Message`-per-line content into `TranscriptEntry` lines
/// in place. Lines that don't parse as a `Message` (already native
/// entries, garbage) are kept verbatim.
fn rewrite_legacy_transcript(path: &Path) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let mut out = String::with_capacity(content.len());
    let mut chain_parent: Option<String> = None;
    let mut seq: usize = 0;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Message>(line) {
            Ok(msg) => {
                seq += 1;
                let role_str = match msg.role {
                    Role::System => "system",
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::Tool => "tool",
                };
                let uuid = uuid::Uuid::new_v4().to_string();
                let entry = crate::session::TranscriptEntry {
                    uuid: uuid.clone(),
                    parent_uuid: chain_parent.take(),
                    source_tool_assistant_uuid: None,
                    id: format!("msg_{seq:03}"),
                    parent_id: None,
                    role: role_str.to_string(),
                    content: msg.content,
                    tool_calls: msg.tool_calls,
                    tool_call_id: msg.tool_call_id,
                    reasoning_content: msg.reasoning_content,
                    usage: None,
                    timestamp: crate::session::chrono_lite_now(),
                    audit: None,
                    step: None,
                };
                chain_parent = Some(uuid);
                if let Ok(json) = serde_json::to_string(&entry) {
                    out.push_str(&json);
                    out.push('\n');
                    continue;
                }
                // Serialize failed — keep the original line rather than
                // dropping history.
                out.push_str(line);
                out.push('\n');
            }
            // Not a raw Message line (native entry or junk) — keep as-is.
            Err(_) => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    if !out.is_empty() {
        let _ = crate::atomic::atomic_write(path, out.as_bytes());
    }
}

/// Best-effort `.meta.json` synthesis for a migrated legacy thread.
fn synthesize_meta_if_missing(dir: &Path, thread_id: &str) {
    let meta_path = dir.join(".meta.json");
    if meta_path.is_file() {
        return;
    }
    let entries = SessionReader::load_transcript(dir).unwrap_or_default();
    let goal = entries
        .iter()
        .find(|e| e.role == "user")
        .map(|e| e.content.chars().take(200).collect::<String>())
        .unwrap_or_default();
    let name = entries
        .first()
        .map(|e| e.content.chars().take(60).collect::<String>());
    let session_id = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| thread_session_key(thread_id));
    let now = crate::session::chrono_lite_now();
    let meta = SessionMeta {
        schema_version: crate::session::SUPPORTED_SESSION_SCHEMA_VERSION,
        session_id,
        goal,
        model: "unknown".to_string(),
        provider: "unknown".to_string(),
        created_at: now.clone(),
        updated_at: now,
        message_count: entries.len() as u64,
        status: SessionStatus::Completed,
        tool_registry_hash: None,
        first_prompt: None,
        last_prompt: None,
        cost: None,
        preset: None,
        name,
        derived_from: None,
        finish_reason: None,
        error: None,
    };
    if let Ok(json) = serde_json::to_string_pretty(&meta) {
        let _ = crate::atomic::atomic_write(&meta_path, json.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionStatus;
    use crate::test_util::IsolatedWorkspace;

    fn user_msg(text: &str) -> Message {
        Message {
            role: Role::User,
            content: text.to_string(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            reasoning_content: None,
            is_compaction_summary: false,
        }
    }

    fn assistant_msg(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: text.to_string(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            reasoning_content: None,
            is_compaction_summary: false,
        }
    }

    fn usage(total: u32) -> TokenUsage {
        TokenUsage {
            prompt_tokens: total,
            completion_tokens: total / 2,
            total_tokens: total + total / 2,
            cache_hit_tokens: 0,
            cache_miss_tokens: total,
            reasoning_tokens: 0,
        }
    }

    fn record<'a>(
        ws: &'a Path,
        thread: &'a str,
        msgs: &'a [Message],
        usage: Option<TokenUsage>,
    ) -> RunRecord<'a> {
        RunRecord {
            workspace: ws,
            thread_id: thread,
            messages: msgs,
            goal: "first prompt",
            model: "deepseek-chat",
            provider: "deepseek",
            preset: None,
            status: SessionStatus::Completed,
            finish_reason: None,
            error: None,
            usage,
            llm_latency_ms: 7,
        }
    }

    // ── thread_session_key ────────────────────────────────────────────────

    #[test]
    fn thread_session_key_is_deterministic_and_collision_free() {
        // Same input → same key (resume derives the dir without a registry).
        assert_eq!(thread_session_key("t/1"), thread_session_key("t/1"));
        // Distinct inputs → distinct keys. These two collided under the
        // legacy sanitiser (both became "tenantA-user1-conv1").
        assert_ne!(
            thread_session_key("tenantA/user1/conv1"),
            thread_session_key("tenantA/user1-conv1")
        );
        assert_ne!(
            thread_session_key("tenant-a/user1/conv1"),
            thread_session_key("tenantA/user1/conv1")
        );
    }

    #[test]
    fn thread_session_key_is_checkpoint_session_id_safe() {
        for thread in ["", "a/b", "..", ".hidden", "tenantA/user1/conv1", "ümlaut"] {
            let key = thread_session_key(thread);
            assert!(key.starts_with("agui-"), "key must keep the agui- prefix");
            assert!(!key.contains("..") && !key.starts_with('.'));
            assert!(key.len() == 21, "agui- + 16 hex chars, got {key}");
            assert!(key[5..].chars().all(|c| c.is_ascii_hexdigit()));
        }
    }

    // ── persist_run: native layout + visibility ──────────────────────────

    #[test]
    fn persist_run_writes_a_listable_native_session() {
        let ws_tmp = IsolatedWorkspace::new();
        let ws = ws_tmp.path();

        let msgs = vec![user_msg("hello thread"), assistant_msg("hi there")];
        let dir = persist_run(record(ws, "thread-1", &msgs, Some(usage(100)))).expect("persist");

        // Documented layout: <sessions>/<slug>/agui-<hash>/
        let base = crate::paths::user_sessions_dir(ws).unwrap();
        let slug = crate::session::workspace_slug(ws);
        assert_eq!(
            dir,
            base.join(&slug).join(thread_session_key("thread-1")),
            "session dir must live under the workspace slug"
        );

        // The exact consumer from the issue report: list_sessions must
        // now see the thread.
        let listed = SessionReader::list_sessions(ws).unwrap();
        assert!(
            listed.iter().any(|p| p == &dir),
            "AG-UI thread must be visible to SessionReader::list_sessions, listed={listed:?}"
        );

        // Meta parses, carries identity + cost.
        let meta = SessionReader::load_meta(&dir).unwrap();
        assert_eq!(meta.session_id, thread_session_key("thread-1"));
        assert_eq!(meta.goal, "first prompt");
        assert_eq!(meta.model, "deepseek-chat");
        assert_eq!(meta.status, SessionStatus::Completed);
        assert_eq!(meta.message_count, 2);
        assert_eq!(meta.first_prompt.as_deref(), Some("hello thread"));
        let cost = meta.cost.expect("usage must land in .meta.json cost");
        assert_eq!(cost.total_input_tokens, 100);
        assert_eq!(cost.total_output_tokens, 50);

        // Transcript is native-format (uuid chain + timestamps), not raw
        // Message lines — otherwise episodic_recall still sees nothing.
        let entries = SessionReader::load_transcript(&dir).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].role, "user");
        assert!(!entries[0].uuid.is_empty(), "entries must carry uuids");
        assert_eq!(
            entries[1].parent_uuid.as_deref(),
            Some(entries[0].uuid.as_str())
        );

        // cost.json written by the tracker.
        assert!(dir.join("cost.json").is_file());
        let cost_json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("cost.json")).unwrap()).unwrap();
        assert_eq!(cost_json["total_usage"]["total_tokens"], 150);
    }

    #[test]
    fn persist_run_appends_across_runs_and_preserves_created_at() {
        let ws_tmp = IsolatedWorkspace::new();
        let ws = ws_tmp.path();

        let first = vec![user_msg("run one"), assistant_msg("done one")];
        let dir = persist_run(record(ws, "thread-2", &first, Some(usage(10)))).unwrap();
        let meta1 = SessionReader::load_meta(&dir).unwrap();

        let second = vec![user_msg("run two")];
        let dir2 = persist_run(record(ws, "thread-2", &second, Some(usage(20)))).unwrap();
        assert_eq!(dir, dir2, "same thread must map to the same session dir");

        let meta2 = SessionReader::load_meta(&dir).unwrap();
        assert_eq!(meta2.created_at, meta1.created_at, "created_at is sticky");
        assert_eq!(
            meta2.message_count, 3,
            "second run appends instead of overwriting"
        );
        let cost = meta2.cost.unwrap();
        assert_eq!(cost.total_input_tokens, 30, "cost accumulates across runs");
        // Resume-visible transcript carries both runs.
        let entries = SessionReader::load_transcript(&dir).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[2].content, "run two");
    }

    #[test]
    fn persist_run_without_usage_skips_cost_but_still_lists() {
        let ws_tmp = IsolatedWorkspace::new();
        let ws = ws_tmp.path();
        let msgs = vec![user_msg("failed run")];
        let dir = persist_run(record(ws, "thread-3", &msgs, None)).unwrap();
        let meta = SessionReader::load_meta(&dir).unwrap();
        assert!(meta.cost.is_none());
        assert!(!dir.join("cost.json").exists());
        assert!(SessionReader::list_sessions(ws)
            .unwrap()
            .iter()
            .any(|p| p == &dir));
    }

    // ── legacy migration ─────────────────────────────────────────────────

    #[test]
    fn legacy_flat_thread_is_migrated_and_becomes_visible() {
        let ws_tmp = IsolatedWorkspace::new();
        let ws = ws_tmp.path();
        let base = crate::paths::user_sessions_dir(ws).unwrap();

        // A pre-#57 thread: flat dir, raw Message lines, no meta, with
        // interrupts and checkpoints alongside.
        let legacy = base.join(legacy_thread_dir_name("legacy/thread"));
        std::fs::create_dir_all(&legacy).unwrap();
        let raw = format!(
            "{}\n{}\n",
            serde_json::to_string(&user_msg("legacy question")).unwrap(),
            serde_json::to_string(&assistant_msg("legacy answer")).unwrap()
        );
        std::fs::write(legacy.join("transcript.jsonl"), raw).unwrap();
        std::fs::write(legacy.join("checkpoints.jsonl"), "{\"turn\":0}\n").unwrap();
        std::fs::write(legacy.join(".interrupts.json"), "[]").unwrap();

        let dir = resolve_session_dir(ws, "legacy/thread").unwrap();
        assert_eq!(
            dir,
            base.join(crate::session::workspace_slug(ws))
                .join(thread_session_key("legacy/thread"))
        );

        // Meta synthesized → visible to list_sessions right away.
        let listed = SessionReader::list_sessions(ws).unwrap();
        assert!(
            listed.iter().any(|p| p == &dir),
            "migrated thread must list"
        );
        let meta = SessionReader::load_meta(&dir).unwrap();
        assert_eq!(meta.message_count, 2);
        assert_eq!(meta.goal, "legacy question");
        assert_eq!(meta.status, SessionStatus::Completed);

        // Raw Message lines rewritten into native entries so memory tools
        // can read the history.
        let entries = SessionReader::load_transcript(&dir).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].content, "legacy question");
        assert_eq!(entries[1].content, "legacy answer");

        // Companion files followed the transcript.
        assert!(dir.join("checkpoints.jsonl").is_file());
        assert!(dir.join(".interrupts.json").is_file());

        // Resolving again is idempotent — no duplicate listing, content
        // not clobbered.
        let dir_again = resolve_session_dir(ws, "legacy/thread").unwrap();
        assert_eq!(dir, dir_again);
        assert_eq!(SessionReader::load_transcript(&dir).unwrap().len(), 2);
        assert_eq!(SessionReader::list_sessions(ws).unwrap().len(), 1);
    }

    #[test]
    fn legacy_thread_then_new_run_appends_after_migrated_history() {
        let ws_tmp = IsolatedWorkspace::new();
        let ws = ws_tmp.path();
        let base = crate::paths::user_sessions_dir(ws).unwrap();
        let legacy = base.join(legacy_thread_dir_name("old"));
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(
            legacy.join("transcript.jsonl"),
            format!(
                "{}\n",
                serde_json::to_string(&user_msg("before upgrade")).unwrap()
            ),
        )
        .unwrap();

        let msgs = vec![user_msg("after upgrade")];
        let dir = persist_run(record(ws, "old", &msgs, None)).unwrap();
        let entries = SessionReader::load_transcript(&dir).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].content, "before upgrade");
        assert_eq!(entries[1].content, "after upgrade");
        assert_eq!(SessionReader::load_meta(&dir).unwrap().message_count, 2);
    }

    // ── resume tool-result splices ───────────────────────────────────────

    #[test]
    fn resume_tool_results_are_applied_on_disk() {
        let ws_tmp = IsolatedWorkspace::new();
        let ws = ws_tmp.path();

        let msgs = vec![
            user_msg("weather?"),
            Message {
                role: Role::Assistant,
                content: "checking".to_string(),
                tool_calls: vec![crate::llm::ToolCall {
                    id: "t1".to_string(),
                    name: "get_weather".to_string(),
                    arguments: serde_json::json!({}),
                }],
                tool_call_id: None,
                reasoning_content: None,
                is_compaction_summary: false,
            },
            Message {
                role: Role::Tool,
                content: "[frontend tool] denied".to_string(),
                tool_calls: Vec::new(),
                tool_call_id: Some("t1".to_string()),
                reasoning_content: None,
                is_compaction_summary: false,
            },
        ];
        let dir = persist_run(record(ws, "resume-thread", &msgs, None)).unwrap();

        // Replaced-in-place: the deny text becomes the real payload.
        apply_resume_tool_results(
            &dir,
            &[("t1".to_string(), "{\"forecast\":\"sunny\"}".to_string())],
            &[],
        );
        let entries = SessionReader::load_transcript(&dir).unwrap();
        assert_eq!(entries[2].tool_call_id.as_deref(), Some("t1"));
        assert_eq!(entries[2].content, "{\"forecast\":\"sunny\"}");
        // The tool-call pairing above it is untouched.
        assert_eq!(entries[1].tool_calls[0].id, "t1");
        assert_eq!(entries.len(), 3);

        // Injected (cancelled interrupt): appended as a proper tool entry.
        apply_resume_tool_results(
            &dir,
            &[],
            &[(
                "t9".to_string(),
                "[interrupt cancelled by user]".to_string(),
            )],
        );
        let entries = SessionReader::load_transcript(&dir).unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[3].role, "tool");
        assert_eq!(entries[3].tool_call_id.as_deref(), Some("t9"));
        assert_eq!(entries[3].content, "[interrupt cancelled by user]");
        // Chain stays linked and meta count follows.
        assert_eq!(
            entries[3].parent_uuid.as_deref(),
            Some(entries[2].uuid.as_str())
        );
        assert_eq!(SessionReader::load_meta(&dir).unwrap().message_count, 4);
    }

    // ── legacy sanitiser (kept for migration lookups only) ───────────────

    #[test]
    fn legacy_sanitize_still_blocks_traversal() {
        assert_eq!(legacy_sanitize_thread_id("abc-123"), "abc-123");
        assert_eq!(legacy_sanitize_thread_id("foo_bar.baz"), "foo_bar.baz");
        let out = legacy_sanitize_thread_id("a/b:c");
        assert!(!out.contains('/') && !out.contains(':'));
        assert!(!legacy_sanitize_thread_id(".hidden").starts_with('.'));
        assert!(!legacy_sanitize_thread_id("a..b").contains(".."));
        assert_eq!(legacy_sanitize_thread_id(""), "default");
    }
}
