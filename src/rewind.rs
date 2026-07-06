//! Selective rewind for a session's checkpoint chain.
//!
//! Reads `checkpoints.jsonl`, identifies which files this session
//! touched in turns >= the rewind cutoff, detects conflicts where the
//! current workspace state diverges from this session's last known
//! post-snapshot for those files, and asks [`ShadowRepo::restore_paths`]
//! to revert only that file subset.
//!
//! Multi-session safety: because `restore_paths` operates on a path
//! whitelist, files modified by a sibling session are never touched
//! unless they happen to also fall within this session's touched set
//! (in which case the conflict is surfaced and the user must
//! `--force`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::checkpoint::{CheckpointId, RestoreStats, ShadowRepo};
use crate::checkpoint_log::{read_log, truncate_to_turn};
use crate::error::{Error, Result};

/// Outcome of a [`rewind`] call when conflicts were detected.
#[derive(Debug, Clone)]
pub struct ConflictReport {
    /// Files that have diverged from this session's last known state.
    pub files: Vec<String>,
}

/// Plan describing what a rewind would do, for a dry-run preview.
#[derive(Debug, Clone)]
pub struct RewindPlan {
    /// Target checkpoint to restore the touched files to.
    pub target: CheckpointId,
    /// Workspace-relative paths in this session's touched set.
    pub touched_paths: Vec<String>,
    /// The last "post" checkpoint this session recorded. Used for
    /// conflict detection.
    pub last_known_post: Option<CheckpointId>,
    /// Turns that will be removed from `checkpoints.jsonl` and from
    /// the conversation transcript on commit.
    pub turns_to_drop: Vec<usize>,
}

/// Compute a [`RewindPlan`] for rewinding `to_turn`.
///
/// `to_turn` is the turn index whose **start** state we want to
/// restore. So `to_turn = 0` undoes everything; `to_turn = 3`
/// preserves turns 0..=2 and undoes 3 and later.
///
/// **Goal 284**: with on-demand checkpoints, the target checkpoint is
/// the `id` field of the most recent log entry with `turn < to_turn`,
/// or the `pre` field of the entry for `to_turn` (old auto-snapshot
/// sessions). If neither is available, returns an error.
///
/// Returns an error if the log doesn't reach `to_turn` or if no
/// suitable checkpoint was recorded.
pub fn plan_rewind(log_path: &Path, to_turn: usize) -> Result<RewindPlan> {
    let recs = read_log(log_path)?;
    if recs.is_empty() {
        return Err(Error::Tool {
            name: "rewind".into(),
            call_id: None,
            message: "no checkpoints recorded for this session".into(),
        });
    }

    // Find the target checkpoint: prefer the `id` of the most recent
    // entry before `to_turn`, falling back to the `pre` field of the
    // entry for `to_turn` (old auto-snapshot sessions).
    let target: CheckpointId = if to_turn == 0 {
        // Rewind to start: use first record's pre (old auto-snapshot
        // sessions), falling back to its id (new on-demand sessions).
        // The id of the first record is the closest thing to "before
        // turn 0" we have in the on-demand model.
        recs.first()
            .and_then(|r| r.pre.clone().or_else(|| Some(r.id.clone())))
            .ok_or_else(|| Error::Tool {
                name: "rewind".into(),
                call_id: None,
                message: "no checkpoint recorded before turn 0".into(),
            })?
    } else {
        // Find the most recent entry with turn < to_turn.
        let prev = recs.iter().rev().find(|r| r.turn < to_turn);
        match prev {
            Some(r) => r.id.clone(),
            None => {
                // Fall back to the pre field of the entry at to_turn
                // (for old auto-snapshot sessions where pre marks the
                // state before this turn's changes).
                let target_rec =
                    recs.iter()
                        .find(|r| r.turn == to_turn)
                        .ok_or_else(|| Error::Tool {
                            name: "rewind".into(),
                            call_id: None,
                            message: format!(
                                "turn {to_turn} is not in this session's checkpoint log"
                            ),
                        })?;
                target_rec.pre.clone().ok_or_else(|| Error::Tool {
                    name: "rewind".into(),
                    call_id: None,
                    message: format!(
                        "turn {to_turn} has no pre-snapshot recorded; \
                         cannot rewind to its start"
                    ),
                })?
            }
        }
    };

    let mut touched: HashSet<String> = HashSet::new();
    let mut turns_to_drop = Vec::new();
    for r in &recs {
        if r.turn >= to_turn {
            turns_to_drop.push(r.turn);
            for p in &r.touched_files {
                touched.insert(p.clone());
            }
        }
    }
    let mut touched_paths: Vec<String> = touched.into_iter().collect();
    touched_paths.sort();

    let last_known_post = recs.last().map(|r| r.id.clone());

    Ok(RewindPlan {
        target,
        touched_paths,
        last_known_post,
        turns_to_drop,
    })
}

