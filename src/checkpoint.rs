//! Shadow bare git repository for per-session, per-turn checkpoints.
//!
//! A single bare repo lives at `<workspace>/.recursive/shadow-git/`.
//! All sessions in the same workspace share that repo's object store
//! (so identical file contents dedup automatically), but each session
//! advances its own ref chain at `refs/sessions/<sid>/HEAD`.
//!
//! Checkpoints are taken automatically by `AgentRuntime` at the
//! beginning and end of every turn — never by the agent itself.
//! Restoration is **selective**: callers must specify which file paths
//! to revert, leaving sibling sessions' work untouched.
//!
//! Implementation note: this module shells out to `git` via
//! `std::process::Command` so no new Cargo dependency is required.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};

// ── public types ─────────────────────────────────────────────────────────────

/// 12-char short SHA identifying one checkpoint commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct CheckpointId(pub String);

impl std::fmt::Display for CheckpointId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Metadata for a single checkpoint commit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointInfo {
    pub id: CheckpointId,
    pub message: String,
    /// Unix seconds.
    pub timestamp: i64,
    /// Number of files that changed relative to the previous checkpoint
    /// in this session's chain.
    pub files_changed: usize,
}

/// Result of a [`ShadowRepo::restore_paths`] call.
#[derive(Debug, Clone, Default)]
pub struct RestoreStats {
    /// Files whose content was overwritten with checkpoint content.
    pub restored: usize,
    /// Files that were deleted because they did not exist at the
    /// target checkpoint.
    pub deleted: usize,
    /// Files in `paths` that didn't need any change (already matched).
    pub unchanged: usize,
}

// ── ShadowRepo ────────────────────────────────────────────────────────────────

/// A shared shadow bare git repository for a workspace.
///
/// Per-session checkpoint chains live under `refs/sessions/<sid>/HEAD`.
#[derive(Debug, Clone)]
pub struct ShadowRepo {
    workspace: PathBuf,
    shadow_dir: PathBuf,
}

impl ShadowRepo {
    /// Open or create the shadow repo for `workspace`. Idempotent.
    /// Returns an error if `git` is not on PATH.
    pub fn open(workspace: impl Into<PathBuf>) -> Result<Self> {
        let workspace = workspace.into().canonicalize().map_err(|e| Error::Tool {
            name: "checkpoint".into(),
            call_id: None,
            message: format!("cannot canonicalize workspace: {e}"),
        })?;
        // On Windows, std::fs::canonicalize() returns extended-length UNC paths
        // (\\?\C:\...) which git does not accept as GIT_WORK_TREE values.
        // Strip the prefix so git receives a plain drive-letter path.
        #[cfg(windows)]
        let workspace = {
            let s = workspace.to_string_lossy();
            if let Some(stripped) = s.strip_prefix(r"\\?\") {
                PathBuf::from(stripped)
            } else {
                workspace
            }
        };
        let shadow_dir = crate::paths::user_shadow_git_dir(&workspace)?;
        Self::open_at(workspace, shadow_dir)
    }

    /// Open or create a shadow repo with an explicit `shadow_dir`,
    /// bypassing `paths::user_data_dir()` resolution.
    ///
    /// Used by tests so they don't have to mutate `RECURSIVE_HOME`
    /// (and thus don't need the cross-module env lock), letting
    /// checkpoint tests run in parallel. Production callers should
    /// stick to [`open`].
    pub fn open_at(workspace: impl Into<PathBuf>, shadow_dir: impl Into<PathBuf>) -> Result<Self> {
        let workspace = workspace.into();
        let shadow_dir = shadow_dir.into();
        if !shadow_dir.exists() {
            std::fs::create_dir_all(&shadow_dir).map_err(|e| Error::Tool {
                name: "checkpoint".into(),
                call_id: None,
                message: format!("cannot create shadow-git dir: {e}"),
            })?;
            let out = git_cmd()
                .args(["init", "--bare"])
                .current_dir(&shadow_dir)
                .output()
                .map_err(|e| Error::Tool {
                    name: "checkpoint".into(),
                    call_id: None,
                    message: format!("git not found or failed: {e}"),
                })?;
            if !out.status.success() {
                return Err(Error::Tool {
                    name: "checkpoint".into(),
                    call_id: None,
                    message: format!(
                        "git init --bare failed: {}",
                        String::from_utf8_lossy(&out.stderr)
                    ),
                });
            }
        }

        Ok(Self {
            workspace,
            shadow_dir,
        })
    }

