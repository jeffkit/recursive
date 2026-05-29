//! Centralised path resolution for per-user and per-workspace state.
//!
//! Recursive separates state into three buckets:
//!
//! - **Per-user** (`~/.recursive/...`): config, global memory, facts.
//! - **Per-user, per-workspace** (`~/.recursive/workspaces/<hash>/...`):
//!   sessions, shadow-git checkpoints, scratchpad. These are the
//!   files this module owns.
//! - **Project-bundled** (`<workspace>/.recursive/...`): `skills/`,
//!   `mcp.json`. These ship with the project; this module never
//!   touches them.
//!
//! Tests can redirect the per-user root by setting
//! `RECURSIVE_HOME=<dir>`.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Per-user data root. Honors `RECURSIVE_HOME` for tests, otherwise
/// `$HOME/.recursive`.
pub fn user_data_dir() -> PathBuf {
    if let Some(custom) = std::env::var_os("RECURSIVE_HOME") {
        return PathBuf::from(custom);
    }
    if let Some(home) = dirs::home_dir() {
        return home.join(".recursive");
    }
    // Last resort: relative path. Should not happen on supported
    // platforms; using the cwd would silently regress to the old
    // behavior we're trying to escape.
    PathBuf::from(".recursive")
}

/// Per-user, per-workspace data dir.
///
/// Resolves to `<user_data_dir>/workspaces/<ws-hash>/`, creating it
/// on first call and writing a `path.txt` marker so a human can map
/// the hash back to the original workspace path.
pub fn user_workspace_dir(workspace: &Path) -> Result<PathBuf> {
    let abs = canonicalize_workspace(workspace)?;
    let hash = workspace_hash_from_canonical(&abs);
    let dir = user_data_dir().join("workspaces").join(&hash);
    if !dir.exists() {
        std::fs::create_dir_all(&dir).map_err(Error::Io)?;
        // Write path marker — best-effort; failing here would be
        // weird given we just created the directory, but we don't
        // want a marker bug to brick the run.
        let marker = dir.join("path.txt");
        if !marker.exists() {
            let _ = std::fs::write(&marker, abs.display().to_string());
        }
    }
    Ok(dir)
}

/// `<user_workspace_dir>/sessions/`.
pub fn user_sessions_dir(workspace: &Path) -> Result<PathBuf> {
    let dir = user_workspace_dir(workspace)?.join("sessions");
    if !dir.exists() {
        std::fs::create_dir_all(&dir).map_err(Error::Io)?;
    }
    Ok(dir)
}

/// `<user_workspace_dir>/shadow-git/` (parent only — caller is
/// responsible for `git init --bare`).
pub fn user_shadow_git_dir(workspace: &Path) -> Result<PathBuf> {
    let dir = user_workspace_dir(workspace)?.join("shadow-git");
    Ok(dir)
}

/// `<user_workspace_dir>/scratchpad.json`.
pub fn user_scratchpad_path(workspace: &Path) -> Result<PathBuf> {
    Ok(user_workspace_dir(workspace)?.join("scratchpad.json"))
}