/// Check whether the workspace's current state for the touched files
/// matches the session's last-known post-snapshot. Returns the list
/// of conflicting paths (empty list = safe to proceed).
pub fn detect_conflicts(repo: &ShadowRepo, plan: &RewindPlan) -> Result<Vec<String>> {
    let last = match &plan.last_known_post {
        Some(id) => id,
        None => return Ok(vec![]),
    };
    let mut conflicts = Vec::new();
    for path in &plan.touched_paths {
        let abs = repo.workspace().join(path);
        let current = std::fs::read(&abs).ok();
        let expected = repo.read_file_at(last, path)?;
        if current != expected {
            conflicts.push(path.clone());
        }
    }
    Ok(conflicts)
}

/// Apply a rewind plan: restore files, then truncate the checkpoint
/// log. Transcript truncation is left to the caller because it owns
/// `transcript.jsonl`.
///
/// `force = false` aborts on detected conflicts and returns
/// `Err(Error::Tool { ... })` whose message lists each file. Callers
/// can choose to retry with `force = true`.
pub fn apply_rewind(
    repo: &ShadowRepo,
    log_path: &Path,
    plan: &RewindPlan,
    force: bool,
) -> Result<RewindResult> {
    if !force {
        let conflicts = detect_conflicts(repo, plan)?;
        if !conflicts.is_empty() {
            return Err(Error::Tool {
                name: "rewind".into(),
                call_id: None,
                message: format!(
                    "rewind blocked by {} conflicting file(s): {}\n\
                     Re-run with --force to overwrite.",
                    conflicts.len(),
                    conflicts.join(", ")
                ),
            });
        }
    }
    let stats = repo.restore_paths(&plan.target, &plan.touched_paths)?;
    truncate_to_turn(
        log_path,
        plan.turns_to_drop.iter().copied().min().unwrap_or(0),
    )?;
    Ok(RewindResult {
        stats,
        dropped_turns: plan.turns_to_drop.clone(),
    })
}

/// Result of a successful rewind.
#[derive(Debug, Clone)]
pub struct RewindResult {
    pub stats: RestoreStats,
    pub dropped_turns: Vec<usize>,
}

