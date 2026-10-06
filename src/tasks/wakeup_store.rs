//! Durable pending-wakeup record (issue #99).
//!
//! `schedule_wakeup` writes an in-process slot, and `run_loop` used to sleep
//! on that slot while holding the only copy of the request: a restart between
//! two turns silently dropped the pending wakeup, and the agent was never
//! woken again.
//!
//! This module is the on-disk half of that loop state. `run_loop` writes the
//! request — reason, prompt, and *due time* — into the session directory as a
//! single JSONL line before it sleeps, and deletes it as soon as the wakeup
//! fires (the request is the following turn's goal then, not a pending record)
//! and again when the loop ends without arming another one. A process that
//! starts later scans the workspace's sessions for a record whose due time has
//! passed and restores it as the loop's first goal
//! ([`take_due_in_workspace`]).
//!
//! Semantics:
//!
//! - **One pending record per session directory.** The in-memory slot holds
//!   at most one request (a later `schedule_wakeup` overwrites it), so the
//!   file does too — a rewrite, not a growing queue.
//! - **Due-time only.** A record that is not yet due is *not* restored: the
//!   operator explicitly restarted the process, and blocking their terminal
//!   until the original wakeup time would be worse than dropping it.
//! - **Live sessions are skipped.** A directory whose `.lock` is held by a
//!   running process belongs to a loop that still owns its record, so the scan
//!   leaves it alone ([`crate::session::locked_by_live_process`]). Only a
//!   record whose owner is gone can be restored.
//! - **At-most-once restore.** Restoring consumes the record, so a second
//!   restart cannot replay work the first restart already handed to the agent.
//! - Writes go through [`crate::atomic::atomic_write`], the same
//!   write-then-rename discipline as session metadata.
//!
//! Wire format (one line):
//! ```json
//! {"reason":"poll build","prompt":"check ci","scheduled_at_ms":1700000000000,"due_at_ms":1700000030000}
//! ```

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// File name of the pending-wakeup record inside a session directory.
pub const WAKEUP_FILE_NAME: &str = "wakeup.jsonl";

/// How many directory levels below the workspace slug dir the scan descends.
/// The layout is `<sessions>/<slug>/<session-id>/wakeup.jsonl`, so one level
/// below the slug is enough; the slack tolerates a nested layout.
const MAX_SCAN_DEPTH: u8 = 2;

/// A wakeup request at rest: the request itself plus the wall-clock instant
/// it becomes due.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedWakeup {
    /// Why the wakeup was requested (free text, shown in logs and prompts).
    pub reason: String,
    /// Goal/context for the wakeup turn. May be empty.
    pub prompt: String,
    /// Unix milliseconds when the request was recorded.
    pub scheduled_at_ms: i64,
    /// Unix milliseconds when the wakeup becomes due.
    pub due_at_ms: i64,
}

impl PersistedWakeup {
    /// Record `delay` from `now_ms` as the due time.
    pub fn new(
        reason: impl Into<String>,
        prompt: impl Into<String>,
        delay: Duration,
        now_ms: i64,
    ) -> Self {
        let delay_ms = i64::try_from(delay.as_millis()).unwrap_or(i64::MAX);
        Self {
            reason: reason.into(),
            prompt: prompt.into(),
            scheduled_at_ms: now_ms,
            due_at_ms: now_ms.saturating_add(delay_ms),
        }
    }

    /// True once the recorded due time is reached.
    pub fn is_due(&self, now_ms: i64) -> bool {
        self.due_at_ms <= now_ms
    }

    /// Milliseconds past due, or 0 when not yet due.
    pub fn overdue_ms(&self, now_ms: i64) -> u64 {
        u64::try_from(now_ms.saturating_sub(self.due_at_ms)).unwrap_or(0)
    }