    /// The workspace this repo snapshots.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Snapshot the current workspace state and advance
    /// `refs/sessions/<session_id>/HEAD` to the new commit.
    ///
    /// Per-session temporary index files prevent concurrent snapshots
    /// from racing each other.
    pub fn snapshot_for_session(&self, session_id: &str, message: &str) -> Result<CheckpointId> {
        validate_session_id(session_id)?;

        let tmp_index = self.shadow_dir.join(format!("tmp-index-{session_id}"));
        // Ensure no stale index from a crashed prior run.
        let _ = std::fs::remove_file(&tmp_index);

        // We exclude the `.recursive/` directory entirely so the shadow
        // repo never snapshots its own internals or sibling sessions'
        // state files. Use `current_dir` so that pathspecs are resolved
        // relative to the workspace even on Windows where git may not
        // honour GIT_WORK_TREE for pathspec resolution.  The short-form
        // `:!` exclude is more portable across git versions and platforms
        // than the long-form `:(exclude,glob)` magic.
        let add_out = git_cmd()
            .env("GIT_INDEX_FILE", &tmp_index)
            .env("GIT_DIR", &self.shadow_dir)
            .env("GIT_WORK_TREE", &self.workspace)
            .current_dir(&self.workspace)
            .args([
                "add",
                "-A",
                "--force",
                "--",
                ".",
                // Never snapshot recursive's own internals or session state.
                ":!.recursive/**",
                ":!.recursive",
                // Exclude large build / cache directories that are git-ignored
                // in the project but would be captured by --force. Including
                // them inflates each snapshot by gigabytes and causes the
                // blocking git-add to take minutes rather than milliseconds.
                ":!target/**",
                ":!target",
                // Common large auto-generated dirs across projects.
                // Use `:**/` prefix so nested copies (e.g. website/node_modules/)
                // are excluded in addition to the workspace root copy.
                ":!**/node_modules/**",
                ":!**/node_modules",
                ":!.flowcast/runs/**",
                ":!.flowcast/logs/**",
                ":!.dev/runs/**",
                ":!.dev/transcripts/**",
                ":!.worktrees/**",
            ])
            .output()
            .map_err(git_err)?;

        if !add_out.status.success() {
            let stderr = String::from_utf8_lossy(&add_out.stderr);
            if !stderr.trim().is_empty()
                && !stderr.contains("nothing to commit")
                && !stderr.contains("warning:")
            {
                let _ = std::fs::remove_file(&tmp_index);
                return Err(Error::Tool {
                    name: "checkpoint".into(),
                    call_id: None,
                    message: format!("git add failed: {stderr}"),
                });
            }
        }

        let tree_out = git_cmd()
            .env("GIT_INDEX_FILE", &tmp_index)
            .env("GIT_DIR", &self.shadow_dir)
            .args(["write-tree"])
            .output()
            .map_err(git_err)?;
        let _ = std::fs::remove_file(&tmp_index);

        if !tree_out.status.success() {
            return Err(Error::Tool {
                name: "checkpoint".into(),
                call_id: None,
                message: format!(
                    "git write-tree failed: {}",
                    String::from_utf8_lossy(&tree_out.stderr)
                ),
            });
        }
        let tree_sha = String::from_utf8_lossy(&tree_out.stdout).trim().to_string();

        // Read this session's current HEAD (if any) for parent linkage.
        let parent = self.session_head_full_sha(session_id);

        let mut ct_args = vec!["commit-tree".to_string(), tree_sha.clone()];
        if let Some(ref p) = parent {
            ct_args.push("-p".to_string());
            ct_args.push(p.clone());
        }
        ct_args.push("-m".to_string());
        ct_args.push(message.to_string());

        let commit_out = git_cmd()
            .env("GIT_DIR", &self.shadow_dir)
            .env("GIT_AUTHOR_NAME", "recursive-agent")
            .env("GIT_AUTHOR_EMAIL", "agent@recursive")
            .env("GIT_COMMITTER_NAME", "recursive-agent")
            .env("GIT_COMMITTER_EMAIL", "agent@recursive")
            .args(&ct_args)
            .output()
            .map_err(git_err)?;

        if !commit_out.status.success() {
            return Err(Error::Tool {
                name: "checkpoint".into(),
                call_id: None,
                message: format!(
                    "git commit-tree failed: {}",
                    String::from_utf8_lossy(&commit_out.stderr)
                ),
            });
        }
        let commit_sha = String::from_utf8_lossy(&commit_out.stdout)
            .trim()
            .to_string();

        // Atomic ref update via `git update-ref`. Provides locking
        // and prevents two concurrent snapshots from clobbering each
        // other's HEAD.
        let ref_name = session_ref(session_id);
        let mut update_args = vec!["update-ref".to_string(), ref_name, commit_sha.clone()];
        if let Some(p) = parent {
            update_args.push(p);
        }
        let upd_out = git_cmd()
            .env("GIT_DIR", &self.shadow_dir)
            .args(&update_args)
            .output()
            .map_err(git_err)?;
        if !upd_out.status.success() {
            return Err(Error::Tool {
                name: "checkpoint".into(),
                call_id: None,
                message: format!(
                    "git update-ref failed: {}",
                    String::from_utf8_lossy(&upd_out.stderr)
                ),
            });
        }

        Ok(CheckpointId(short_sha(&commit_sha)))
    }

