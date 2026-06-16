//! Read-only and save checkpoint tools that surface the session's
//! checkpoint chain.
//!
//! `checkpoint_list` and `checkpoint_diff` are read-only observer tools.
//! `checkpoint_save` (Goal 284) lets the agent create explicit restore
//! points on demand — the only path that creates new shadow-git objects
//! during a run.
//!
//! Restoration remains runtime/CLI-driven (see `recursive sessions rewind`).

use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::Tool;
use crate::checkpoint::{CheckpointId, ShadowRepo};
use crate::checkpoint_log::{read_log, CheckpointLogWriter, CheckpointRecord, TouchedVia};
use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::tools::TouchedFiles;

/// Shared state for the read-only checkpoint tools.
///
/// `session_id` is fixed at registration time so the agent can only
/// inspect its own checkpoint chain — never another session's.
#[derive(Clone)]
pub struct CheckpointToolCtx {
    pub repo: Arc<Mutex<ShadowRepo>>,
    pub session_id: String,
}

// ── checkpoint_list ───────────────────────────────────────────────────────────

pub struct CheckpointList {
    ctx: CheckpointToolCtx,
}

impl CheckpointList {
    pub fn new(ctx: CheckpointToolCtx) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for CheckpointList {
    fn is_deferred(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "checkpoint_list".into(),
            description: "List checkpoints for the current session, newest first. \
                 Checkpoints are saved by the agent via `checkpoint_save` or \
                 by the runtime at session boundaries. Use `checkpoint_diff` \
                 to inspect the changes captured in a checkpoint."
                .into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    fn side_effect_class(&self) -> crate::tools::ToolSideEffect {
        crate::tools::ToolSideEffect::ReadOnly
    }

    async fn execute(&self, _args: Value) -> Result<String> {
        let infos = {
            let repo = self.ctx.repo.lock().map_err(|_| Error::Tool {
                name: "checkpoint_list".into(),
                message: "lock poisoned".into(),
            })?;
            repo.list_for_session(&self.ctx.session_id)?
        };
        if infos.is_empty() {
            return Ok("no checkpoints in this session yet".to_string());
        }
        let mut out = String::new();
        for i in &infos {
            out.push_str(&format!(
                "[{}]  {}  ({} file(s) changed)  ts={}\n",
                i.id, i.message, i.files_changed, i.timestamp
            ));
        }
        Ok(out.trim_end().to_string())
    }
}

// ── checkpoint_diff ───────────────────────────────────────────────────────────

pub struct CheckpointDiff {
    ctx: CheckpointToolCtx,
}

impl CheckpointDiff {
    pub fn new(ctx: CheckpointToolCtx) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for CheckpointDiff {
    fn is_deferred(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "checkpoint_diff".into(),
            description: "Show a unified diff between two checkpoints in this session. \
                 If `b` is omitted, diffs `a` against the current workspace state."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "a": {"type": "string", "description": "Older checkpoint id."},
                    "b": {"type": "string", "description": "Optional newer checkpoint id; omit for current workspace."}
                },
                "required": ["a"]
            }),
        }
    }

    fn side_effect_class(&self) -> crate::tools::ToolSideEffect {
        crate::tools::ToolSideEffect::ReadOnly
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let a = args["a"].as_str().ok_or_else(|| Error::BadToolArgs {
            name: "checkpoint_diff".into(),
            message: "missing `a`".into(),
        })?;
        let b = args["b"].as_str();
        let aid = CheckpointId(a.to_string());
        let bid = b.map(|s| CheckpointId(s.to_string()));
        let diff = {
            let repo = self.ctx.repo.lock().map_err(|_| Error::Tool {
                name: "checkpoint_diff".into(),
                message: "lock poisoned".into(),
            })?;
            repo.diff(&aid, bid.as_ref(), &[])?
        };
        if diff.trim().is_empty() {
            Ok("no differences".to_string())
        } else {
            Ok(diff)
        }
    }
}

