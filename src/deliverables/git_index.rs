//! Zero-pollution git shadow index.
//!
//! The index is a *shadow*: git writes the temporary index and every new
//! object into a private directory, and reads pre-existing objects from the
//! user's object store through `GIT_ALTERNATE_OBJECT_DIRECTORIES`. The
//! user's `.git` is never written to: the real index file, the real object
//! store and the ref namespace are all out of reach of every command this
//! module runs.
//!
//! Because the shadow index stages the *working tree* (not `HEAD`), a
//! snapshot taken at the start of a turn already contains whatever the user
//! had left uncommitted. Those pre-existing edits are therefore part of the
//! baseline and can never be attributed to (or "absorbed" by) the agent's
//! turn.
//!
//! Implementation note: this module shells out to `git` via
//! `std::process::Command`, exactly like [`crate::checkpoint`], so no new
//! Cargo dependency is required.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};

/// The git-visible state of a workspace at one instant.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitSnapshot {
    /// Tree object id written by `git write-tree`, if the index was not empty.
    pub tree: Option<String>,
    /// Workspace-relative path → blob object id.
    pub paths: BTreeMap<String, String>,
}

impl GitSnapshot {
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
}

/// A private git index + object store shadowing `workspace`.
#[derive(Debug, Clone)]
pub struct ShadowIndex {
    workspace: PathBuf,
    /// Repo common dir (`<repo>/.git`), used as the alternate object store.
    common_dir: PathBuf,
    /// Path from the repository root to `workspace` (`""` or `"sub/"`).
    prefix: String,
    /// Private root holding `objects/`, `index` and `home/`.
    dir: PathBuf,
}

fn tool_error(message: impl Into<String>) -> Error {
    Error::Tool {
        name: "deliverables".into(),
        call_id: None,
        message: message.into(),
    }
}