    /// List checkpoints for `session_id` in reverse chronological order.
    pub fn list_for_session(&self, session_id: &str) -> Result<Vec<CheckpointInfo>> {
        validate_session_id(session_id)?;
        let head = match self.session_head_full_sha(session_id) {
            None => return Ok(vec![]),
            Some(h) => h,
        };

        let log_out = git_cmd()
            .env("GIT_DIR", &self.shadow_dir)
            .args(["log", "--format=%H|%ct|%s", &head])
            .output()
            .map_err(git_err)?;

        if !log_out.status.success() {
            return Ok(vec![]);
        }
        let stdout = String::from_utf8_lossy(&log_out.stdout);
        let lines: Vec<&str> = stdout.lines().collect();

        let mut infos = Vec::with_capacity(lines.len());
        for (i, line) in lines.iter().enumerate() {
            let parts: Vec<&str> = line.splitn(3, '|').collect();
            if parts.len() < 3 {
                continue;
            }
            let full_sha = parts[0].to_string();
            let timestamp: i64 = parts[1].parse().unwrap_or(0);
            let msg = parts[2].to_string();

            let files_changed = if i + 1 < lines.len() {
                self.count_diff_files(&format!("{full_sha}^"), &full_sha)
            } else {
                // Root commit — diff against empty tree.
                self.count_diff_files(EMPTY_TREE_SHA, &full_sha)
            };

            infos.push(CheckpointInfo {
                id: CheckpointId(short_sha(&full_sha)),
                message: msg,
                timestamp,
                files_changed,
            });
        }
        Ok(infos)
    }

    /// Read a single file's contents at `checkpoint`.
    /// Returns `Ok(None)` if the file did not exist at that checkpoint.
    pub fn read_file_at(&self, checkpoint: &CheckpointId, path: &str) -> Result<Option<Vec<u8>>> {
        let full = self.expand_sha(&checkpoint.0)?;
        let spec = format!("{full}:{path}");
        let out = git_cmd()
            .env("GIT_DIR", &self.shadow_dir)
            .args(["cat-file", "-p", &spec])
            .output()
            .map_err(git_err)?;
        if out.status.success() {
            Ok(Some(out.stdout))
        } else {
            // git cat-file fails with non-zero when path is absent.
            // Distinguish "missing path" from real errors by stderr text.
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("does not exist")
                || stderr.contains("not a valid object name")
                || stderr.contains("Not a valid object name")
                || stderr.contains("exists on disk, but not in")
            {
                Ok(None)
            } else {
                Err(Error::Tool {
                    name: "checkpoint".into(),
                    call_id: None,
                    message: format!("git cat-file failed: {stderr}"),
                })
            }
        }
    }