// ── checkpoint_save (Goal 284) ────────────────────────────────────────────────

/// Context for the on-demand `checkpoint_save` tool.
///
/// Unlike the read-only tools, this holds the touched-files collector,
/// log writer, turn index, and log path so it can snapshot the workspace,
/// attribute file changes, and append a [`CheckpointRecord`].
#[derive(Clone)]
pub struct CheckpointSaveCtx {
    pub repo: Arc<Mutex<ShadowRepo>>,
    pub session_id: String,
    pub touched_files: Option<Arc<Mutex<TouchedFiles>>>,
    pub writer: Arc<Mutex<CheckpointLogWriter>>,
    pub turn_index: Arc<std::sync::atomic::AtomicUsize>,
    pub log_path: PathBuf,
}

pub struct CheckpointSave {
    ctx: CheckpointSaveCtx,
}

impl CheckpointSave {
    pub fn new(ctx: CheckpointSaveCtx) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for CheckpointSave {
    fn is_deferred(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "checkpoint_save".into(),
            description: "Save an explicit restore point for the current session. \
                 Call this before making a risky batch of changes, or after \
                 completing a logical unit of work you might want to revert \
                 to. Unlike automatic checkpoints (which no longer exist), \
                 this runs only when you call it."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "A short label for this checkpoint, e.g. 'before refactor' or 'after adding tests'. Defaults to the current turn number if omitted."
                    }
                }
            }),
        }
    }

    fn side_effect_class(&self) -> crate::tools::ToolSideEffect {
        crate::tools::ToolSideEffect::ReadOnly
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let turn = self
            .ctx
            .turn_index
            .load(std::sync::atomic::Ordering::Relaxed);
        let message = args
            .get("message")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("turn {turn}"));

        // Collect touched files before snapshotting.
        let (mut paths, saw_shell) = match &self.ctx.touched_files {
            Some(slot) => match slot.lock() {
                Ok(t) => (t.paths_sorted(), t.saw_shell),
                Err(_) => (vec![], false),
            },
            None => (vec![], true),
        };

        // Find the last checkpoint id for shell-diff attribution.
        let last_id: Option<CheckpointId> = {
            match read_log(&self.ctx.log_path) {
                Ok(recs) => recs.last().map(|r| r.id.clone()),
                Err(_) => None,
            }
        };

        // Snapshot the workspace.
        let id = {
            let repo = self.ctx.repo.lock().map_err(|_| Error::Tool {
                name: "checkpoint_save".into(),
                message: "lock poisoned".into(),
            })?;
            repo.snapshot_for_session(&self.ctx.session_id, &message)?
        };

        // If the agent ran a shell command this turn, diff against the
        // last checkpoint to capture files created/modified.
        let mut via = TouchedVia::Structured;
        if saw_shell {
            via = TouchedVia::ShellDiff;
            if let Some(ref last) = last_id {
                let repo = self.ctx.repo.lock().map_err(|_| Error::Tool {
                    name: "checkpoint_save".into(),
                    message: "lock poisoned".into(),
                })?;
                if let Ok(diff_paths) = repo.changed_paths(last, &id) {
                    let mut set: std::collections::HashSet<String> = paths.drain(..).collect();
                    for p in diff_paths {
                        set.insert(p);
                    }
                    paths = {
                        let mut v: Vec<String> = set.into_iter().collect();
                        v.sort();
                        v
                    };
                }
            }
        }

        // Write the log record.
        {
            let writer = self.ctx.writer.lock().map_err(|_| Error::Tool {
                name: "checkpoint_save".into(),
                message: "writer lock poisoned".into(),
            })?;
            let rec = CheckpointRecord {
                turn,
                pre: last_id,
                id: id.clone(),
                message: Some(message),
                touched_files: paths,
                touched_via: via,
                started_at: 0,
                finished_at: 0,
                saved_at: unix_now(),
            };
            writer.append(&rec).map_err(|e| Error::Tool {
                name: "checkpoint_save".into(),
                message: format!("failed to append checkpoint record: {e}"),
            })?;
        }

        Ok(id.0)
    }
}