    /// Merge a restored wakeup with the goal the operator just supplied, so
    /// the interrupted work and the new instruction both reach the agent.
    /// An empty wakeup prompt restores to the operator's goal unchanged.
    pub fn restored_goal(&self, current_goal: &str) -> String {
        if self.prompt.trim().is_empty() {
            return current_goal.to_string();
        }
        format!(
            "[Restored wakeup] reason: {}\n\n{}\n\n---\n[Current loop goal]\n\n{}",
            self.reason, self.prompt, current_goal
        )
    }
}

/// Current wall-clock time in Unix milliseconds.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Path of the pending-wakeup file inside session directory `dir`.
pub fn path_in(dir: &Path) -> PathBuf {
    dir.join(WAKEUP_FILE_NAME)
}

/// Write `w` as the session's single pending wakeup, replacing any previous
/// one. Creates `dir` if it does not exist yet.
pub fn persist(dir: &Path, w: &PersistedWakeup) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|e| Error::Storage {
        message: format!("wakeup store: create {}: {e}", dir.display()),
    })?;
    let mut line = serde_json::to_string(w)?;
    line.push('\n');
    crate::atomic::atomic_write(&path_in(dir), line.as_bytes()).map_err(Error::Io)
}

/// Drop the session's pending wakeup. Missing file is not an error — the
/// loop clears unconditionally, and most session directories never had one.
pub fn clear(dir: &Path) -> Result<()> {
    match std::fs::remove_file(path_in(dir)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Read the session's pending wakeup, if any. Malformed or partial lines are
/// skipped; the last parseable line wins.
pub fn load(dir: &Path) -> Option<PersistedWakeup> {
    let text = std::fs::read_to_string(path_in(dir)).ok()?;
    parse_records(&text).pop()
}

/// Take the oldest *due* pending wakeup recorded anywhere in this
/// workspace's sessions, consuming it. Returns the session directory it came
/// from alongside the record, for logging.
///
/// Session directories held by a live process are skipped: their record
/// belongs to a loop that is still running (it is either still waiting, or
/// between firing and clearing the record), so consuming it here would make
/// this process run another loop's prompt and delete its state.
pub fn take_due_in_workspace(workspace: &Path, now_ms: i64) -> Option<(PathBuf, PersistedWakeup)> {
    let root = crate::paths::user_sessions_dir(workspace).ok()?;
    let slug_dir = root.join(crate::session::workspace_slug(workspace));

    let mut due = Vec::new();
    collect_due(&slug_dir, now_ms, 0, &mut due);
    due.sort_by_key(|(_, w)| w.due_at_ms);

    let (dir, w) = due.into_iter().next()?;
    if let Err(e) = std::fs::remove_file(path_in(&dir)) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                error = %e,
                dir = %dir.display(),
                "could not consume restored wakeup; it may be restored twice"
            );
        }
    }
    Some((dir, w))
}

