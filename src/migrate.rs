//! Migrate legacy in-tree state to the per-user data dir.
//!
//! Pre-g142, sessions / shadow-git / scratchpad lived under
//! `<workspace>/.recursive/`. This module moves any such state to
//! `~/.recursive/workspaces/<ws-hash>/...` and is invoked by the
//! `recursive migrate` CLI command.
//!
//! Project-bundled assets (`skills/`, `mcp.json`) are left in place.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::paths;

/// Outcome of a migration attempt.
#[derive(Debug, Default, Clone)]
pub struct MigrateReport {
    /// Items that were moved (legacy_path, new_path).
    pub moved: Vec<(PathBuf, PathBuf)>,
    /// Items skipped because the destination already had data
    /// (legacy_path, destination).
    pub skipped: Vec<(PathBuf, PathBuf)>,
    /// True if the workspace had no legacy state at all.
    pub already_clean: bool,
    /// True if `<workspace>/.recursive/` was empty after the migration
    /// and was therefore removed.
    pub removed_empty_dotrecursive: bool,
}

/// Plan + execute the migration.
///
/// `dry_run = true` returns a report describing what would happen
/// without touching the filesystem.
pub fn migrate_workspace(workspace: &Path, dry_run: bool) -> Result<MigrateReport> {
    let mut report = MigrateReport::default();
    let legacy = paths::legacy_paths_in_workspace(workspace);
    if legacy.is_empty() {
        report.already_clean = true;
        return Ok(report);
    }

    let target_dir = paths::user_workspace_dir(workspace)?;

    for src in legacy {
        let name = src
            .file_name()
            .ok_or_else(|| Error::Tool {
                name: "migrate".into(),
                call_id: None,
                message: format!("legacy path has no file name: {}", src.display()),
            })?
            .to_owned();
        let dst = target_dir.join(&name);

        if dst.exists() {
            report.skipped.push((src, dst));
            continue;
        }

        if dry_run {
            report.moved.push((src, dst));
            continue;
        }

        // Try `rename` first (cheap, atomic on same FS). Fall back to
        // copy + remove if the user's home is on a different mount.
        if let Err(e) = std::fs::rename(&src, &dst) {
            // EXDEV (cross-device link) → fall back to copy + remove.
            if e.raw_os_error() == Some(libc_exdev()) {
                copy_recursively(&src, &dst).map_err(|e| Error::Tool {
                    name: "migrate".into(),
                    call_id: None,
                    message: format!("copy across mounts failed for {}: {e}", src.display()),
                })?;
                if src.is_dir() {
                    std::fs::remove_dir_all(&src).map_err(Error::Io)?;
                } else {
                    std::fs::remove_file(&src).map_err(Error::Io)?;
                }
            } else {
                return Err(Error::Tool {
                    name: "migrate".into(),
                    call_id: None,
                    message: format!("rename {} -> {} failed: {e}", src.display(), dst.display()),
                });
            }
        }
        report.moved.push((src, dst));
    }

    // Try to remove `<workspace>/.recursive/` if it's now empty
    // (skills/ and mcp.json being there will keep us out).
    if !dry_run {
        let dotrec = workspace.join(".recursive");
        if dotrec.is_dir() {
            if let Ok(mut iter) = std::fs::read_dir(&dotrec) {
                if iter.next().is_none() && std::fs::remove_dir(&dotrec).is_ok() {
                    report.removed_empty_dotrecursive = true;
                }
            }
        }
    }

    Ok(report)
}

/// macOS / Linux EXDEV constant. Avoids pulling in `libc` for one number.
fn libc_exdev() -> i32 {
    18
}