/// Convenience: locate `checkpoints.jsonl` for a session given the
/// workspace root and a session id. Mirrors the path layout chosen
/// by `SessionWriter`.
pub fn checkpoint_log_path(workspace: &Path, workspace_slug: &str, session_id: &str) -> PathBuf {
    workspace
        .join(".recursive")
        .join("sessions")
        .join(workspace_slug)
        .join(session_id)
        .join("checkpoints.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint_log::{CheckpointLogWriter, CheckpointRecord, TouchedVia};
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    #[test]
    fn checkpoint_log_path_has_correct_structure() {
        // kills function-level replacement and component-order mutations
        use std::path::PathBuf;
        let workspace = PathBuf::from("/home/user/project");
        let path = checkpoint_log_path(&workspace, "my-slug", "sess-01");
        assert_eq!(
            path,
            PathBuf::from(
                "/home/user/project/.recursive/sessions/my-slug/sess-01/checkpoints.jsonl"
            )
        );
    }

    fn has_git() -> bool {
        Command::new("git").arg("--version").output().is_ok()
    }

    /// Workspace tempdir + sibling shadow tempdir. The pair is passed
    /// to `ShadowRepo::open_at` so tests don't need the global env
    /// lock and can run in parallel. `dir.path()` returns the
    /// workspace; `shadow_dir(&dir)` returns the shadow path.
    struct ShadowWs {
        workspace: TempDir,
        shadow: TempDir,
    }

    impl ShadowWs {
        fn path(&self) -> &Path {
            self.workspace.path()
        }
        fn open_repo(&self) -> Result<ShadowRepo> {
            ShadowRepo::open_at(self.path(), self.shadow.path().join("shadow-git"))
        }
    }

    fn shadow_ws() -> ShadowWs {
        ShadowWs {
            workspace: tempfile::tempdir().expect("workspace tempdir"),
            shadow: tempfile::tempdir().expect("shadow tempdir"),
        }
    }

    fn write_log(path: &Path, records: &[CheckpointRecord]) {
        let w = CheckpointLogWriter::open(path).unwrap();
        for r in records {
            w.append(r).unwrap();
        }
    }

    fn rec(turn: usize, pre: Option<&str>, id: &str, touched: &[&str]) -> CheckpointRecord {
        CheckpointRecord {
            turn,
            pre: pre.map(|s| CheckpointId(s.to_string())),
            id: CheckpointId(id.to_string()),
            message: None,
            touched_files: touched.iter().map(|s| s.to_string()).collect(),
            touched_via: TouchedVia::Structured,
            started_at: 0,
            finished_at: 0,
            saved_at: 0,
        }
    }

    #[test]
    fn plan_rewind_collects_touched_files_across_dropped_turns() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("c.jsonl");
        write_log(
            &log,
            &[
                rec(0, Some("p0"), "q0", &["a.txt"]),
                rec(1, Some("q0"), "q1", &["b.txt"]),
                rec(2, Some("q1"), "q2", &["c.txt"]),
            ],
        );
        let plan = plan_rewind(&log, 1).unwrap();
        assert_eq!(plan.target.0, "q0");
        assert_eq!(plan.touched_paths, vec!["b.txt", "c.txt"]);
        assert_eq!(plan.turns_to_drop, vec![1, 2]);
        assert_eq!(
            plan.last_known_post.as_ref().map(|c| c.0.as_str()),
            Some("q2")
        );
    }

    #[test]
    fn plan_rewind_to_zero_drops_all() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("c.jsonl");
        write_log(
            &log,
            &[
                rec(0, Some("p0"), "q0", &["a.txt"]),
                rec(1, Some("q0"), "q1", &["b.txt"]),
            ],
        );
        let plan = plan_rewind(&log, 0).unwrap();
        assert_eq!(plan.turns_to_drop, vec![0, 1]);
    }

    /// Goal 284: on-demand checkpoints have no `pre` field. Verify
    /// `plan_rewind(to_turn=0)` uses the `id` of the first record
    /// as the target when `pre` is absent.
    #[test]
    fn plan_rewind_to_zero_uses_first_id_when_no_pre() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("c.jsonl");
        write_log(
            &log,
            &[
                rec(0, None, "q0", &["a.txt"]),
                rec(1, None, "q1", &["b.txt"]),
            ],
        );
        let plan = plan_rewind(&log, 0).unwrap();
        assert_eq!(plan.target.0, "q0");
        assert_eq!(plan.turns_to_drop, vec![0, 1]);
    }

    #[test]
    fn plan_rewind_uses_closest_previous_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("c.jsonl");
        write_log(&log, &[rec(0, Some("p0"), "q0", &["a.txt"])]);
        // to_turn=5 is beyond the last recorded turn; use turn 0's id.
        let plan = plan_rewind(&log, 5).unwrap();
        assert_eq!(plan.target.0, "q0");
        assert!(plan.turns_to_drop.is_empty());
        assert!(plan.touched_paths.is_empty());
    }

    #[test]
    fn detect_conflicts_flags_externally_modified_file() {
        if !has_git() {
            return;
        }
        let dir = shadow_ws();
        fs::write(dir.path().join("a.txt"), "v0").unwrap();
        let repo = dir.open_repo().unwrap();
        let pre = repo.snapshot_for_session("s", "pre").unwrap();
        fs::write(dir.path().join("a.txt"), "v1").unwrap();
        let post = repo.snapshot_for_session("s", "post").unwrap();

        // Simulate "external" change after this session's last snapshot.
        fs::write(dir.path().join("a.txt"), "v2-from-someone-else").unwrap();

        let plan = RewindPlan {
            target: pre.clone(),
            touched_paths: vec!["a.txt".into()],
            last_known_post: Some(post),
            turns_to_drop: vec![0],
        };
        let conflicts = detect_conflicts(&repo, &plan).unwrap();
        assert_eq!(conflicts, vec!["a.txt".to_string()]);
    }

    #[test]
    fn apply_rewind_restores_and_truncates_log() {
        if !has_git() {
            return;
        }
        let dir = shadow_ws();
        let log = dir.path().join("c.jsonl");
        let target_path = dir.path().join("file.txt");
        fs::write(&target_path, "before").unwrap();
        let repo = dir.open_repo().unwrap();
        let pre = repo.snapshot_for_session("s", "t0 pre").unwrap();
        fs::write(&target_path, "after").unwrap();
        let post = repo.snapshot_for_session("s", "t0 post").unwrap();

        write_log(
            &log,
            &[CheckpointRecord {
                turn: 0,
                pre: Some(pre.clone()),
                id: post.clone(),
                message: None,
                touched_files: vec!["file.txt".into()],
                touched_via: TouchedVia::Structured,
                started_at: 0,
                finished_at: 0,
                saved_at: 0,
            }],
        );

        let plan = plan_rewind(&log, 0).unwrap();
        let result = apply_rewind(&repo, &log, &plan, false).unwrap();
        assert_eq!(result.stats.restored, 1);
        assert_eq!(fs::read_to_string(&target_path).unwrap(), "before");
        assert!(read_log(&log).unwrap().is_empty());
    }

    #[test]
    fn apply_rewind_blocks_on_conflict_without_force() {
        if !has_git() {
            return;
        }
        let dir = shadow_ws();
        let log = dir.path().join("c.jsonl");
        let f = dir.path().join("file.txt");
        fs::write(&f, "v0").unwrap();
        let repo = dir.open_repo().unwrap();
        let pre = repo.snapshot_for_session("s", "pre").unwrap();
        fs::write(&f, "v1").unwrap();
        let post = repo.snapshot_for_session("s", "post").unwrap();

        write_log(
            &log,
            &[CheckpointRecord {
                turn: 0,
                pre: Some(pre.clone()),
                id: post.clone(),
                message: None,
                touched_files: vec!["file.txt".into()],
                touched_via: TouchedVia::Structured,
                started_at: 0,
                finished_at: 0,
                saved_at: 0,
            }],
        );

        // External edit between session's last snapshot and rewind.
        fs::write(&f, "external-edit").unwrap();

        let plan = plan_rewind(&log, 0).unwrap();
        let err = apply_rewind(&repo, &log, &plan, false).unwrap_err();
        assert!(err.to_string().contains("conflict"));
    }

    #[test]
    fn apply_rewind_force_overrides_conflict() {
        if !has_git() {
            return;
        }
        let dir = shadow_ws();
        let log = dir.path().join("c.jsonl");
        let f = dir.path().join("file.txt");
        fs::write(&f, "v0").unwrap();
        let repo = dir.open_repo().unwrap();
        let pre = repo.snapshot_for_session("s", "pre").unwrap();
        fs::write(&f, "v1").unwrap();
        let post = repo.snapshot_for_session("s", "post").unwrap();

        write_log(
            &log,
            &[CheckpointRecord {
                turn: 0,
                pre: Some(pre.clone()),
                id: post.clone(),
                message: None,
                touched_files: vec!["file.txt".into()],
                touched_via: TouchedVia::Structured,
                started_at: 0,
                finished_at: 0,
                saved_at: 0,
            }],
        );

        fs::write(&f, "external").unwrap();
        let plan = plan_rewind(&log, 0).unwrap();
        let _ = apply_rewind(&repo, &log, &plan, true).unwrap();
        assert_eq!(fs::read_to_string(&f).unwrap(), "v0");
    }

    #[test]
    fn rewind_does_not_touch_sibling_session_files() {
        if !has_git() {
            return;
        }
        let dir = shadow_ws();
        let log_a = dir.path().join("a.jsonl");
        let mine = dir.path().join("mine.txt");
        let theirs = dir.path().join("theirs.txt");
        fs::write(&mine, "mine-v0").unwrap();
        fs::write(&theirs, "theirs-v0").unwrap();
        let repo = dir.open_repo().unwrap();

        let pre_a = repo.snapshot_for_session("a", "pre").unwrap();
        // A modifies its own file.
        fs::write(&mine, "mine-v1").unwrap();
        let post_a = repo.snapshot_for_session("a", "post").unwrap();

        // B (sibling) modifies its own file *after* A took its post-snapshot.
        fs::write(&theirs, "theirs-v1").unwrap();

        write_log(
            &log_a,
            &[CheckpointRecord {
                turn: 0,
                pre: Some(pre_a.clone()),
                id: post_a.clone(),
                message: None,
                touched_files: vec!["mine.txt".into()],
                touched_via: TouchedVia::Structured,
                started_at: 0,
                finished_at: 0,
                saved_at: 0,
            }],
        );

        let plan = plan_rewind(&log_a, 0).unwrap();
        let _ = apply_rewind(&repo, &log_a, &plan, false).unwrap();

        assert_eq!(fs::read_to_string(&mine).unwrap(), "mine-v0");
        assert_eq!(
            fs::read_to_string(&theirs).unwrap(),
            "theirs-v1",
            "sibling session's file must not be touched"
        );
    }

    #[test]
    fn plan_rewind_no_prev_and_target_has_no_pre_returns_error() {
        // kills `ok_or_else(|| "turn {to_turn} has no pre-snapshot recorded")` guard removal
        // Scenario: only turn=3 exists (no prev with turn < 3), and turn=3 has no `pre` field.
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("ck.jsonl");
        // Only turn 3 exists with no pre field.
        write_log(&log_path, &[rec(3, None, "snap-3-id", &["x.txt"])]);
        let err = plan_rewind(&log_path, 3);
        assert!(
            err.is_err(),
            "turn with no pre-snapshot must return an error"
        );
        let msg = format!("{}", err.unwrap_err());
        assert!(
            msg.contains("no pre-snapshot") || msg.contains("pre"),
            "error must mention missing pre-snapshot; got: {msg}"
        );
    }

    #[test]
    fn plan_rewind_returns_error_for_empty_log() {
        // kills `if recs.is_empty() { return Err(...) }` guard-removal mutation
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("empty.jsonl");
        // Write an empty file (no records).
        std::fs::write(&log_path, "").unwrap();
        let err = plan_rewind(&log_path, 1);
        assert!(err.is_err(), "empty log must return an error");
        let msg = format!("{}", err.unwrap_err());
        assert!(
            msg.contains("no checkpoints"),
            "error must mention 'no checkpoints', got: {msg}"
        );
    }

    #[test]
    fn plan_rewind_returns_error_when_turn_not_in_log_and_no_prev() {
        // kills the `ok_or_else(|| Error::Tool { "turn {to_turn} is not in this session" })` path.
        // We need to_turn=3 where all existing records have turn >= 3, so `prev` (turn < 3) is None,
        // and to_turn=3 itself does not exist.
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("ck.jsonl");
        // Only turn 5 exists. to_turn=3: no record has turn < 3, and turn 3 is not in the log.
        write_log(
            &log_path,
            &[rec(5, Some("snap-pre"), "snap-post", &["a.txt"])],
        );
        let err = plan_rewind(&log_path, 3);
        assert!(err.is_err(), "non-existent turn must return an error");
        let msg = format!("{}", err.unwrap_err());
        assert!(
            msg.contains("3"),
            "error must mention the missing turn number, got: {msg}"
        );
    }

    #[test]
    fn checkpoint_log_path_omits_workspace_slug_duplication() {
        // kills literal mutations in the path segments
        let ws = std::path::Path::new("/home/user/myproject");
        let path = checkpoint_log_path(ws, "myproject", "sess-abc");
        let expected = ws
            .join(".recursive")
            .join("sessions")
            .join("myproject")
            .join("sess-abc")
            .join("checkpoints.jsonl");
        assert_eq!(path, expected);
    }
}
