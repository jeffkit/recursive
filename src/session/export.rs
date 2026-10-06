//! Streaming export of a whole session tree (issue #131, borrowed from DSH
//! `session-log-export`).
//!
//! An export is a ZIP built **while it is written**: the archive is produced
//! straight into the caller's `Write` sink, so a large session history is never
//! held in memory as a whole. A session tree is the root session plus every
//! session that records it as its [`derived_from`](SessionMeta::derived_from)
//! source (a fork / sub-session) plus the session directory's non-transcript
//! files (the attachments: cost ledger, pending wakeup, …).
//!
//! Only one export of a given session may be in flight at a time: the archive
//! is a consistent snapshot of the tree, and two concurrent downloads of a
//! session whose writer is mid-append would each capture a different prefix.
//! [`try_acquire_export`] hands out the process-wide lock, and
//! [`export_session_tree`] takes it for the duration of the stream.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::error::{Error, Result};
use crate::session::reader::SessionReader;

/// Marker string written into the manifest so a reader can tell what it got.
pub const EXPORT_FORMAT: &str = "recursive-session-tree";
/// Manifest schema version; bump on an incompatible manifest change.
pub const EXPORT_FORMAT_VERSION: u32 = 1;

/// The `.lock` sentinel is a live-process marker, not session content.
const LOCK_FILE: &str = ".lock";
const TRANSCRIPT_FILE: &str = "transcript.jsonl";
const MANIFEST_FILE: &str = "manifest.json";

/// What an archive entry holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportKind {
    /// `transcript.jsonl` — the session's history.
    Transcript,
    /// `.meta.json` — the session header.
    Meta,
    /// Any other file in the session directory.
    Attachment,
    /// `manifest.json` — the archive's own table of contents.
    Manifest,
}

impl ExportKind {
    /// Lowercase name used in the manifest.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Transcript => "transcript",
            Self::Meta => "meta",
            Self::Attachment => "attachment",
            Self::Manifest => "manifest",
        }
    }
}

/// One file in the archive (also one row of the manifest).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportEntry {
    /// Path inside the archive.
    pub path: String,
    pub kind: ExportKind,
    pub bytes: u64,
}

/// Result of a successful export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionExport {
    /// Root session id.
    pub session_id: String,
    /// Root session first, then its descendants (breadth-first).
    pub sessions: Vec<String>,
    /// Every file written, manifest included, in write order.
    pub entries: Vec<ExportEntry>,
}

/// One node of the session tree.
struct SessionNode {
    id: String,
    dir: PathBuf,
    children: Vec<SessionNode>,
}

/// Export a session and everything derived from it into `out` as a ZIP stream.
///
/// Fails with [`Error::ExportInProgress`] when another export of the same
/// session is already running.
pub fn export_session_tree(
    workspace: &Path,
    session_id: &str,
    out: impl Write,
) -> Result<SessionExport> {
    let _guard = try_acquire_export(session_id).ok_or_else(|| Error::ExportInProgress {
        session_id: session_id.to_string(),
    })?;
    let tree = collect_tree(workspace, session_id)?;
    let mut sessions = Vec::new();
    let mut files: Vec<(String, ExportKind, PathBuf)> = Vec::new();
    collect_files(&tree, "", &mut sessions, &mut files)?;

    let mut archive = zip::ZipWriter::new_stream(out);
    let options =
        zip::write::FileOptions::<()>::default().compression_method(zip::CompressionMethod::Stored);

    let mut entries = Vec::with_capacity(files.len() + 1);
    let manifest_path = MANIFEST_FILE.to_string();
    let manifest = manifest_json(session_id, &sessions, &files);
    write_entry(&mut archive, &options, &manifest_path, manifest.as_bytes())?;
    entries.push(ExportEntry {
        path: manifest_path,
        kind: ExportKind::Manifest,
        bytes: manifest.len() as u64,
    });

    for (path, kind, source) in &files {
        let bytes = std::fs::read(source).map_err(|e| Error::Storage {
            message: format!("read export source {}: {e}", source.display()),
        })?;
        write_entry(&mut archive, &options, path, &bytes)?;
        entries.push(ExportEntry {
            path: path.clone(),
            kind: *kind,
            bytes: bytes.len() as u64,
        });
    }
    archive.finish().map_err(zip_err)?;

    Ok(SessionExport {
        session_id: session_id.to_string(),
        sessions,
        entries,
    })
}

