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

/// `<user_workspace_dir>/sessions/`. Honors `RECURSIVE_SESSIONS_DIR`
/// as a hard override (Goal-H J1) — useful for tests and
/// integrations that need to know exactly where the binary writes
/// a session for a given workspace, without going through the
/// user-data + workspace-hash layout.
pub fn user_sessions_dir(workspace: &Path) -> Result<PathBuf> {
    if let Some(custom) = std::env::var_os("RECURSIVE_SESSIONS_DIR") {
        return Ok(PathBuf::from(custom));
    }
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
    use crate::test_util::PinnedRecursiveHome;

    #[test]
    fn user_data_dir_honors_env_override() {
        let _g = PinnedRecursiveHome::new("/tmp/recursive-test-fixed");
        assert_eq!(user_data_dir(), PathBuf::from("/tmp/recursive-test-fixed"));
    }

    #[test]
    fn sessions_dir_honors_recursive_sessions_dir_override() {
        // Goal-H J1: the e2e smoke-01 session assertion
        // previously pointed at <workspace>/.recursive/sessions/
        // while the binary wrote to
        // <RECURSIVE_HOME>/workspaces/<hash>/sessions/. The fix
        // is RECURSIVE_SESSIONS_DIR — a hard override that
        // bypasses the user-data + workspace-hash layout. This
        // test pins that the override is honored and that
        // user_workspace_dir is **not** called (which would
        // canonicalize the path and crash on the empty
        // /tmp/recursive-test-fixed fixture).
        let prev = std::env::var_os("RECURSIVE_SESSIONS_DIR");
        std::env::set_var("RECURSIVE_SESSIONS_DIR", "/tmp/explicit-sessions");
        let dir = user_sessions_dir(Path::new("/tmp/recursive-test-fixed")).unwrap();
        assert_eq!(dir, PathBuf::from("/tmp/explicit-sessions"));
        match prev {
            Some(v) => std::env::set_var("RECURSIVE_SESSIONS_DIR", v),
            None => std::env::remove_var("RECURSIVE_SESSIONS_DIR"),
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
        let home = tempfile::tempdir().unwrap();
        let _g = PinnedRecursiveHome::new(home.path());

        let workspace = tempfile::tempdir().unwrap();
        let ws_dir = user_workspace_dir(workspace.path()).unwrap();
        assert!(ws_dir.exists());
        let marker = ws_dir.join("path.txt");
        assert!(marker.exists());
        let contents = std::fs::read_to_string(&marker).unwrap();
        // Marker should resolve to the canonical workspace path.
        let abs = workspace.path().canonicalize().unwrap();
        assert_eq!(contents, abs.display().to_string());
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

    // ── user_sessions_dir creates dir even when it doesn't exist ─────────────

    #[test]
    fn user_sessions_dir_creates_dir_when_absent() {
        // kills `delete ! in user_sessions_dir` line 67
        let home = tempfile::tempdir().unwrap();
        let _g = PinnedRecursiveHome::new(home.path());
        // Ensure RECURSIVE_SESSIONS_DIR is not set
        let prev = std::env::var_os("RECURSIVE_SESSIONS_DIR");
        std::env::remove_var("RECURSIVE_SESSIONS_DIR");

        let workspace = tempfile::tempdir().unwrap();
        let sessions = user_sessions_dir(workspace.path()).unwrap();
        assert!(
            sessions.exists(),
            "user_sessions_dir must create the directory if absent, got: {sessions:?}"
        );
        assert!(
            sessions.ends_with("sessions"),
            "path must end with 'sessions', got: {sessions:?}"
        );

        match prev {
            Some(v) => std::env::set_var("RECURSIVE_SESSIONS_DIR", v),
            None => std::env::remove_var("RECURSIVE_SESSIONS_DIR"),
        }
    }

    // ── user_shadow_git_dir / user_scratchpad_path path components ───────────

    #[test]
    fn user_shadow_git_dir_ends_with_shadow_git() {
        // kills `replace user_shadow_git_dir -> Result<PathBuf> with Ok(Default::default())`
        let home = tempfile::tempdir().unwrap();
        let _g = PinnedRecursiveHome::new(home.path());
        let workspace = tempfile::tempdir().unwrap();
        let p = user_shadow_git_dir(workspace.path()).unwrap();
        assert!(
            p.ends_with("shadow-git"),
            "user_shadow_git_dir must end with 'shadow-git', got: {p:?}"
        );
    }

    #[test]
    fn user_scratchpad_path_ends_with_scratchpad_json() {
        // kills `replace user_scratchpad_path -> Result<PathBuf> with Ok(Default::default())`
        let home = tempfile::tempdir().unwrap();
        let _g = PinnedRecursiveHome::new(home.path());
        let workspace = tempfile::tempdir().unwrap();
        let p = user_scratchpad_path(workspace.path()).unwrap();
        assert!(
            p.ends_with("scratchpad.json"),
            "user_scratchpad_path must end with 'scratchpad.json', got: {p:?}"
        );
    }

    // ── workspace_hash properties ─────────────────────────────────────────────

    #[test]
    fn workspace_hash_is_non_empty_and_12_chars() {
        // kills `replace workspace_hash_from_canonical -> String with String::new()`
        let h = workspace_hash(Path::new("/tmp/workspace-hash-test"));
        assert_eq!(h.len(), 12, "hash must be exactly 12 chars");
        assert!(!h.is_empty(), "hash must not be empty");
        assert_ne!(h, "xyzzy", "hash must not be placeholder");
    }
}
