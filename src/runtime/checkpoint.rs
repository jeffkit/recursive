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