/// Build a [`CheckpointSave`] tool wired to the given shared state.
pub fn build_checkpoint_save_tool(
    repo: Arc<Mutex<ShadowRepo>>,
    session_id: String,
    touched_files: Option<Arc<Mutex<TouchedFiles>>>,
    writer: Arc<Mutex<CheckpointLogWriter>>,
    turn_index: Arc<std::sync::atomic::AtomicUsize>,
    log_path: PathBuf,
) -> CheckpointSave {
    CheckpointSave::new(CheckpointSaveCtx {
        repo,
        session_id,
        touched_files,
        writer,
        turn_index,
        log_path,
    })
}

/// Current Unix timestamp in seconds.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ── helper ────────────────────────────────────────────────────────────────────

/// Build the read-only checkpoint tools for a given session.
/// Returns `None` if the shadow repo cannot be opened (e.g. git missing).
pub fn build_checkpoint_tools(
    workspace: impl Into<std::path::PathBuf>,
    session_id: impl Into<String>,
) -> Option<(CheckpointList, CheckpointDiff, Arc<Mutex<ShadowRepo>>)> {
    let repo = match ShadowRepo::open(workspace) {
        Ok(r) => Arc::new(Mutex::new(r)),
        Err(e) => {
            tracing::warn!("checkpoint tools unavailable: {e}");
            return None;
        }
    };
    build_checkpoint_tools_inner(repo, session_id.into())
}

/// Like [`build_checkpoint_tools`], but with an explicit `shadow_dir`,
/// for tests that want to bypass `paths::user_data_dir()` resolution.
pub fn build_checkpoint_tools_at(
    workspace: impl Into<std::path::PathBuf>,
    shadow_dir: impl Into<std::path::PathBuf>,
    session_id: impl Into<String>,
) -> Option<(CheckpointList, CheckpointDiff, Arc<Mutex<ShadowRepo>>)> {
    let repo = match ShadowRepo::open_at(workspace, shadow_dir) {
        Ok(r) => Arc::new(Mutex::new(r)),
        Err(e) => {
            tracing::warn!("checkpoint tools unavailable: {e}");
            return None;
        }
    };
    build_checkpoint_tools_inner(repo, session_id.into())
}

