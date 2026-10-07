//! Issue #121: mirror an HTTP session into the native session layout.
//!
//! HTTP sessions persist through [`crate::storage::StorageBackend`] — a flat
//! `<sessions>/<id>.jsonl` plus a `session-meta/<id>` blob — so they were
//! invisible to every consumer of
//! [`crate::session::SessionReader::list_sessions`]: `recursive sessions list`,
//! the resume picker, `episodic_recall`. This module gives an HTTP session the
//! same on-disk shape #57 gave AG-UI threads ([`crate::agui_session`]): a native
//! `<sessions>/<workspace-slug>/<id>/` directory carrying `.meta.json` +
//! `transcript.jsonl`, so the CLI sees one session layout regardless of
//! transport. Like [`crate::session::SessionWriter`], the write holds a
//! [`SessionLock`] so a mirror can never clobber a session a live
//! `recursive resume` owns.
//!
//! The mirror is best-effort by design: the flat `StorageBackend` transcript
//! stays authoritative for cold load ([`super::cold_load`]), so a mirror
//! failure only means the session is missing from the CLI listing — never data
//! loss, and never a teardown error.
//!
//! It runs at teardown only (session eviction and graceful shutdown), so a
//! session that is still running is not yet listed by the CLI (`recursive
//! sessions list` / `agents`); it appears once it closes. `recursive agents`
//! therefore reports native sessions (CLI runs, resumed sessions) as `live`
//! but cannot see a running HTTP session.
//!
//! The sessions root is **injected** ([`AppState::session_mirror_root`]), not
//! resolved here: production pins it once at startup, and `None` disables the
//! mirror. Resolution stays out of the teardown path so a teardown never
//! depends on process-global env that may have moved under it.
//!
//! [`AppState::session_mirror_root`]: super::AppState::session_mirror_root

use std::path::{Path, PathBuf};

use crate::message::{Message, Role};
use crate::session::{
    chrono_lite_now, workspace_slug, SessionCost, SessionLock, SessionMeta, SessionStatus,
    TranscriptEntry, SUPPORTED_SESSION_SCHEMA_VERSION,
};

/// Everything the mirror needs from the closing session.
pub(super) struct MirrorInput<'a> {
    pub workspace: &'a Path,
    pub id: &'a str,
    pub created_at: &'a str,
    pub transcript: &'a [Message],
    pub model: &'a str,
    pub provider: &'a str,
    pub preset: Option<&'a str>,
    pub name: Option<&'a str>,
    pub cost: Option<SessionCost>,
    pub status: SessionStatus,
}

/// Native session directory for an HTTP session id, or `None` when the id is
/// not filesystem-safe. HTTP ids are server-minted UUIDs, but a value that
/// reaches a path never gets the benefit of the doubt.
fn native_session_dir(root: &Path, workspace: &Path, id: &str) -> Option<PathBuf> {
    let safe = !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !safe {
        return None;
    }
    Some(root.join(workspace_slug(workspace)).join(id))
}

/// Mirror `input` into `root` (the sessions root) in the native layout;
/// delegates to [`mirror_into`] once the directory resolves. An unsafe id is a
/// silent no-op.
pub(super) fn mirror_session(root: &Path, input: &MirrorInput<'_>) {
    let Some(dir) = native_session_dir(root, input.workspace, input.id) else {
        return;
    };
    mirror_into(&dir, input);
}

/// Write the native `.meta.json` + `transcript.jsonl` under `dir`, overwriting
/// any previous mirror (an evicted session that cold-loads and is evicted again
/// must not append duplicate history).
///
/// Takes the same [`SessionLock`] `SessionWriter` does: if a live
/// `recursive resume` owns this directory the mirror stands down rather than
/// clobbering it, so the two writers can never interleave.
///
/// Residual limitation (out of scope here): the mirror is a snapshot of the
/// HTTP transcript, so if a session is resumed *after* it was mirrored and the
/// HTTP server later cold-loads and evicts it again, the snapshot wins. Routing
/// both transports through one writer is the layering follow-up in
/// `crate::agui_session` (#56).
fn mirror_into(dir: &Path, input: &MirrorInput<'_>) {
    let Ok(_lock) = SessionLock::acquire(dir) else {
        return;
    };
    // Both files or neither: serialization cannot fail for this shape, but
    // writing a `.meta.json` whose `transcript.jsonl` was skipped would list a
    // session with no transcript.
    let Some(transcript) = serialize_transcript(input.transcript) else {
        return;
    };
    let Ok(meta) = serde_json::to_string_pretty(&build_meta(input)) else {
        return;
    };
    let _ = crate::atomic::atomic_write(&dir.join("transcript.jsonl"), transcript.as_bytes());
    let _ = crate::atomic::atomic_write(&dir.join(".meta.json"), meta.as_bytes());
}