fn write_entry<A: Write + std::io::Seek>(
    archive: &mut zip::ZipWriter<A>,
    options: &zip::write::FileOptions<()>,
    path: &str,
    bytes: &[u8],
) -> Result<()> {
    archive.start_file(path, *options).map_err(zip_err)?;
    archive.write_all(bytes).map_err(zip_err)
}

fn zip_err(e: impl std::fmt::Display) -> Error {
    Error::Storage {
        message: format!("session export: {e}"),
    }
}

/// Walk the tree (root first), appending `(archive path, kind, source file)`.
fn collect_files(
    node: &SessionNode,
    prefix: &str,
    sessions: &mut Vec<String>,
    files: &mut Vec<(String, ExportKind, PathBuf)>,
) -> Result<()> {
    let base = format!("{prefix}{}/", node.id);
    sessions.push(node.id.clone());
    let mut names: Vec<String> = std::fs::read_dir(&node.dir)
        .map_err(|e| Error::Storage {
            message: format!("read session dir {}: {e}", node.dir.display()),
        })?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in names {
        let source = node.dir.join(&name);
        if !source.is_file() || name == LOCK_FILE {
            continue;
        }
        let kind = match name.as_str() {
            TRANSCRIPT_FILE => ExportKind::Transcript,
            ".meta.json" => ExportKind::Meta,
            _ => ExportKind::Attachment,
        };
        files.push((format!("{base}{name}"), kind, source));
    }
    for child in &node.children {
        collect_files(child, &format!("{base}children/"), sessions, files)?;
    }
    Ok(())
}

/// Load every session header of the workspace, then link children to their
/// `derived_from` source. Sessions whose metadata cannot be read are skipped —
/// a half-written session must not fail the export.
fn collect_tree(workspace: &Path, session_id: &str) -> Result<SessionNode> {
    let mut by_id: Vec<(String, PathBuf, Option<String>)> = Vec::new();
    let dirs = SessionReader::list_sessions(workspace).map_err(|e| Error::Storage {
        message: format!("list sessions: {e}"),
    })?;
    for dir in dirs {
        let Ok(meta) = SessionReader::load_meta(&dir) else {
            continue;
        };
        by_id.push((meta.session_id, dir, meta.derived_from));
    }
    if !by_id.iter().any(|(id, _, _)| id == session_id) {
        return Err(Error::NotFound(format!("session {session_id}")));
    }
    build_node(session_id, &by_id, &mut HashSet::new())
}

fn build_node(
    session_id: &str,
    by_id: &[(String, PathBuf, Option<String>)],
    visiting: &mut HashSet<String>,
) -> Result<SessionNode> {
    if !visiting.insert(session_id.to_string()) {
        // A derived_from cycle: stop rather than recurse forever.
        return Err(Error::Storage {
            message: format!("session tree cycle at {session_id}"),
        });
    }
    let (_, dir, _) = by_id
        .iter()
        .find(|(id, _, _)| id == session_id)
        .ok_or_else(|| Error::NotFound(format!("session {session_id}")))?;
    let mut children = Vec::new();
    for (id, _, derived_from) in by_id {
        if derived_from.as_deref() == Some(session_id) {
            children.push(build_node(id, by_id, visiting)?);
        }
    }
    visiting.remove(session_id);
    Ok(SessionNode {
        id: session_id.to_string(),
        dir: dir.clone(),
        children,
    })
}

