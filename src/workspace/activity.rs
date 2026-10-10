//! Archive-admission capability seam (issue #135).
//!
//! Archiving a workspace is gated on "is anything still running here?". The
//! registry does not know how to answer that — turns, background jobs,
//! subagents, and scheduled wakeups all live with the running runtime, not with
//! the registry. So the registry asks an [`ActivityProbe`], and callers supply
//! one. This module ships a store-backed probe for the signals that are
//! observable from disk (active sessions and enabled scheduled triggers) and a
//! no-op probe for callers with nothing to ask.

use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::session::{SessionReader, SessionStatus};
use crate::triggers::TriggerStore;
use crate::workspace::registry::WorkspaceRecord;

/// The kind of work still running in a workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActiveWorkKind {
    /// A live (in-flight) session turn.
    Turn,
    /// A background job.
    Job,
    /// A spawned subagent / worker.
    Subagent,
    /// A scheduled wakeup (cron trigger) that is armed to fire.
    Schedule,
}

impl ActiveWorkKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ActiveWorkKind::Turn => "turn",
            ActiveWorkKind::Job => "job",
            ActiveWorkKind::Subagent => "subagent",
            ActiveWorkKind::Schedule => "schedule",
        }
    }
}

/// One piece of work still running in a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveWork {
    pub kind: ActiveWorkKind,
    /// Identity of the work item (a session id, job id, or trigger id).
    pub id: String,
    /// Human-readable detail for admission messages.
    pub detail: String,
}

/// Answers "what is still running here?" for the archive gate.
pub trait ActivityProbe: Send + Sync {
    /// Enumerate work still running for `record`. An empty vector means the
    /// workspace is idle and may be archived.
    fn active_work(&self, record: &WorkspaceRecord) -> Vec<ActiveWork>;

    /// Ask the reported work to stop. Called only *after* the archive write is
    /// durable, so a crash in between leaves the workspace archived rather than
    /// "stopped but not archived".
    ///
    /// Returns the subset of `work` that was actually stopped. A probe that
    /// cannot reach a piece of work (a live turn's stop handle lives with the
    /// running runtime) must leave it out rather than claim it, so the caller
    /// can report the rest as still running. Best-effort: implementations must
    /// not panic.
    fn stop(&self, record: &WorkspaceRecord, work: &[ActiveWork]) -> Vec<ActiveWork>;
}

/// A probe that reports nothing running. Use when there is no runtime to ask
/// (e.g. a CLI archiving an idle workspace).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoActivityProbe;

impl ActivityProbe for NoActivityProbe {
    fn active_work(&self, _record: &WorkspaceRecord) -> Vec<ActiveWork> {
        Vec::new()
    }

    fn stop(&self, _record: &WorkspaceRecord, _work: &[ActiveWork]) -> Vec<ActiveWork> {
        Vec::new()
    }
}

/// A probe over the on-disk signals: sessions whose status is `Active` (a turn
/// that has not been finalised) and enabled cron triggers (scheduled wakeups
/// that would fire into this workspace).
///
/// Jobs and subagents are in-process and have no on-disk registry, so a caller
/// that owns the runtime should wrap this probe and append those. `stop` can
/// disarm scheduled wakeups and does so; a live turn's stop handle lives with
/// the runtime, so turns are reported here but not force-killed — `stop`
/// returns only the disarmed schedules.
///
/// Session discovery goes through [`crate::paths::user_sessions_dir`], which
/// honours `RECURSIVE_SESSIONS_DIR` as a *hard* override ignoring the
/// workspace. In an environment where that variable is set, every workspace
/// therefore reports the override directory's live sessions as its own.
#[derive(Debug, Default, Clone, Copy)]
pub struct StoreActivityProbe;

impl ActivityProbe for StoreActivityProbe {
    fn active_work(&self, record: &WorkspaceRecord) -> Vec<ActiveWork> {
        let root = Path::new(&record.id);
        let mut work = Vec::new();

        if let Ok(dirs) = SessionReader::list_sessions(root) {
            for dir in dirs {
                if let Ok(meta) = SessionReader::load_meta(&dir) {
                    if meta.status == SessionStatus::Active {
                        work.push(ActiveWork {
                            kind: ActiveWorkKind::Turn,
                            id: meta.session_id.clone(),
                            detail: meta
                                .last_prompt
                                .clone()
                                .unwrap_or_else(|| meta.goal.clone()),
                        });
                    }
                }
            }
        }

        if let Ok(triggers) = TriggerStore::for_workspace(root).load() {
            for trigger in triggers {
                if trigger.enabled {
                    work.push(ActiveWork {
                        kind: ActiveWorkKind::Schedule,
                        id: trigger.id.clone(),
                        detail: trigger.goal.clone(),
                    });
                }
            }
        }

        work
    }