fn role_str(role: &Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// Serialize the runtime transcript as JSONL. One `TranscriptEntry` per line,
/// chained by `parent_uuid` so the mirrored session is a valid resume seed
/// (same shape `SessionWriter` emits). `None` if an entry cannot be
/// serialized — the caller then writes nothing rather than a half transcript
/// that resume would silently truncate at the corrupt line.
fn serialize_transcript(transcript: &[Message]) -> Option<String> {
    let mut out = String::new();
    let mut parent_uuid: Option<String> = None;
    for (i, msg) in transcript.iter().enumerate() {
        let uuid = uuid::Uuid::new_v4().to_string();
        let entry = TranscriptEntry {
            uuid: uuid.clone(),
            parent_uuid: parent_uuid.take(),
            source_tool_assistant_uuid: None,
            id: format!("msg_{:03}", i + 1),
            parent_id: (i > 0).then(|| format!("msg_{:03}", i)),
            role: role_str(&msg.role).to_string(),
            content: msg.content.clone(),
            tool_calls: msg.tool_calls.clone(),
            tool_call_id: msg.tool_call_id.clone(),
            reasoning_content: msg.reasoning_content.clone(),
            usage: None,
            timestamp: chrono_lite_now(),
            audit: None,
            step: None,
        };
        let line = serde_json::to_string(&entry).ok()?;
        parent_uuid = Some(uuid);
        out.push_str(&line);
        out.push('\n');
    }
    Some(out)
}

fn prompt_of(msg: &Message) -> String {
    msg.content.chars().take(200).collect()
}

fn build_meta(input: &MirrorInput<'_>) -> SessionMeta {
    let first_prompt = input
        .transcript
        .iter()
        .find(|m| matches!(m.role, Role::User))
        .map(prompt_of);
    let last_prompt = input
        .transcript
        .iter()
        .rev()
        .find(|m| matches!(m.role, Role::User))
        .map(prompt_of);
    let goal = first_prompt.clone().unwrap_or_default();
    SessionMeta {
        schema_version: SUPPORTED_SESSION_SCHEMA_VERSION,
        session_id: input.id.to_string(),
        goal,
        model: input.model.to_string(),
        provider: input.provider.to_string(),
        created_at: input.created_at.to_string(),
        updated_at: chrono_lite_now(),
        message_count: input.transcript.len() as u64,
        status: input.status,
        tool_registry_hash: None,
        first_prompt,
        last_prompt,
        cost: input.cost.clone(),
        preset: input.preset.map(|s| s.to_string()),
        name: input.name.map(|s| s.to_string()),
        derived_from: None,
        finish_reason: None,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: Role, content: &str) -> Message {
        Message {
            role,
            content: content.to_string(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            reasoning_content: None,
            is_compaction_summary: false,
        }
    }

    fn input<'a>(id: &'a str, created_at: &'a str, transcript: &'a [Message]) -> MirrorInput<'a> {
        MirrorInput {
            workspace: Path::new("/tmp/ws"),
            id,
            created_at,
            transcript,
            model: "deepseek-chat",
            provider: "deepseek",
            preset: Some("deepseek"),
            name: Some("title"),
            cost: Some(SessionCost {
                total_input_tokens: 10,
                total_output_tokens: 20,
                ..Default::default()
            }),
            status: SessionStatus::Completed,
        }
    }

    #[test]
    fn unsafe_ids_never_resolve_to_a_path() {
        let root = Path::new("/tmp/root");
        let ws = Path::new("/tmp/ws");
        assert!(native_session_dir(root, ws, "../escape").is_none());
        assert!(native_session_dir(root, ws, "a/b").is_none());
        assert!(native_session_dir(root, ws, "").is_none());
    }

    #[test]
    fn native_session_dir_resolves_under_the_injected_root() {
        let root = Path::new("/tmp/mirror-root");
        let ws = Path::new("/tmp/ws");
        let dir = native_session_dir(root, ws, "sess-1").expect("a safe id resolves");
        assert_eq!(dir, root.join(workspace_slug(ws)).join("sess-1"));
    }

    /// The mirror writes only where it is pointed: the injected root, never a
    /// path derived from the ambient environment.
    #[test]
    fn mirror_session_writes_under_the_injected_root() {
        let root = tempfile::tempdir().unwrap();
        let ws = Path::new("/tmp/ws");
        let transcript = vec![msg(Role::User, "hello there")];
        mirror_session(
            root.path(),
            &input("sess-1", "2026-01-01T00:00:00Z", &transcript),
        );

        let dir = root.path().join(workspace_slug(ws)).join("sess-1");
        assert!(
            dir.join("transcript.jsonl").exists(),
            "the mirror must land under the injected root"
        );
        assert!(dir.join(".meta.json").exists());
    }

    #[test]
    fn a_live_lock_blocks_the_mirror() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("mirror");
        let _held = SessionLock::acquire(&session_dir).expect("take the lock");

        let transcript = vec![msg(Role::User, "hello there")];
        mirror_into(&session_dir, &input("sess-1", "t0", &transcript));

        assert!(
            !session_dir.join("transcript.jsonl").exists(),
            "a session held by a live owner must not be overwritten"
        );
        assert!(
            !session_dir.join(".meta.json").exists(),
            "a live-locked session must not be mirrored at all"
        );
    }

    #[test]
    fn mirror_writes_meta_and_chained_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("mirror");
        let transcript = vec![
            msg(Role::System, "sys"),
            msg(Role::User, "hello there"),
            msg(Role::Assistant, "hi"),
        ];
        let inp = input("sess-1", "2026-01-01T00:00:00Z", &transcript);
        mirror_into(&session_dir, &inp);

        let meta = crate::session::SessionReader::load_meta(&session_dir).unwrap();
        assert_eq!(meta.session_id, "sess-1");
        assert_eq!(meta.goal, "hello there");
        assert_eq!(meta.message_count, 3);
        assert_eq!(meta.model, "deepseek-chat");
        assert_eq!(meta.preset.as_deref(), Some("deepseek"));
        assert_eq!(meta.name.as_deref(), Some("title"));
        assert_eq!(meta.cost.as_ref().unwrap().total_output_tokens, 20);

        let entries = crate::session::SessionReader::load_full_history(&session_dir).unwrap();
        assert_eq!(entries.len(), 3);
    }

    /// The mirrored transcript must be a valid resume seed: `SessionWriter`'s
    /// `msg_NNN` id / `parent_id` pairing, a `parent_uuid` chain, and the role
    /// names every reader spells the same way.
    #[test]
    fn mirrored_transcript_is_a_chained_resume_seed() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("mirror");
        let transcript = vec![
            msg(Role::System, "sys"),
            msg(Role::User, "hello there"),
            msg(Role::Assistant, "hi"),
            msg(Role::Tool, "42"),
        ];
        mirror_into(&session_dir, &input("sess-1", "t0", &transcript));

        let entries = crate::session::SessionReader::load_full_history(&session_dir).unwrap();
        let msgs: Vec<&crate::session::TranscriptEntry> = entries
            .iter()
            .map(|e| match e {
                crate::session::LoadedEntry::Message(m) => m.as_ref(),
                other => panic!("unexpected entry: {other:?}"),
            })
            .collect();
        assert_eq!(msgs.len(), 4);

        let roles: Vec<&str> = msgs.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["system", "user", "assistant", "tool"]);

        assert_eq!(msgs[0].id, "msg_001");
        assert_eq!(
            msgs[0].parent_id, None,
            "the first entry starts the id chain"
        );
        assert_eq!(msgs[0].parent_uuid, None, "and the uuid chain too");
        for (i, entry) in msgs.iter().enumerate().skip(1) {
            assert_eq!(entry.id, format!("msg_{:03}", i + 1));
            assert_eq!(
                entry.parent_id.as_deref(),
                Some(format!("msg_{:03}", i).as_str()),
                "entry {i} must chain to its predecessor's id"
            );
            assert_eq!(
                entry.parent_uuid.as_deref(),
                Some(msgs[i - 1].uuid.as_str()),
                "entry {i} must chain to its predecessor's uuid"
            );
        }
    }

    #[test]
    fn mirror_overwrites_instead_of_appending() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("mirror");
        let first = vec![msg(Role::User, "one")];
        mirror_into(&session_dir, &input("sess-1", "t0", &first));
        let second = vec![msg(Role::User, "one"), msg(Role::Assistant, "two")];
        mirror_into(&session_dir, &input("sess-1", "t0", &second));

        let entries = crate::session::SessionReader::load_full_history(&session_dir).unwrap();
        assert_eq!(
            entries.len(),
            2,
            "a second mirror must not append duplicates"
        );
        let meta = crate::session::SessionReader::load_meta(&session_dir).unwrap();
        assert_eq!(meta.message_count, 2);
    }
}
