//! Read-only agent tools that surface the session's checkpoint chain.
//!
//! Only `checkpoint_list` and `checkpoint_diff` are exposed to agents.
//! Snapshot creation and restoration are runtime-driven (see
//! `AgentRuntime` and the `recursive sessions rewind` CLI), not
//! agent-driven.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

use super::Tool;
use crate::checkpoint::{CheckpointId, ShadowRepo};
use crate::error::{Error, Result};
use crate::llm::ToolSpec;

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
            description:
                "List checkpoints (turn-level workspace snapshots) for the current session, \
                 newest first. Snapshots are created automatically by the runtime around each \
                 turn — agents do not create or restore them; that's a CLI/runtime operation."
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
}