impl ShadowIndex {
    /// Open a shadow index for `workspace`, keeping private state in `dir`.
    ///
    /// Returns an error when `git` is missing or `workspace` is not inside a
    /// git repository — callers fall back to the content-addressed walk.
    pub fn open(workspace: impl Into<PathBuf>, dir: impl Into<PathBuf>) -> Result<Self> {
        let workspace = workspace.into();
        let workspace = workspace.canonicalize().map_err(|e| {
            tool_error(format!(
                "cannot canonicalize workspace {}: {e}",
                workspace.display()
            ))
        })?;
        let dir = dir.into();
        std::fs::create_dir_all(dir.join("objects")).map_err(Error::Io)?;
        std::fs::create_dir_all(dir.join("home")).map_err(Error::Io)?;

        let out = headless_git(&workspace)
            .args(["rev-parse", "--git-common-dir", "--show-prefix"])
            .output()
            .map_err(|e| tool_error(format!("git not found or failed: {e}")))?;
        if !out.status.success() {
            return Err(tool_error(format!(
                "not a git repository: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut lines = stdout.lines();
        let common = lines.next().unwrap_or("").trim().to_string();
        let prefix = lines.next().unwrap_or("").trim().to_string();
        if common.is_empty() {
            return Err(tool_error("git rev-parse returned no common dir"));
        }
        // `--git-common-dir` is relative to the workspace when it is a
        // relative path (bare `.git` in the simple case).
        let common_dir = {
            let p = PathBuf::from(&common);
            if p.is_absolute() {
                p
            } else {
                workspace.join(p)
            }
        };

        Ok(Self {
            workspace,
            common_dir,
            prefix,
            dir,
        })
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// The private directory holding this index's `objects/` and `index`.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The user's object store, consulted read-only through the alternate.
    pub fn common_dir(&self) -> &Path {
        &self.common_dir
    }

    fn env_for<'a>(&self, cmd: &'a mut Command) -> &'a mut Command {
        let home = self.dir.join("home");
        cmd.env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &home)
            .env("GIT_CONFIG_COUNT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_OBJECT_DIRECTORY", self.dir.join("objects"))
            .env(
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                self.common_dir.join("objects"),
            )
            .env("GIT_INDEX_FILE", self.dir.join("index"))
            .current_dir(&self.workspace)
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new("git");
        self.env_for(&mut cmd);
        cmd
    }

    /// Stage the whole workspace into the shadow index and record the
    /// resulting tree.
    pub fn snapshot(&self) -> Result<GitSnapshot> {
        let add = self
            .command()
            .args(["add", "--all", "--ignore-errors", "."])
            .output()
            .map_err(|e| tool_error(format!("git add failed: {e}")))?;
        if !add.status.success() {
            return Err(tool_error(format!(
                "git add --all failed: {}",
                String::from_utf8_lossy(&add.stderr).trim()
            )));
        }

        let write = self
            .command()
            .args(["write-tree"])
            .output()
            .map_err(|e| tool_error(format!("git write-tree failed: {e}")))?;
        if !write.status.success() {
            // An empty workspace produces an empty index, which `write-tree`
            // refuses. That is a legitimate (empty) snapshot, not a failure.
            return Ok(GitSnapshot::default());
        }
        let tree = String::from_utf8_lossy(&write.stdout).trim().to_string();
        if tree.is_empty() {
            return Ok(GitSnapshot::default());
        }

        let list = self
            .command()
            .args(["ls-tree", "-r", "--full-tree", &tree])
            .output()
            .map_err(|e| tool_error(format!("git ls-tree failed: {e}")))?;
        if !list.status.success() {
            return Err(tool_error(format!(
                "git ls-tree failed: {}",
                String::from_utf8_lossy(&list.stderr).trim()
            )));
        }

        let mut paths = BTreeMap::new();
        for line in String::from_utf8_lossy(&list.stdout).lines() {
            let Some((meta, raw_path)) = line.split_once('\t') else {
                continue;
            };
            let mut fields = meta.split_whitespace();
            let _mode = fields.next();
            let kind = fields.next();
            let oid = fields.next();
            if kind != Some("blob") {
                continue;
            }
            let (Some(oid), Some(rel)) = (oid, raw_path.strip_prefix(self.prefix.as_str())) else {
                continue;
            };
            paths.insert(rel.to_string(), oid.to_string());
        }
        Ok(GitSnapshot {
            tree: Some(tree),
            paths,
        })
    }

    /// Read a blob recorded by an earlier snapshot.
    pub fn read_blob(&self, oid: &str) -> Result<Vec<u8>> {
        let out = self
            .command()
            .args(["cat-file", "-p", oid])
            .output()
            .map_err(|e| tool_error(format!("git cat-file failed: {e}")))?;
        if !out.status.success() {
            return Err(tool_error(format!(
                "git cat-file {oid} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(out.stdout)
    }
}

/// Run `git` in `workspace` with a scrubbed environment (no repository
/// redirection). Used for repository discovery, which must happen *before*
/// the object-directory override can be applied.
fn headless_git(workspace: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("GIT_CONFIG_COUNT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(workspace);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_COUNT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git must be installed for these tests");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init", "-q"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "user.name", "test"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
    }

    fn commit_all(dir: &Path, msg: &str) {
        git(dir, &["add", "--all"]);
        git(dir, &["commit", "-q", "-m", msg, "--no-gpg-sign"]);
    }

    fn status(dir: &Path) -> String {
        git(dir, &["status", "--porcelain"])
    }

    #[test]
    fn snapshot_lists_tracked_and_untracked_files() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        init_repo(&ws);
        std::fs::write(ws.join("tracked.txt"), "tracked\n").unwrap();
        commit_all(&ws, "init");
        std::fs::write(ws.join("untracked.txt"), "new\n").unwrap();

        let index = ShadowIndex::open(&ws, tmp.path().join("shadow")).unwrap();
        let snap = index.snapshot().unwrap();
        assert!(snap.tree.is_some());
        assert!(snap.paths.contains_key("tracked.txt"));
        assert!(snap.paths.contains_key("untracked.txt"));
    }

    #[test]
    fn snapshot_never_touches_the_user_repository() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        init_repo(&ws);
        std::fs::write(ws.join("a.txt"), "one\n").unwrap();
        commit_all(&ws, "init");
        std::fs::write(ws.join("a.txt"), "one\ntwo\n").unwrap();

        let before_status = status(&ws);
        let before_index = std::fs::read(ws.join(".git/index")).unwrap();
        let before_log = git(&ws, &["rev-parse", "HEAD"]);

        let index = ShadowIndex::open(&ws, tmp.path().join("shadow")).unwrap();
        index.snapshot().unwrap();
        index.snapshot().unwrap();

        assert_eq!(status(&ws), before_status, "git status must not change");
        assert_eq!(
            std::fs::read(ws.join(".git/index")).unwrap(),
            before_index,
            "the user's index file must not be rewritten"
        );
        assert_eq!(git(&ws, &["rev-parse", "HEAD"]), before_log);
        // The shadow's own index and objects live outside the repository.
        let shadow = tmp.path().join("shadow");
        assert!(shadow.join("index").exists());
        assert!(std::fs::read_dir(shadow.join("objects")).unwrap().count() > 0);
        assert!(
            !ws.join(".git/index.lock").exists(),
            "the shadow index must not leave a lock in the user's repo"
        );
    }

    #[test]
    fn uncommitted_edits_are_part_of_the_baseline() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        init_repo(&ws);
        std::fs::write(ws.join("a.txt"), "committed\n").unwrap();
        commit_all(&ws, "init");

        let index = ShadowIndex::open(&ws, tmp.path().join("shadow")).unwrap();
        let before = index.snapshot().unwrap();
        let committed_oid = before.paths["a.txt"].clone();

        // A user edits without committing: the shadow sees the edit, so it
        // belongs to the baseline rather than to the next agent turn.
        std::fs::write(ws.join("a.txt"), "committed\nuser-edit\n").unwrap();
        let after = index.snapshot().unwrap();
        assert_ne!(after.paths["a.txt"], committed_oid);
        let blob = index.read_blob(&after.paths["a.txt"]).unwrap();
        assert_eq!(String::from_utf8_lossy(&blob), "committed\nuser-edit\n");
        // Both versions remain readable — a turn diff can render either side.
        assert_eq!(
            String::from_utf8_lossy(&index.read_blob(&committed_oid).unwrap()),
            "committed\n"
        );
    }

    #[test]
    fn ignored_files_stay_out_of_the_index() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        init_repo(&ws);
        std::fs::write(ws.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(ws.join("ignored.txt"), "nope\n").unwrap();
        std::fs::write(ws.join("kept.txt"), "yes\n").unwrap();

        let index = ShadowIndex::open(&ws, tmp.path().join("shadow")).unwrap();
        let snap = index.snapshot().unwrap();
        assert!(snap.paths.contains_key("kept.txt"));
        assert!(!snap.paths.contains_key("ignored.txt"));
    }

    #[test]
    fn subdirectory_workspace_paths_are_workspace_relative() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let ws = repo.join("sub");
        std::fs::create_dir_all(&ws).unwrap();
        init_repo(&repo);
        std::fs::write(repo.join("root.txt"), "root\n").unwrap();
        std::fs::write(ws.join("inner.txt"), "inner\n").unwrap();
        commit_all(&repo, "init");

        let index = ShadowIndex::open(&ws, tmp.path().join("shadow")).unwrap();
        let snap = index.snapshot().unwrap();
        assert!(snap.paths.contains_key("inner.txt"), "{:?}", snap.paths);
        assert!(
            !snap.paths.contains_key("root.txt"),
            "files outside the workspace must not be indexed: {:?}",
            snap.paths
        );
    }

    #[test]
    fn non_repository_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let err = ShadowIndex::open(tmp.path(), tmp.path().join("shadow")).unwrap_err();
        assert!(
            err.to_string().contains("not a git repository"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn empty_workspace_yields_an_empty_snapshot() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        init_repo(&ws);
        let index = ShadowIndex::open(&ws, tmp.path().join("shadow")).unwrap();
        let snap = index.snapshot().unwrap();
        assert!(snap.is_empty());
        assert_eq!(
            snap.paths.len(),
            0,
            "an empty workspace must index no files"
        );
    }
}