/// 12-char workspace hash. Stable across calls for the same canonical
/// path. Public for diagnostics.
pub fn workspace_hash(workspace: &Path) -> String {
    let abs = canonicalize_workspace(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    workspace_hash_from_canonical(&abs)
}

fn workspace_hash_from_canonical(abs: &Path) -> String {
    let bytes = abs.as_os_str().to_string_lossy();
    let hash = blake3::hash(bytes.as_bytes());
    hash.to_hex().chars().take(12).collect()
}

fn canonicalize_workspace(workspace: &Path) -> Result<PathBuf> {
    // Avoid failing when the workspace path is not yet canonicalisable
    // (e.g. brand-new dir resolved to "."). Fall back to absolutising
    // via cwd.
    if let Ok(abs) = workspace.canonicalize() {
        return Ok(abs);
    }
    let cwd = std::env::current_dir().map_err(Error::Io)?;
    Ok(if workspace.is_absolute() {
        workspace.to_path_buf()
    } else {
        cwd.join(workspace)
    })
}

/// Detect any legacy in-tree state files that should now live under
/// the user data dir. Returns the absolute paths that exist.
///
/// Used by the startup warning and by `recursive migrate`.
pub fn legacy_paths_in_workspace(workspace: &Path) -> Vec<PathBuf> {
    let root = workspace.join(".recursive");
    if !root.exists() {
        return vec![];
    }
    [
        root.join("sessions"),
        root.join("shadow-git"),
        root.join("scratchpad.json"),
    ]
    .into_iter()
    .filter(|p| p.exists())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Tests that mutate `RECURSIVE_HOME` must run sequentially —
    /// the env var is process-global. This mutex serialises them.
    /// (Marked `#[allow(dead_code)]` because some test runners may
    /// not exercise every test that takes the lock.)
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn user_data_dir_honors_env_override() {
        let _g = ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os("RECURSIVE_HOME");
        std::env::set_var("RECURSIVE_HOME", "/tmp/recursive-test-fixed");
        assert_eq!(user_data_dir(), PathBuf::from("/tmp/recursive-test-fixed"));
        match prev {
            Some(v) => std::env::set_var("RECURSIVE_HOME", v),
            None => std::env::remove_var("RECURSIVE_HOME"),
        }
    }

    #[test]
    fn workspace_hash_is_stable_across_calls() {
        let p = Path::new("/tmp/some/path");
        assert_eq!(workspace_hash(p), workspace_hash(p));
        assert_eq!(workspace_hash(p).len(), 12);
    }

    #[test]
    fn workspace_hash_differs_for_different_paths() {
        let a = workspace_hash(Path::new("/tmp/a"));
        let b = workspace_hash(Path::new("/tmp/b"));
        assert_ne!(a, b);
    }

    #[test]
    fn user_workspace_dir_writes_path_txt() {
        let _g = ENV_LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("RECURSIVE_HOME");
        std::env::set_var("RECURSIVE_HOME", home.path());

        let workspace = tempfile::tempdir().unwrap();
        let ws_dir = user_workspace_dir(workspace.path()).unwrap();
        assert!(ws_dir.exists());
        let marker = ws_dir.join("path.txt");
        assert!(marker.exists());
        let contents = std::fs::read_to_string(&marker).unwrap();
        // Marker should resolve to the canonical workspace path.
        let abs = workspace.path().canonicalize().unwrap();
        assert_eq!(contents, abs.display().to_string());

        match prev {
            Some(v) => std::env::set_var("RECURSIVE_HOME", v),
            None => std::env::remove_var("RECURSIVE_HOME"),
        }
    }

    #[test]
    fn legacy_paths_detects_in_tree_state() {
        let workspace = tempfile::tempdir().unwrap();
        let dotrec = workspace.path().join(".recursive");
        std::fs::create_dir_all(dotrec.join("sessions")).unwrap();
        std::fs::create_dir_all(dotrec.join("shadow-git")).unwrap();
        std::fs::write(dotrec.join("scratchpad.json"), "{}").unwrap();

        let found = legacy_paths_in_workspace(workspace.path());
        assert_eq!(found.len(), 3, "expected all 3 legacy paths, got {found:?}");
    }

    #[test]
    fn legacy_paths_returns_empty_when_clean() {
        let workspace = tempfile::tempdir().unwrap();
        assert!(legacy_paths_in_workspace(workspace.path()).is_empty());
    }

    #[test]
    fn legacy_paths_does_not_flag_skills_or_mcp_json() {
        let workspace = tempfile::tempdir().unwrap();
        let dotrec = workspace.path().join(".recursive");
        std::fs::create_dir_all(dotrec.join("skills")).unwrap();
        std::fs::write(dotrec.join("mcp.json"), "{}").unwrap();
        // No sessions/, shadow-git/, or scratchpad.json → still clean
        // from the migrator's perspective.
        assert!(legacy_paths_in_workspace(workspace.path()).is_empty());
    }
}