fn parse_records(text: &str) -> Vec<PersistedWakeup> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn collect_due(dir: &Path, now_ms: i64, depth: u8, out: &mut Vec<(PathBuf, PersistedWakeup)>) {
    if depth > MAX_SCAN_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_due(&path, now_ms, depth + 1, out);
        } else if path.ends_with(WAKEUP_FILE_NAME) {
            if crate::session::locked_by_live_process(dir) {
                continue;
            }
            if let Some(w) = load(dir) {
                if w.is_due(now_ms) {
                    out.push((dir.to_path_buf(), w));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn persists_and_loads_round_trip() {
        let d = dir();
        let w = PersistedWakeup::new("poll build", "check ci", Duration::from_secs(30), 1_000);
        persist(d.path(), &w).expect("persist");
        assert_eq!(load(d.path()), Some(w));
    }

    #[test]
    fn due_at_is_scheduled_at_plus_delay() {
        let w = PersistedWakeup::new("r", "p", Duration::from_millis(2_500), 1_000);
        assert_eq!(w.scheduled_at_ms, 1_000);
        assert_eq!(w.due_at_ms, 3_500);
    }

    #[test]
    fn file_holds_exactly_one_line() {
        let d = dir();
        persist(
            d.path(),
            &PersistedWakeup::new("a", "b", Duration::from_secs(1), 0),
        )
        .unwrap();
        persist(
            d.path(),
            &PersistedWakeup::new("c", "d", Duration::from_secs(1), 0),
        )
        .unwrap();
        let text = std::fs::read_to_string(path_in(d.path())).unwrap();
        assert_eq!(text.lines().count(), 1, "overwrite, not append");
        assert_eq!(load(d.path()).unwrap().reason, "c");
    }

    #[test]
    fn load_without_file_is_none() {
        let d = dir();
        assert_eq!(load(d.path()), None);
        // ...and does not create anything.
        assert!(!path_in(d.path()).exists());
    }

    #[test]
    fn load_skips_malformed_lines() {
        let d = dir();
        std::fs::write(
            path_in(d.path()),
            "not json\n{\"reason\":\"ok\",\"prompt\":\"p\",\"scheduled_at_ms\":1,\"due_at_ms\":2}\n",
        )
        .unwrap();
        assert_eq!(load(d.path()).unwrap().reason, "ok");
    }

    #[test]
    fn clear_removes_the_record_and_tolerates_absence() {
        let d = dir();
        persist(
            d.path(),
            &PersistedWakeup::new("r", "p", Duration::from_secs(1), 0),
        )
        .unwrap();
        clear(d.path()).unwrap();
        assert!(!path_in(d.path()).exists());
        // Second clear (nothing to remove) must still be Ok.
        clear(d.path()).unwrap();
    }

    #[test]
    fn persist_creates_missing_directory() {
        let d = dir();
        let nested = d.path().join("slug").join("session-id");
        persist(
            &nested,
            &PersistedWakeup::new("r", "p", Duration::from_secs(1), 0),
        )
        .unwrap();
        assert!(load(&nested).is_some());
    }

    #[test]
    fn is_due_boundary_is_inclusive() {
        let w = PersistedWakeup::new("r", "p", Duration::from_secs(10), 1_000);
        assert!(!w.is_due(10_999));
        assert!(w.is_due(11_000), "exactly due counts as due");
        assert!(w.is_due(20_000));
    }

    #[test]
    fn overdue_ms_clamps_at_zero() {
        let w = PersistedWakeup::new("r", "p", Duration::from_secs(10), 1_000);
        assert_eq!(w.overdue_ms(5_000), 0, "not yet due");
        assert_eq!(w.overdue_ms(12_000), 1_000);
    }

    #[test]
    fn restored_goal_keeps_both_prompts() {
        let w = PersistedWakeup::new("poll", "check ci", Duration::from_secs(1), 0);
        let merged = w.restored_goal("review the PR");
        assert!(merged.contains("check ci"), "got: {merged}");
        assert!(merged.contains("review the PR"), "got: {merged}");
        assert!(
            merged.contains("poll"),
            "reason must survive; got: {merged}"
        );
    }

    #[test]
    fn restored_goal_with_empty_prompt_is_the_current_goal() {
        let w = PersistedWakeup::new("r", "", Duration::from_secs(1), 0);
        assert_eq!(w.restored_goal("review the PR"), "review the PR");
    }

    #[test]
    fn scan_returns_due_records_oldest_first_and_consumes_only_the_taken_one() {
        let root = dir();
        let slug = root.path().join("some-slug");
        let old = slug.join("s-old");
        let new = slug.join("s-new");
        let future = slug.join("s-future");
        let now = 1_000_000i64;
        // 10s overdue, 1s overdue, and not-yet-due.
        persist(
            &old,
            &PersistedWakeup::new("old", "p1", Duration::from_secs(1), now - 11_000),
        )
        .unwrap();
        persist(
            &new,
            &PersistedWakeup::new("new", "p2", Duration::from_secs(1), now - 2_000),
        )
        .unwrap();
        persist(
            &future,
            &PersistedWakeup::new("future", "p3", Duration::from_secs(600), now),
        )
        .unwrap();

        let mut due = Vec::new();
        collect_due(&slug, now, 0, &mut due);
        due.sort_by_key(|(_, w)| w.due_at_ms);
        let reasons: Vec<&str> = due.iter().map(|(_, w)| w.reason.as_str()).collect();
        assert_eq!(
            reasons,
            vec!["old", "new"],
            "future record must not be restored"
        );
        assert_eq!(due[0].0, old);
    }

    #[test]
    fn take_due_in_workspace_consumes_the_record() {
        let (workspace, _home, _guard) = workspace_with_sessions();
        let slug_dir = crate::paths::user_sessions_dir(&workspace)
            .unwrap()
            .join(crate::session::workspace_slug(&workspace));
        let session = slug_dir.join("2026-01-01T00-00-00Z-slug");
        let now = 5_000_000i64;
        persist(
            &session,
            &PersistedWakeup::new("poll", "check ci", Duration::from_secs(1), now - 60_000),
        )
        .unwrap();

        let (dir, w) = take_due_in_workspace(&workspace, now).expect("a due wakeup");
        assert_eq!(dir, session);
        assert_eq!(w.prompt, "check ci");
        assert!(
            !path_in(&session).exists(),
            "restoring must consume the record (at-most-once)"
        );
        assert!(
            take_due_in_workspace(&workspace, now).is_none(),
            "a consumed record must not be restored again"
        );
    }

    #[test]
    fn take_due_in_workspace_ignores_future_records() {
        let (workspace, _home, _guard) = workspace_with_sessions();
        let slug_dir = crate::paths::user_sessions_dir(&workspace)
            .unwrap()
            .join(crate::session::workspace_slug(&workspace));
        let session = slug_dir.join("2026-01-01T00-00-00Z-slug");
        let now = 5_000_000i64;
        persist(
            &session,
            &PersistedWakeup::new("poll", "check ci", Duration::from_secs(3_600), now),
        )
        .unwrap();
        assert!(take_due_in_workspace(&workspace, now).is_none());
        assert!(path_in(&session).exists(), "future record stays on disk");
    }

    #[test]
    fn take_due_in_workspace_skips_a_session_held_by_a_live_process() {
        let (workspace, _home, _guard) = workspace_with_sessions();
        let slug_dir = crate::paths::user_sessions_dir(&workspace)
            .unwrap()
            .join(crate::session::workspace_slug(&workspace));
        let session = slug_dir.join("2026-01-01T00-00-00Z-live");
        let now = 5_000_000i64;
        persist(
            &session,
            &PersistedWakeup::new("poll", "check ci", Duration::from_secs(1), now - 60_000),
        )
        .unwrap();

        // A record inside a directory whose owner is still running belongs to
        // that loop — consuming it here would run another loop's prompt and
        // delete its state.
        let live_sentinel = format!(
            "{}\n{}\n0\n",
            std::process::id(),
            crate::session::lifecycle::current_hostname()
        );
        std::fs::write(
            session.join(crate::session::lifecycle::SESSION_LOCK_FILE),
            live_sentinel,
        )
        .unwrap();
        assert!(
            take_due_in_workspace(&workspace, now).is_none(),
            "a live owner's record must not be stolen"
        );
        assert!(
            path_in(&session).exists(),
            "the record stays with its owner"
        );

        // Once the owner is gone the record is restorable again.
        std::fs::remove_file(session.join(crate::session::lifecycle::SESSION_LOCK_FILE)).unwrap();
        let (dir, w) = take_due_in_workspace(&workspace, now).expect("restorable after owner exit");
        assert_eq!(dir, session);
        assert_eq!(w.prompt, "check ci");
    }

    /// Pin `RECURSIVE_HOME` (and clear the `RECURSIVE_SESSIONS_DIR` hard
    /// override) so session paths resolve inside a tempdir.
    fn workspace_with_sessions() -> (
        PathBuf,
        tempfile::TempDir,
        crate::test_util::PinnedRecursiveHome,
    ) {
        let home = tempfile::tempdir().expect("home tempdir");
        let guard = crate::test_util::PinnedRecursiveHome::new(home.path());
        let workspace = home.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        (workspace, home, guard)
    }
}