fn manifest_json(
    session_id: &str,
    sessions: &[String],
    files: &[(String, ExportKind, PathBuf)],
) -> String {
    let entries: Vec<serde_json::Value> = files
        .iter()
        .map(|(path, kind, source)| {
            let bytes = std::fs::metadata(source).map(|m| m.len()).unwrap_or(0);
            serde_json::json!({"path": path, "kind": kind.as_str(), "bytes": bytes})
        })
        .collect();
    let manifest = serde_json::json!({
        "format": EXPORT_FORMAT,
        "version": EXPORT_FORMAT_VERSION,
        "session_id": session_id,
        "exported_at": crate::session::chrono_lite_now(),
        "sessions": sessions,
        "entries": entries,
    });
    serde_json::to_string_pretty(&manifest).unwrap_or_else(|_| "{}".to_string())
}

// ── single-download lock ────────────────────────────────────────────────────

fn locks() -> &'static Mutex<HashSet<String>> {
    static LOCKS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Process-wide "one export per session" guard. Dropping it releases the slot.
#[derive(Debug)]
pub struct ExportGuard {
    session_id: String,
}

impl ExportGuard {
    /// Session this guard holds.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

impl Drop for ExportGuard {
    fn drop(&mut self) {
        if let Ok(mut held) = locks().lock() {
            held.remove(&self.session_id);
        }
    }
}

/// Take the export lock for `session_id`, or `None` when an export is already
/// running. A poisoned lock is recovered: the set is rebuilt from scratch, so a
/// panicking exporter cannot wedge every later download.
pub fn try_acquire_export(session_id: &str) -> Option<ExportGuard> {
    let mut held = match locks().lock() {
        Ok(held) => held,
        Err(poisoned) => poisoned.into_inner(),
    };
    if !held.insert(session_id.to_string()) {
        return None;
    }
    Some(ExportGuard {
        session_id: session_id.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;
    use crate::session::{SessionStatus, SessionWriter};
    use crate::test_util::IsolatedWorkspace;
    use std::io::Read;

    fn write_session(ws: &Path, goal: &str, body: &str) -> String {
        let mut writer = SessionWriter::create(ws, goal, "m", "p").unwrap();
        writer
            .append(&Message::user(body.to_string()), None, None)
            .unwrap();
        writer.finish(SessionStatus::Completed).unwrap();
        writer.session_id().to_string()
    }

    fn zip_names(bytes: &[u8]) -> Vec<String> {
        let mut archive =
            zip::ZipArchive::new(std::io::Cursor::new(bytes.to_vec())).expect("valid zip");
        (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect()
    }

    fn zip_read(bytes: &[u8], name: &str) -> String {
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes.to_vec())).unwrap();
        let mut file = archive.by_name(name).expect("entry exists");
        let mut out = String::new();
        file.read_to_string(&mut out).unwrap();
        out
    }

    #[test]
    fn export_includes_transcript_meta_and_attachments() {
        let env = IsolatedWorkspace::new();
        let session_id = write_session(env.path(), "export me", "hello export");
        // An attachment: anything in the session dir that is not the transcript.
        let dir = SessionReader::list_sessions(env.path()).unwrap()[0].clone();
        std::fs::write(dir.join("cost.json"), "{\"usd\":0}").unwrap();
        std::fs::write(dir.join(LOCK_FILE), "stale lock").unwrap();

        let mut out = Vec::new();
        let export = export_session_tree(env.path(), &session_id, &mut out).unwrap();
        assert_eq!(export.session_id, session_id);
        assert_eq!(export.sessions, vec![session_id.clone()]);

        let names = zip_names(&out);
        assert!(names.contains(&MANIFEST_FILE.to_string()));
        assert!(names.contains(&format!("{session_id}/{TRANSCRIPT_FILE}")));
        assert!(names.contains(&format!("{session_id}/.meta.json")));
        assert!(names.contains(&format!("{session_id}/cost.json")));
        assert!(
            !names.iter().any(|n| n.ends_with(LOCK_FILE)),
            "the lock sentinel is not session content: {names:?}"
        );

        let manifest = zip_read(&out, MANIFEST_FILE);
        assert!(manifest.contains(EXPORT_FORMAT));
        assert!(manifest.contains("\"kind\": \"attachment\""));
        assert!(zip_read(&out, &format!("{session_id}/cost.json")).contains("usd"));
    }

    #[test]
    fn export_walks_child_sessions() {
        let env = IsolatedWorkspace::new();
        let parent = write_session(env.path(), "parent", "parent body");
        let mut child_writer = SessionWriter::create(env.path(), "child", "m", "p").unwrap();
        child_writer.set_derived_from(&parent);
        child_writer
            .append(&Message::user("child body".to_string()), None, None)
            .unwrap();
        child_writer.finish(SessionStatus::Completed).unwrap();
        let child = child_writer.session_id().to_string();
        assert_ne!(child, parent);

        let mut out = Vec::new();
        let export = export_session_tree(env.path(), &parent, &mut out).unwrap();
        assert_eq!(export.sessions, vec![parent.clone(), child.clone()]);

        let names = zip_names(&out);
        let child_prefix = format!("{parent}/children/{child}/");
        assert!(names.contains(&format!("{child_prefix}{TRANSCRIPT_FILE}")));
        assert!(names.contains(&format!("{child_prefix}.meta.json")));

        // Exporting the child alone does not drag the parent in.
        let mut child_out = Vec::new();
        let child_export = export_session_tree(env.path(), &child, &mut child_out).unwrap();
        assert_eq!(child_export.sessions, vec![child]);
    }

    #[test]
    fn export_of_unknown_session_is_not_found() {
        let env = IsolatedWorkspace::new();
        write_session(env.path(), "goal", "body");
        let mut out = Vec::new();
        let err = export_session_tree(env.path(), "nope", &mut out).unwrap_err();
        assert!(matches!(err, Error::NotFound(_)));
    }

    #[test]
    fn only_one_export_per_session_is_running() {
        let env = IsolatedWorkspace::new();
        let session_id = write_session(env.path(), "goal", "body");

        let guard = try_acquire_export(&session_id).expect("first export takes the lock");
        assert_eq!(guard.session_id(), session_id);
        assert!(
            try_acquire_export(&session_id).is_none(),
            "a second concurrent download must be refused"
        );
        // A different session is not blocked by this one.
        assert!(try_acquire_export("other-session").is_some());

        let mut out = Vec::new();
        let err = export_session_tree(env.path(), &session_id, &mut out).unwrap_err();
        assert!(matches!(err, Error::ExportInProgress { .. }));

        drop(guard);
        assert!(
            try_acquire_export(&session_id).is_some(),
            "the lock is released when the download finishes"
        );
    }

    #[test]
    fn a_derived_from_cycle_is_reported_not_recursed() {
        let env = IsolatedWorkspace::new();
        let a = write_session(env.path(), "a", "a body");
        let mut b_writer = SessionWriter::create(env.path(), "b", "m", "p").unwrap();
        b_writer.set_derived_from(&a);
        b_writer
            .append(&Message::user("b".to_string()), None, None)
            .unwrap();
        b_writer.finish(SessionStatus::Completed).unwrap();
        let b = b_writer.session_id().to_string();

        // Patch a's meta to point back at b, forming a cycle.
        let dirs = SessionReader::list_sessions(env.path()).unwrap();
        let a_dir = dirs
            .iter()
            .find(|d| d.file_name().unwrap().to_string_lossy() == a)
            .unwrap();
        let mut meta = SessionReader::load_meta(a_dir).unwrap();
        meta.derived_from = Some(b.clone());
        std::fs::write(a_dir.join(".meta.json"), serde_json::to_vec(&meta).unwrap()).unwrap();

        let mut out = Vec::new();
        let err = export_session_tree(env.path(), &a, &mut out).unwrap_err();
        assert!(
            matches!(err, Error::Storage { .. }),
            "cycle must be reported"
        );
    }
}