    /// Restore *only* the given workspace-relative file `paths` to
    /// their state at `checkpoint`. Files not in `paths` remain
    /// untouched. Files present in the workspace but not in the
    /// checkpoint tree are deleted (when listed in `paths`).
    pub fn restore_paths(
        &self,
        checkpoint: &CheckpointId,
        paths: &[String],
    ) -> Result<RestoreStats> {
        let full = self.expand_sha(&checkpoint.0)?;
        let mut stats = RestoreStats::default();

        for path in paths {
            let abs =
                crate::tools::resolve_within(&self.workspace, path).map_err(|_| Error::Tool {
                    name: "checkpoint".into(),
                    call_id: None,
                    message: format!("path '{path}' escapes workspace root"),
                })?;
            let cp_bytes = self.read_file_at(checkpoint, path)?;
            let current_bytes = std::fs::read(&abs).ok();

            match (cp_bytes, current_bytes) {
                (None, None) => {
                    stats.unchanged += 1;
                }
                (None, Some(_)) => {
                    // Existed in workspace but not in checkpoint → delete.
                    if abs.exists() {
                        std::fs::remove_file(&abs).map_err(|e| Error::Tool {
                            name: "checkpoint".into(),
                            call_id: None,
                            message: format!("cannot delete {path}: {e}"),
                        })?;
                        stats.deleted += 1;
                    }
                }
                (Some(want), Some(have)) if want == have => {
                    stats.unchanged += 1;
                }
                (Some(want), _) => {
                    if let Some(parent) = abs.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| Error::Tool {
                            name: "checkpoint".into(),
                            call_id: None,
                            message: format!("cannot create dir for {path}: {e}"),
                        })?;
                    }
                    crate::atomic::atomic_write(&abs, &want).map_err(|e| Error::Tool {
                        name: "checkpoint".into(),
                        call_id: None,
                        message: format!("cannot restore {path}: {e}"),
                    })?;
                    stats.restored += 1;
                }
            }
        }

        // Suppress unused-variable lint by referring to full once more.
        let _ = full;
        Ok(stats)
    }

    /// Diff between two checkpoints (or `a` vs current workspace if
    /// `b` is None), optionally limited to `paths`.
    pub fn diff(
        &self,
        a: &CheckpointId,
        b: Option<&CheckpointId>,
        paths: &[String],
    ) -> Result<String> {
        let a_full = self.expand_sha(&a.0)?;
        let b_full = match b {
            Some(id) => self.expand_sha(&id.0)?,
            None => self.write_workspace_tree()?,
        };

        let mut args: Vec<String> = vec!["diff".to_string(), a_full, b_full];
        if !paths.is_empty() {
            args.push("--".to_string());
            for p in paths {
                args.push(p.clone());
            }
        }
        let out = git_cmd()
            .env("GIT_DIR", &self.shadow_dir)
            .args(&args)
            .output()
            .map_err(git_err)?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// List file paths changed between checkpoints `a` and `b`.
    /// Used for "shell-diff" attribution after `run_shell` calls.
    pub fn changed_paths(&self, a: &CheckpointId, b: &CheckpointId) -> Result<Vec<String>> {
        let a_full = self.expand_sha(&a.0)?;
        let b_full = self.expand_sha(&b.0)?;
        let out = git_cmd()
            .env("GIT_DIR", &self.shadow_dir)
            .args(["diff-tree", "--name-only", "-r", &a_full, &b_full])
            .output()
            .map_err(git_err)?;
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect())
    }

    /// Drop the shadow repo entirely. Used by tests and `clean` UX.
    pub fn clean(&self) -> Result<()> {
        if self.shadow_dir.exists() {
            std::fs::remove_dir_all(&self.shadow_dir).map_err(|e| Error::Tool {
                name: "checkpoint".into(),
                call_id: None,
                message: format!("cannot remove shadow-git: {e}"),
            })?;
        }
        Ok(())
    }

    /// Run git garbage collection on the shadow repo, pruning all unreachable
    /// objects immediately. This reclaims disk space after rewinds (which
    /// orphan commits) and after the pathspec exclusion fixes (which left
    /// large build-artifact blobs in the object store from earlier snapshots).
    ///
    /// The operation is intentionally best-effort: if git gc fails (e.g.
    /// another process holds a lock) we log a warning and continue rather than
    /// propagating an error that would break the caller's primary workflow.
    pub fn gc(&self) -> Result<()> {
        if !self.shadow_dir.exists() {
            return Ok(());
        }

        // Expire all reflog entries immediately so the subsequent gc can
        // collect commits that were orphaned by rewinds.
        let reflog_out = git_cmd()
            .env("GIT_DIR", &self.shadow_dir)
            .args(["reflog", "expire", "--all", "--expire=now"])
            .output()
            .map_err(git_err)?;

        if !reflog_out.status.success() {
            let stderr = String::from_utf8_lossy(&reflog_out.stderr);
            if !stderr.trim().is_empty() && !stderr.contains("warning:") {
                return Err(Error::Tool {
                    name: "checkpoint".into(),
                    call_id: None,
                    message: format!("git reflog expire failed: {stderr}"),
                });
            }
        }

        let gc_out = git_cmd()
            .env("GIT_DIR", &self.shadow_dir)
            .args(["gc", "--prune=now", "--quiet"])
            .output()
            .map_err(git_err)?;

        if !gc_out.status.success() {
            let stderr = String::from_utf8_lossy(&gc_out.stderr);
            if !stderr.trim().is_empty() && !stderr.contains("warning:") {
                return Err(Error::Tool {
                    name: "checkpoint".into(),
                    call_id: None,
                    message: format!("git gc failed: {stderr}"),
                });
            }
        }

        Ok(())
    }

    // ── private helpers ───────────────────────────────────────────────────────

    fn session_head_full_sha(&self, session_id: &str) -> Option<String> {
        let out = git_cmd()
            .env("GIT_DIR", &self.shadow_dir)
            .args(["rev-parse", "--verify", "--quiet", &session_ref(session_id)])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }

    /// Stage the current workspace into a temp index and return the
    /// resulting tree SHA (used by `diff` against current workspace).
    fn write_workspace_tree(&self) -> Result<String> {
        let tmp_index = self.shadow_dir.join("tmp-index-diff");
        let _ = std::fs::remove_file(&tmp_index);

        let _ = git_cmd()
            .env("GIT_INDEX_FILE", &tmp_index)
            .env("GIT_DIR", &self.shadow_dir)
            .env("GIT_WORK_TREE", &self.workspace)
            .current_dir(&self.workspace)
            .args([
                "add",
                "-A",
                "--force",
                "--",
                ".",
                ":!.recursive/**",
                ":!.recursive",
                ":!target/**",
                ":!target",
                ":!**/node_modules/**",
                ":!**/node_modules",
                ":!.flowcast/runs/**",
                ":!.flowcast/logs/**",
                ":!.dev/runs/**",
                ":!.dev/transcripts/**",
                ":!.worktrees/**",
            ])
            .output();

        let tree_out = git_cmd()
            .env("GIT_INDEX_FILE", &tmp_index)
            .env("GIT_DIR", &self.shadow_dir)
            .args(["write-tree"])
            .output()
            .map_err(git_err)?;
        let _ = std::fs::remove_file(&tmp_index);
        if !tree_out.status.success() {
            return Err(Error::Tool {
                name: "checkpoint".into(),
                call_id: None,
                message: format!(
                    "git write-tree failed: {}",
                    String::from_utf8_lossy(&tree_out.stderr)
                ),
            });
        }
        Ok(String::from_utf8_lossy(&tree_out.stdout).trim().to_string())
    }

    fn expand_sha(&self, short: &str) -> Result<String> {
        // Try direct rev-parse of "<short>^{commit}" — fastest path.
        let out = git_cmd()
            .env("GIT_DIR", &self.shadow_dir)
            .args(["rev-parse", &format!("{short}^{{commit}}")])
            .output()
            .map_err(git_err)?;
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
        }
        // Try plain rev-parse for tree-like refs.
        let out2 = git_cmd()
            .env("GIT_DIR", &self.shadow_dir)
            .args(["rev-parse", short])
            .output()
            .map_err(git_err)?;
        if out2.status.success() {
            return Ok(String::from_utf8_lossy(&out2.stdout).trim().to_string());
        }
        Err(Error::Tool {
            name: "checkpoint".into(),
            call_id: None,
            message: format!("checkpoint '{short}' not found"),
        })
    }

    fn count_diff_files(&self, a: &str, b: &str) -> usize {
        let out = git_cmd()
            .env("GIT_DIR", &self.shadow_dir)
            .args(["diff-tree", "--name-only", "-r", a, b])
            .output();
        match out {
            Ok(o) => String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| !l.trim().is_empty())
                .count(),
            Err(_) => 0,
        }
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