fn copy_recursively(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_recursively(&entry.path(), &dst.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::PinnedRecursiveHome;

    fn with_home<F: FnOnce()>(f: F) {
        let home = tempfile::tempdir().unwrap();
        let _g = PinnedRecursiveHome::new(home.path());
        f();
    }

    #[test]
    fn migrate_clean_workspace_is_noop() {
        with_home(|| {
            let ws = tempfile::tempdir().unwrap();
            let r = migrate_workspace(ws.path(), false).unwrap();
            assert!(r.already_clean);
            assert!(r.moved.is_empty());
        });
    }

    #[test]
    fn migrate_moves_sessions_and_shadow_git() {
        with_home(|| {
            let ws = tempfile::tempdir().unwrap();
            let dotrec = ws.path().join(".recursive");
            std::fs::create_dir_all(dotrec.join("sessions/foo")).unwrap();
            std::fs::write(dotrec.join("sessions/foo/.meta.json"), "{}").unwrap();
            std::fs::create_dir_all(dotrec.join("shadow-git")).unwrap();
            std::fs::write(dotrec.join("shadow-git/HEAD"), "ref: refs/heads/master\n").unwrap();
            std::fs::write(dotrec.join("scratchpad.json"), r#"{"entries":[]}"#).unwrap();

            let r = migrate_workspace(ws.path(), false).unwrap();
            assert!(!r.already_clean);
            assert_eq!(r.moved.len(), 3);

            // Old paths gone
            assert!(!dotrec.join("sessions").exists());
            assert!(!dotrec.join("shadow-git").exists());
            assert!(!dotrec.join("scratchpad.json").exists());

            // New paths in place
            let target = paths::user_workspace_dir(ws.path()).unwrap();
            assert!(target.join("sessions").is_dir());
            assert!(target.join("shadow-git").is_dir());
            assert!(target.join("scratchpad.json").is_file());
        });
    }

    #[test]
    fn migrate_skips_skills_and_mcp_json() {
        with_home(|| {
            let ws = tempfile::tempdir().unwrap();
            let dotrec = ws.path().join(".recursive");
            std::fs::create_dir_all(dotrec.join("sessions")).unwrap();
            std::fs::create_dir_all(dotrec.join("skills")).unwrap();
            std::fs::write(dotrec.join("mcp.json"), "{}").unwrap();

            let r = migrate_workspace(ws.path(), false).unwrap();
            assert_eq!(r.moved.len(), 1);
            // skills/ and mcp.json untouched
            assert!(dotrec.join("skills").is_dir());
            assert!(dotrec.join("mcp.json").is_file());
        });
    }

    #[test]
    fn migrate_aborts_on_destination_collision() {
        with_home(|| {
            let ws = tempfile::tempdir().unwrap();
            let dotrec = ws.path().join(".recursive");
            std::fs::create_dir_all(dotrec.join("sessions")).unwrap();

            // Pre-create the destination so migrate sees a collision.
            let target = paths::user_workspace_dir(ws.path()).unwrap();
            std::fs::create_dir_all(target.join("sessions")).unwrap();

            let r = migrate_workspace(ws.path(), false).unwrap();
            assert_eq!(r.moved.len(), 0);
            assert_eq!(r.skipped.len(), 1);
            // Source untouched.
            assert!(dotrec.join("sessions").exists());
        });
    }

    #[test]
    fn migrate_dry_run_does_not_mutate() {
        with_home(|| {
            let ws = tempfile::tempdir().unwrap();
            let dotrec = ws.path().join(".recursive");
            std::fs::create_dir_all(dotrec.join("sessions")).unwrap();

            let r = migrate_workspace(ws.path(), true).unwrap();
            assert_eq!(r.moved.len(), 1);

            // Source still there
            assert!(dotrec.join("sessions").exists());
            // Dest not created
            let target = paths::user_workspace_dir(ws.path()).unwrap();
            assert!(!target.join("sessions").exists());
        });
    }

    #[test]
    fn migrate_removes_empty_dotrecursive_when_only_byproducts_existed() {
        with_home(|| {
            let ws = tempfile::tempdir().unwrap();
            let dotrec = ws.path().join(".recursive");
            std::fs::create_dir_all(dotrec.join("sessions")).unwrap();

            let r = migrate_workspace(ws.path(), false).unwrap();
            assert!(r.removed_empty_dotrecursive);
            assert!(!dotrec.exists());
        });
    }

    #[test]
    fn libc_exdev_is_18() {
        // kills `replace libc_exdev -> i32 with 0` and literal mutations
        assert_eq!(libc_exdev(), 18, "EXDEV must be 18 (the Linux errno for cross-device link)");
    }

    #[test]
    fn copy_recursively_copies_file_to_new_path() {
        // kills `is_dir()` → always-true and function-level replacement mutations
        let src_dir = tempfile::TempDir::new().unwrap();
        let dst_dir = tempfile::TempDir::new().unwrap();
        let src_file = src_dir.path().join("hello.txt");
        std::fs::write(&src_file, "hello").unwrap();

        let dst_file = dst_dir.path().join("subdir").join("hello.txt");
        copy_recursively(&src_file, &dst_file).unwrap();

        let content = std::fs::read_to_string(&dst_file).unwrap();
        assert_eq!(content, "hello");
    }

    #[test]
    fn copy_recursively_copies_directory_tree() {
        // kills mutations in the `is_dir()` branch that handle recursive copies
        let src_dir = tempfile::TempDir::new().unwrap();
        let dst_dir = tempfile::TempDir::new().unwrap();

        let sub = src_dir.path().join("subdir");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("a.txt"), "aaa").unwrap();
        std::fs::write(src_dir.path().join("b.txt"), "bbb").unwrap();

        let dst = dst_dir.path().join("dest");
        copy_recursively(src_dir.path(), &dst).unwrap();

        let a = std::fs::read_to_string(dst.join("subdir").join("a.txt")).unwrap();
        let b = std::fs::read_to_string(dst.join("b.txt")).unwrap();
        assert_eq!(a, "aaa");
        assert_eq!(b, "bbb");
    }
}