fn build_checkpoint_tools_inner(
    repo: Arc<Mutex<ShadowRepo>>,
    session_id: String,
) -> Option<(CheckpointList, CheckpointDiff, Arc<Mutex<ShadowRepo>>)> {
    let ctx = CheckpointToolCtx {
        repo: Arc::clone(&repo),
        session_id,
    };
    Some((
        CheckpointList::new(ctx.clone()),
        CheckpointDiff::new(ctx),
        repo,
    ))
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    fn has_git() -> bool {
        Command::new("git").arg("--version").output().is_ok()
    }

    /// Test workspace bundle: workspace tempdir + sibling shadow tempdir.
    /// Tests use `build_checkpoint_tools_at` to bypass `paths::user_data_dir()`,
    /// avoiding any need to mutate `RECURSIVE_HOME` (and thus the global
    /// env lock). This lets the suite run fully in parallel.
    struct TestWs {
        workspace: TempDir,
        shadow: TempDir,
    }

    impl TestWs {
        fn path(&self) -> &std::path::Path {
            self.workspace.path()
        }
        fn shadow_dir(&self) -> std::path::PathBuf {
            self.shadow.path().join("shadow-git")
        }
    }

    fn ws() -> TestWs {
        TestWs {
            workspace: tempfile::tempdir().expect("workspace tempdir"),
            shadow: tempfile::tempdir().expect("shadow tempdir"),
        }
    }

    #[tokio::test]
    async fn list_tool_shows_session_checkpoints() {
        if !has_git() {
            return;
        }
        let w = ws();
        fs::write(w.path().join("a.txt"), "hi").unwrap();
        let (list, _, repo) = build_checkpoint_tools_at(w.path(), w.shadow_dir(), "alpha").unwrap();
        repo.lock()
            .unwrap()
            .snapshot_for_session("alpha", "t0")
            .unwrap();
        let out = list.execute(json!({})).await.unwrap();
        assert!(out.contains("t0"));
    }

    #[tokio::test]
    async fn list_tool_only_shows_own_session() {
        if !has_git() {
            return;
        }
        let w = ws();
        fs::write(w.path().join("a.txt"), "hi").unwrap();
        let (list_a, _, repo) =
            build_checkpoint_tools_at(w.path(), w.shadow_dir(), "alpha").unwrap();
        repo.lock()
            .unwrap()
            .snapshot_for_session("alpha", "tA")
            .unwrap();
        repo.lock()
            .unwrap()
            .snapshot_for_session("beta", "tB")
            .unwrap();
        let out_a = list_a.execute(json!({})).await.unwrap();
        assert!(out_a.contains("tA"));
        assert!(
            !out_a.contains("tB"),
            "alpha must not see beta's checkpoints"
        );
    }

    #[tokio::test]
    async fn diff_tool_returns_empty_for_no_change() {
        if !has_git() {
            return;
        }
        let w = ws();
        fs::write(w.path().join("a.txt"), "x").unwrap();
        let (_, diff, repo) = build_checkpoint_tools_at(w.path(), w.shadow_dir(), "s").unwrap();
        let id = repo
            .lock()
            .unwrap()
            .snapshot_for_session("s", "snap")
            .unwrap();
        let out = diff.execute(json!({"a": id.0})).await.unwrap();
        assert_eq!(out, "no differences");
    }

    /// Goal 284: verify that `checkpoint_save` creates a shadow-git
    /// commit and a `checkpoints.jsonl` entry.
    #[tokio::test]
    async fn checkpoint_save_tool_creates_entry() {
        if !has_git() {
            return;
        }
        let w = ws();
        let log_path = w.path().join("checkpoints.jsonl");
        fs::write(w.path().join("a.txt"), "hello").unwrap();

        let repo = Arc::new(Mutex::new(
            ShadowRepo::open_at(w.path(), w.shadow_dir()).unwrap(),
        ));
        let touched = Arc::new(Mutex::new(TouchedFiles::new()));
        {
            let mut t = touched.lock().unwrap();
            t.paths.insert("a.txt".to_string());
        }
        let writer = Arc::new(Mutex::new(CheckpointLogWriter::open(&log_path).unwrap()));
        let turn_index = Arc::new(std::sync::atomic::AtomicUsize::new(3));

        let tool = build_checkpoint_save_tool(
            repo.clone(),
            "sess".into(),
            Some(touched.clone()),
            writer.clone(),
            turn_index.clone(),
            log_path.clone(),
        );

        let id_str = tool
            .execute(json!({"message": "my save point"}))
            .await
            .unwrap();
        assert!(!id_str.is_empty(), "should return a checkpoint id");

        // Verify the checkpoint is in the shadow repo.
        let cid = CheckpointId(id_str.clone());
        let data = repo.lock().unwrap().read_file_at(&cid, "a.txt").unwrap();
        assert_eq!(data, Some(b"hello".to_vec()));

        // Verify the log entry.
        let recs = read_log(&log_path).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].turn, 3);
        assert_eq!(recs[0].id.0, id_str);
        assert_eq!(recs[0].message.as_deref(), Some("my save point"));
        assert_eq!(recs[0].touched_files, vec!["a.txt".to_string()]);
        assert_eq!(recs[0].touched_via, TouchedVia::Structured);
    }
}