    fn stop(&self, record: &WorkspaceRecord, work: &[ActiveWork]) -> Vec<ActiveWork> {
        let root = Path::new(&record.id);
        let scheduled: BTreeSet<&str> = work
            .iter()
            .filter(|w| w.kind == ActiveWorkKind::Schedule)
            .map(|w| w.id.as_str())
            .collect();
        if scheduled.is_empty() {
            return Vec::new();
        }
        let store = TriggerStore::for_workspace(root);
        let Ok(mut triggers) = store.load() else {
            return Vec::new();
        };
        let mut disarmed: BTreeSet<String> = BTreeSet::new();
        for trigger in triggers.iter_mut() {
            if trigger.enabled && scheduled.contains(trigger.id.as_str()) {
                trigger.enabled = false;
                disarmed.insert(trigger.id.clone());
            }
        }
        if disarmed.is_empty() {
            return Vec::new();
        }
        if store.save(&triggers).is_err() {
            // Nothing reached the disk, so nothing was stopped.
            return Vec::new();
        }
        work.iter()
            .filter(|w| w.kind == ActiveWorkKind::Schedule && disarmed.contains(&w.id))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::PinnedRecursiveHome;
    use crate::triggers::{Trigger, TriggerSpec};

    fn record(root: &Path) -> WorkspaceRecord {
        WorkspaceRecord {
            id: root.to_string_lossy().into_owned(),
            display: root.to_string_lossy().into_owned(),
            name: "ws".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            archived: false,
            sessions: Vec::new(),
        }
    }

    /// Write an `Active` session header into the workspace's session store.
    fn write_active_session(root: &Path, id: &str) {
        let session_dir = crate::paths::user_sessions_dir(root)
            .unwrap()
            .join("slug")
            .join(id);
        std::fs::create_dir_all(&session_dir).unwrap();
        let meta = crate::session::SessionMeta {
            schema_version: 1,
            session_id: id.into(),
            goal: "g".into(),
            model: "m".into(),
            provider: "p".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            message_count: 1,
            status: SessionStatus::Active,
            tool_registry_hash: None,
            first_prompt: None,
            last_prompt: Some("working".into()),
            cost: None,
            preset: None,
            name: None,
            derived_from: None,
            finish_reason: None,
            error: None,
            owner: None,
            tenant: None,
        };
        std::fs::write(
            session_dir.join(".meta.json"),
            serde_json::to_vec(&meta).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn no_activity_probe_reports_nothing() {
        let ws = tempfile::tempdir().unwrap();
        let probe = NoActivityProbe;
        assert!(probe.active_work(&record(ws.path())).is_empty());
        assert!(probe.stop(&record(ws.path()), &[]).is_empty());
    }

    #[test]
    fn store_activity_probe_reports_enabled_triggers() {
        let home = tempfile::tempdir().unwrap();
        let _pin = PinnedRecursiveHome::new(home.path());
        let ws = tempfile::tempdir().unwrap();
        let store = TriggerStore::for_workspace(ws.path());

        let mut armed = Trigger::new(
            "t-1",
            TriggerSpec::Cron {
                expr: "* * * * *".into(),
            },
            "wake up",
            None,
            None,
        );
        armed.enabled = true;
        let mut idle = Trigger::new(
            "t-2",
            TriggerSpec::Cron {
                expr: "* * * * *".into(),
            },
            "disabled",
            None,
            None,
        );
        idle.enabled = false;
        store.save(&[armed, idle]).unwrap();

        let probe = StoreActivityProbe;
        let rec = record(ws.path());
        let work = probe.active_work(&rec);
        assert_eq!(work.len(), 1);
        assert_eq!(work[0].kind, ActiveWorkKind::Schedule);
        assert_eq!(work[0].id, "t-1");

        // stop disarms the scheduled work and says so.
        let stopped = probe.stop(&rec, &work);
        assert_eq!(stopped.len(), 1);
        assert_eq!(stopped[0].kind, ActiveWorkKind::Schedule);
        assert_eq!(stopped[0].id, "t-1");
        let remaining = TriggerStore::for_workspace(ws.path()).load().unwrap();
        assert!(remaining.iter().all(|t| !t.enabled));
    }

    #[test]
    fn store_activity_probe_stop_reports_only_disarmed_schedules() {
        let home = tempfile::tempdir().unwrap();
        let _pin = PinnedRecursiveHome::new(home.path());
        let ws = tempfile::tempdir().unwrap();
        let mut armed = Trigger::new(
            "t-1",
            TriggerSpec::Cron {
                expr: "* * * * *".into(),
            },
            "wake up",
            None,
            None,
        );
        armed.enabled = true;
        TriggerStore::for_workspace(ws.path())
            .save(&[armed])
            .unwrap();
        write_active_session(ws.path(), "sess-9");

        let probe = StoreActivityProbe;
        let rec = record(ws.path());
        let work = probe.active_work(&rec);
        assert_eq!(work.len(), 2, "turn + schedule: {work:?}");

        let stopped = probe.stop(&rec, &work);
        assert_eq!(
            stopped.len(),
            1,
            "a live turn is not stopped by a disk-only probe: {stopped:?}"
        );
        assert_eq!(stopped[0].kind, ActiveWorkKind::Schedule);
        assert_eq!(stopped[0].id, "t-1");
    }

    #[test]
    fn store_activity_probe_stop_never_claims_a_turn_was_stopped() {
        let home = tempfile::tempdir().unwrap();
        let _pin = PinnedRecursiveHome::new(home.path());
        let ws = tempfile::tempdir().unwrap();
        write_active_session(ws.path(), "sess-9");

        let rec = record(ws.path());
        let work = StoreActivityProbe.active_work(&rec);
        assert_eq!(work.len(), 1);
        assert_eq!(work[0].kind, ActiveWorkKind::Turn);
        assert!(StoreActivityProbe.stop(&rec, &work).is_empty());
    }

    #[test]
    fn store_activity_probe_reports_active_sessions() {
        let home = tempfile::tempdir().unwrap();
        let _pin = PinnedRecursiveHome::new(home.path());
        let ws = tempfile::tempdir().unwrap();
        write_active_session(ws.path(), "sess-9");

        let work = StoreActivityProbe.active_work(&record(ws.path()));
        assert_eq!(work.len(), 1);
        assert_eq!(work[0].kind, ActiveWorkKind::Turn);
        assert_eq!(work[0].id, "sess-9");
    }
}