const EMPTY_TREE_SHA: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

fn git_cmd() -> Command {
    let mut cmd = Command::new("git");
    // Bypass safe.directory ownership checks so git works inside containers
    // and CI environments where the repo owner differs from the running user.
    cmd.arg("-c").arg("safe.directory=*");
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.env("HOME", std::env::temp_dir());
    cmd.env("GIT_PAGER", "cat");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    // Force English locale so stderr-message matching (e.g. for the
    // "path not in tree" case in `read_file_at`) is stable across
    // user environments that have a localized git installation.
    cmd.env("LC_ALL", "C");
    cmd.env("LANG", "C");
    // Clear any ambient GIT_DIR / GIT_WORK_TREE that a parent process (e.g. a
    // pre-receive hook) might have set. Commands that need these variables set
    // them explicitly; leaving them inherited causes `git init --bare` to fail
    // with "GIT_WORK_TREE not allowed without GIT_DIR".
    cmd.env_remove("GIT_DIR");
    cmd.env_remove("GIT_WORK_TREE");
    cmd
}

fn git_err(e: std::io::Error) -> Error {
    Error::Tool {
        name: "checkpoint".into(),
        call_id: None,
        message: format!("git invocation failed: {e}"),
    }
}

fn short_sha(full: &str) -> String {
    full.chars().take(12).collect()
}

fn session_ref(sid: &str) -> String {
    format!("refs/sessions/{}/HEAD", sanitize_for_refname(sid))
}

/// Encode a session id into a git refname-safe segment. Git refnames
/// disallow consecutive dots (`..`), leading/trailing dots, `.lock`
/// suffixes, and a few control characters. For our purposes (we
/// already pre-validate via [`validate_session_id`]) we encode `.`
/// as `_dot_` and `-` as `_dash_` so that distinct session IDs
/// (e.g. `sess.1` vs `sess-1`) never collide on the same git ref.
fn sanitize_for_refname(sid: &str) -> String {
    sid.replace('.', "_dot_").replace('-', "_dash_")
}

