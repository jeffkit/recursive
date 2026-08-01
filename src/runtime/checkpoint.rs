//! Checkpoint subsystem state for [`crate::runtime::AgentRuntime`].
//!
//! Kept in a child module so `runtime.rs` stays under the invariant #1
//! line budget. The struct is grouped session-scoped state shared by the
//! runtime and the checkpoint tools; fields are `pub(crate)` because the
//! runtime (parent module) reads/writes them directly.

use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};

use crate::checkpoint::ShadowRepo;
use crate::checkpoint_log::CheckpointLogWriter;
use crate::tools::TouchedFiles;

/// Checkpoint subsystem state, grouped to reduce field count on [`crate::runtime::AgentRuntime`].
///
/// When `shadow` and `session_id` are both `Some`, checkpoint tools are active.
/// When `None`, all checkpoint tools are unavailable.
///
/// **Goal 284**: automatic per-turn snapshots (pre + post) have been removed.
/// Checkpoints are now created only when the agent explicitly calls the
/// `checkpoint_save` tool.
pub(crate) struct CheckpointState {
    pub(crate) shadow: Option<Arc<ShadowRepo>>,
    pub(crate) session_id: Option<String>,
    /// 0-indexed turn counter. Shared with `checkpoint_save` tool via AtomicUsize.
    pub(crate) turn_index: Arc<AtomicUsize>,
    pub(crate) writer: Option<Arc<Mutex<CheckpointLogWriter>>>,
    pub(crate) touched_files: Option<Arc<Mutex<TouchedFiles>>>,
    /// Path to the `checkpoints.jsonl` log file for this session.
    pub(crate) log_path: Option<PathBuf>,
}

impl CheckpointState {
    pub(crate) fn disabled() -> Self {
        Self {
            shadow: None,
            session_id: None,
            turn_index: Arc::new(AtomicUsize::new(0)),
            writer: None,
            touched_files: None,
            log_path: None,
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.shadow.is_some() && self.session_id.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn turn_index_starts_at_zero_and_increments() {
        let state = CheckpointState::disabled();
        assert_eq!(state.turn_index.load(Ordering::Relaxed), 0);

        state.turn_index.fetch_add(1, Ordering::Relaxed);
        state.turn_index.fetch_add(1, Ordering::Relaxed);
        assert_eq!(state.turn_index.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn disabled_state_has_no_checkpoint_subsystem() {
        let state = CheckpointState::disabled();
        assert!(!state.enabled());
        assert!(state.shadow.is_none());
        assert!(state.session_id.is_none());
        assert!(state.writer.is_none());
        assert!(state.touched_files.is_none());
        assert!(state.log_path.is_none());
    }

    #[test]
    fn touched_files_lifecycle() {
        let state = CheckpointState::disabled();
        assert!(state.touched_files.is_none());

        // A checkpoint-enabled runtime owns a TouchedFiles observer shared
        // with the tool registry; the add/read/clear cycle must not lose
        // entries (pins accidental field-swaps in CheckpointState).
        let touched = Arc::new(Mutex::new(TouchedFiles::new()));
        let mut with_touched = CheckpointState::disabled();
        with_touched.touched_files = Some(touched.clone());

        {
            let mut guard = touched.lock().unwrap();
            guard.paths.insert("src/foo.rs".into());
            guard.paths.insert("Cargo.toml".into());
        }
        {
            let guard = touched.lock().unwrap();
            assert_eq!(guard.paths_sorted(), vec!["Cargo.toml", "src/foo.rs"]);
        }
        {
            let mut guard = touched.lock().unwrap();
            guard.paths.clear();
        }
        assert!(touched.lock().unwrap().is_empty());
    }
}