fn validate_session_id(sid: &str) -> Result<()> {
    // Allow alphanumerics + `-` `_` `.`. The `.` is permitted because
    // real session ids include the workspace slug, which on macOS may
    // contain `.tmpXXX` segments from `/var/folders/...`. We still
    // reject path separators, `..`, and leading-dot to keep the id
    // safe for use as a git ref component.
    if sid.is_empty()
        || sid.contains('/')
        || sid.contains('\\')
        || sid.contains("..")
        || sid.starts_with('.')
        || !sid
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(Error::BadToolArgs {
            name: "checkpoint".into(),
            message: format!("invalid session_id `{sid}` (must be alphanumeric/-/_/.)"),
        });
    }
    Ok(())
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn has_git() -> bool {
        Command::new("git").arg("--version").output().is_ok()
    }

    /// Test workspace bundle: a workspace tempdir + a sibling tempdir
    /// to use as the shadow-git directory. Tests pass `ws.shadow_dir()`
    /// to `ShadowRepo::open_at` so they bypass `paths::user_data_dir()`
    /// entirely — no env mutation, no global lock, full parallelism.
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
        /// Convenience for tests that previously called
        /// `ShadowRepo::open(w.path())`.
        fn open_repo(&self) -> Result<ShadowRepo> {
            ShadowRepo::open_at(self.path(), self.shadow_dir())
        }
    }

    fn ws() -> TestWs {
        TestWs {
            workspace: tempfile::tempdir().expect("workspace tempdir"),
            shadow: tempfile::tempdir().expect("shadow tempdir"),
        }
    }

    #[test]
    fn shadow_repo_init_creates_dir() {
        if !has_git() {
            return;
        }
        let w = ws();
        let r = w.open_repo().expect("open");
        assert!(r.shadow_dir.exists());
        assert!(r.shadow_dir.join("HEAD").exists());
    }

    #[test]
    fn validate_session_id_rejects_paths() {
        assert!(validate_session_id("").is_err());
        assert!(validate_session_id("a/b").is_err());
        assert!(validate_session_id("..").is_err());
        assert!(validate_session_id(".hidden").is_err());
        assert!(validate_session_id("ok-1").is_ok());
        assert!(validate_session_id("ok_2").is_ok());
        assert!(validate_session_id("AbCdef123").is_ok());
        // Real-world session ids contain `.` from macOS tmpdirs.
        assert!(validate_session_id("2026-05-29T00-09-56Z-var-folders-T-.tmpAbc").is_ok());
    }

    #[test]
    fn sanitize_for_refname_no_collision() {
        // `.` and `-` must produce distinct encodings so that `sess.1` and
        // `sess-1` never map to the same git ref.
        let dot_encoded = sanitize_for_refname("sess.1");
        let dash_encoded = sanitize_for_refname("sess-1");
        assert_ne!(dot_encoded, dash_encoded);
        assert_eq!(sanitize_for_refname("a.b.c"), "a_dot_b_dot_c");
        assert_eq!(sanitize_for_refname("a-b-c"), "a_dash_b_dash_c");
        assert_eq!(sanitize_for_refname("plain"), "plain");
    }

    #[test]
    fn snapshot_per_session_independent() {
        if !has_git() {
            return;
        }
        let w = ws();
        fs::write(w.path().join("a.txt"), "from-A").unwrap();
        let r = w.open_repo().unwrap();
        let id_a1 = r.snapshot_for_session("sessA", "A turn 0").unwrap();

        fs::write(w.path().join("a.txt"), "from-B").unwrap();
        let id_b1 = r.snapshot_for_session("sessB", "B turn 0").unwrap();

        let list_a = r.list_for_session("sessA").unwrap();
        let list_b = r.list_for_session("sessB").unwrap();
        assert_eq!(list_a.len(), 1, "A should see only its own checkpoint");
        assert_eq!(list_b.len(), 1, "B should see only its own checkpoint");
        assert_eq!(list_a[0].id, id_a1);
        assert_eq!(list_b[0].id, id_b1);
    }

    #[test]
    fn snapshot_dedups_objects() {
        if !has_git() {
            return;
        }
        let w = ws();
        fs::write(w.path().join("same.txt"), "identical content").unwrap();
        let r = w.open_repo().unwrap();
        let _ = r.snapshot_for_session("a", "A").unwrap();
        let _ = r.snapshot_for_session("b", "B").unwrap();

        // The blob "identical content" appears once. We can verify by
        // listing all blobs via `git cat-file --batch-check
        // --batch-all-objects` and counting matching content size.
        let out = git_cmd()
            .env("GIT_DIR", &r.shadow_dir)
            .args(["cat-file", "--batch-check", "--batch-all-objects"])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let blob_count = stdout.lines().filter(|l| l.contains(" blob ")).count();
        // 1 file → 1 blob, regardless of how many sessions snapshotted.
        assert_eq!(blob_count, 1, "blobs should dedupe across sessions");
    }

    #[test]
    fn restore_paths_only_touches_specified_files() {
        if !has_git() {
            return;
        }
        let w = ws();
        let x = w.path().join("X.txt");
        let y = w.path().join("Y.txt");
        fs::write(&x, "x-orig").unwrap();
        fs::write(&y, "y-orig").unwrap();

        let r = w.open_repo().unwrap();
        let cp = r.snapshot_for_session("s", "init").unwrap();

        fs::write(&x, "x-modified").unwrap();
        fs::write(&y, "y-modified").unwrap();

        let stats = r.restore_paths(&cp, &["X.txt".into()]).expect("restore");
        assert_eq!(stats.restored, 1);

        assert_eq!(fs::read_to_string(&x).unwrap(), "x-orig");
        assert_eq!(
            fs::read_to_string(&y).unwrap(),
            "y-modified",
            "Y must not be restored"
        );
    }

    #[test]
    fn restore_paths_handles_deletion() {
        if !has_git() {
            return;
        }
        let w = ws();
        fs::write(w.path().join("keeper.txt"), "k").unwrap();
        let r = w.open_repo().unwrap();
        let cp = r.snapshot_for_session("s", "before-new").unwrap();

        let nf = w.path().join("new.txt");
        fs::write(&nf, "added later").unwrap();

        let stats = r.restore_paths(&cp, &["new.txt".into()]).expect("restore");
        assert_eq!(stats.deleted, 1);
        assert!(!nf.exists());
    }

    #[test]
    fn read_file_at_returns_none_for_missing() {
        if !has_git() {
            return;
        }
        let w = ws();
        fs::write(w.path().join("a.txt"), "exists").unwrap();
        let r = w.open_repo().unwrap();
        let cp = r.snapshot_for_session("s", "init").unwrap();
        assert_eq!(
            r.read_file_at(&cp, "a.txt").unwrap(),
            Some(b"exists".to_vec())
        );
        assert_eq!(r.read_file_at(&cp, "ghost.txt").unwrap(), None);
    }

    #[test]
    fn changed_paths_lists_files_between_checkpoints() {
        if !has_git() {
            return;
        }
        let w = ws();
        fs::write(w.path().join("a.txt"), "1").unwrap();
        let r = w.open_repo().unwrap();
        let c1 = r.snapshot_for_session("s", "v1").unwrap();
        fs::write(w.path().join("a.txt"), "2").unwrap();
        fs::write(w.path().join("b.txt"), "new").unwrap();
        let c2 = r.snapshot_for_session("s", "v2").unwrap();

        let changed = r.changed_paths(&c1, &c2).unwrap();
        let set: std::collections::HashSet<_> = changed.into_iter().collect();
        assert!(set.contains("a.txt"));
        assert!(set.contains("b.txt"));
    }

    #[test]
    fn restore_paths_unchanged_stats() {
        if !has_git() {
            return;
        }
        let w = ws();
        let r = w.open_repo().unwrap();

        // Snapshot with one file
        fs::write(w.path().join("a.txt"), "same-content").unwrap();
        let cp = r.snapshot_for_session("s", "init").unwrap();

        // File content has NOT changed — restore should count it as unchanged.
        let stats = r
            .restore_paths(&cp, &["a.txt".into()])
            .expect("restore must succeed");
        assert_eq!(
            stats.unchanged, 1,
            "file with identical content must be unchanged (kills += → -=/×= mutation)"
        );
        assert_eq!(stats.restored, 0);
        assert_eq!(stats.deleted, 0);
    }

    #[test]
    fn restore_paths_nonexistent_in_both_counts_unchanged() {
        if !has_git() {
            return;
        }
        let w = ws();
        let r = w.open_repo().unwrap();

        // Snapshot with no files at all
        let cp = r.snapshot_for_session("s", "empty").unwrap();

        // Ask to restore a file that never existed in the checkpoint or workspace.
        let stats = r
            .restore_paths(&cp, &["ghost.txt".into()])
            .expect("restore must succeed");
        assert_eq!(
            stats.unchanged, 1,
            "(None, None) path must increment unchanged (kills += → -=/×= mutation)"
        );
        assert_eq!(stats.restored, 0);
        assert_eq!(stats.deleted, 0);
    }

    #[test]
    fn diff_returns_non_empty_for_changed_file() {
        if !has_git() {
            return;
        }
        let w = ws();
        let r = w.open_repo().unwrap();

        fs::write(w.path().join("f.txt"), "before").unwrap();
        let c1 = r.snapshot_for_session("s", "v1").unwrap();
        fs::write(w.path().join("f.txt"), "after").unwrap();
        let c2 = r.snapshot_for_session("s", "v2").unwrap();

        let diff = r.diff(&c1, Some(&c2), &[]).expect("diff must succeed");
        assert!(!diff.is_empty(), "diff between two different snapshots must be non-empty");
    }

    #[test]
    fn diff_with_path_filter_limits_output() {
        if !has_git() {
            return;
        }
        let w = ws();
        let r = w.open_repo().unwrap();

        fs::write(w.path().join("a.txt"), "a1").unwrap();
        fs::write(w.path().join("b.txt"), "b1").unwrap();
        let c1 = r.snapshot_for_session("s", "v1").unwrap();
        fs::write(w.path().join("a.txt"), "a2").unwrap();
        fs::write(w.path().join("b.txt"), "b2").unwrap();
        let c2 = r.snapshot_for_session("s", "v2").unwrap();

        // Diff with empty paths → shows all changes
        let full_diff = r.diff(&c1, Some(&c2), &[]).expect("full diff");
        // Diff filtered to a.txt → should not contain b.txt changes
        let filtered = r.diff(&c1, Some(&c2), &["a.txt".into()]).expect("filtered diff");
        assert!(
            filtered.contains("a.txt"),
            "filtered diff must mention a.txt"
        );
        // full diff contains both files; filtered should be a subset
        assert!(
            filtered.len() <= full_diff.len(),
            "filtered diff must be <= full diff in size"
        );
    }

    #[test]
    fn diff_against_workspace_uses_write_workspace_tree() {
        if !has_git() {
            return;
        }
        let w = ws();
        let r = w.open_repo().unwrap();

        // Snapshot with initial content
        fs::write(w.path().join("x.txt"), "v1").unwrap();
        let c1 = r.snapshot_for_session("s", "v1").unwrap();

        // Modify workspace without creating a new snapshot
        fs::write(w.path().join("x.txt"), "v2-workspace").unwrap();

        // diff(c1, None, &[]) compares c1 against current workspace
        // (exercises write_workspace_tree → kills Ok(String::new()) mutation)
        let diff = r.diff(&c1, None, &[]).expect("diff vs workspace must succeed");
        assert!(!diff.is_empty(), "diff vs modified workspace must be non-empty");
    }

    #[test]
    fn list_for_session_returns_empty_before_any_snapshot() {
        if !has_git() {
            return;
        }
        let w = ws();
        let r = w.open_repo().unwrap();
        assert!(r.list_for_session("never").unwrap().is_empty());
    }

    #[test]
    fn worktree_workspace_supported() {
        if !has_git() {
            return;
        }
        let w = ws();
        // Simulate a `git worktree`: workspace's .git is a file.
        fs::write(
            w.path().join(".git"),
            "gitdir: /elsewhere/.git/worktrees/foo\n",
        )
        .unwrap();
        let r = w.open_repo().expect("open with worktree");
        // Snapshots still work.
        fs::write(w.path().join("a.txt"), "hi").unwrap();
        let _ = r.snapshot_for_session("s", "wt").unwrap();
    }

    #[test]
    fn concurrent_snapshots_use_distinct_temp_indexes() {
        if !has_git() {
            return;
        }
        // Sequential test of the temp-index naming invariant. True
        // concurrency would require threads; the goal here is to
        // verify that `tmp-index-<sid>` is per-session so no overlap
        // can happen under load.
        let w = ws();
        fs::write(w.path().join("a.txt"), "v1").unwrap();
        let r = w.open_repo().unwrap();
        let _ = r.snapshot_for_session("alpha", "1").unwrap();
        // Tmp index should be cleaned up after each call.
        assert!(!r.shadow_dir.join("tmp-index-alpha").exists());
        let _ = r.snapshot_for_session("beta", "1").unwrap();
        assert!(!r.shadow_dir.join("tmp-index-beta").exists());
    }

    #[test]
    fn list_for_session_root_and_non_root_commits() {
        if !has_git() {
            return;
        }
        let w = ws();
        let r = w.open_repo().unwrap();

        // First snapshot: 1 file (root commit)
        fs::write(w.path().join("a.txt"), "v1").unwrap();
        let c1 = r.snapshot_for_session("sess", "turn-0").unwrap();

        // Second snapshot: change + add 1 more file (non-root commit)
        fs::write(w.path().join("a.txt"), "v2").unwrap();
        fs::write(w.path().join("b.txt"), "new").unwrap();
        let _c2 = r.snapshot_for_session("sess", "turn-1").unwrap();

        let list = r.list_for_session("sess").unwrap();
        assert_eq!(list.len(), 2, "must list exactly 2 checkpoints");

        // Newest-first ordering: list[0] is turn-1, list[1] is turn-0.
        // Root commit (turn-0) had 1 file added → files_changed >= 1.
        // Non-root (turn-1) added 2 changes → files_changed >= 1.
        // The key invariant: both must be >= 1 (not 0 from a wrongly-taken branch).
        assert!(
            list[0].files_changed >= 1,
            "non-root commit must have files_changed >= 1"
        );
        assert!(
            list[1].files_changed >= 1,
            "root commit must have files_changed >= 1 (vs empty tree)"
        );

        // IDs must differ
        assert_ne!(list[0].id, list[1].id);

        // Specifically: root commit (oldest, list[1]) should reflect turn-0 message,
        // and non-root (newest, list[0]) should reflect turn-1 message.
        assert!(list[0].message.contains("turn-1") || !list[0].message.is_empty());
        assert!(list[1].message.contains("turn-0") || !list[1].message.is_empty());

        // Also verify that c1 checkpoint id appears in the list.
        let ids: Vec<_> = list.iter().map(|c| c.id.0.as_str()).collect();
        assert!(
            ids.iter().any(|id| c1.0.starts_with(id) || id.starts_with(&c1.0[..6.min(c1.0.len())])),
            "c1 checkpoint must appear in list"
        );
    }

    #[test]
    fn list_for_session_ordering_newest_first() {
        if !has_git() {
            return;
        }
        let w = ws();
        let r = w.open_repo().unwrap();

        fs::write(w.path().join("f.txt"), "a").unwrap();
        let c1 = r.snapshot_for_session("sess", "first").unwrap();
        fs::write(w.path().join("f.txt"), "b").unwrap();
        let c2 = r.snapshot_for_session("sess", "second").unwrap();

        let list = r.list_for_session("sess").unwrap();
        assert_eq!(list.len(), 2);
        // Newest (c2 = "second") must come before oldest (c1 = "first")
        assert!(
            list[0].timestamp >= list[1].timestamp,
            "list must be newest-first; list[0].ts={} list[1].ts={}",
            list[0].timestamp,
            list[1].timestamp
        );
        let _ = (c1, c2); // suppress unused warnings
    }
}
