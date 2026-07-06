//! In-memory background-task registry.
//!
//! # Design
//!
//! When the `agent` tool is called with `run_in_background: true`, the
//! work is spawned into a `tokio::task` and a `TaskId` is returned
//! immediately.  The `TaskRegistry` holds:
//!
//! - the live `JoinHandle` (so `task_stop` can cancel),
//! - the captured `TaskStatus` (so `task_get` / `task_list` can report),
//! - the output buffer (so `task_output` can stream partial results).
//!
//! This is **in-memory only** by design.  Tasks do not survive process
//! restart.  If a task is still running when the process exits, its
//! output is lost.  The spec explicitly calls this out as acceptable
//! for Phase D.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio::task::JoinHandle;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// TaskStatus
// ---------------------------------------------------------------------------

/// Lifecycle status of a background task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Task is queued or actively running.
    Running,
    /// Task completed successfully.
    Completed,
    /// Task exited with an error.
    Failed,
    /// Task was explicitly stopped by `task_stop`.
    Stopped,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Running => "running",
            TaskStatus::Completed => "completed",
            TaskStatus::Failed => "failed",
            TaskStatus::Stopped => "stopped",
        }
    }

    /// True if the task will never change state again.
    pub fn is_terminal(self) -> bool {
        !matches!(self, TaskStatus::Running)
    }
}

// ---------------------------------------------------------------------------
// TaskState
// ---------------------------------------------------------------------------

/// Full state of a single background task.
#[derive(Debug)]
pub struct TaskState {
    /// Stable opaque task ID.  Use this for `task_get`, `task_stop`, etc.
    pub id: TaskId,
    /// Human-readable description (e.g. the goal the agent was given).
    pub description: String,
    /// Team this task belongs to, if any.  Empty string means "no team".
    pub team: String,
    /// Teammate name within the team (empty if not a teammate).
    pub name: String,
    /// When the task was started.
    pub started_at: DateTime<Utc>,
    /// Current status. Behind a Mutex for `&self` mutation through Arc.
    status: Mutex<TaskStatus>,
    /// Output buffer: anything the inner agent has printed so far.
    /// Pushed to from the inner agent via `output_tx`.
    /// Behind a Mutex so callers can append through `&self` (since the
    /// state is typically held inside an `Arc<TaskState>`).
    pub(crate) output: Mutex<Vec<String>>,
    /// Channel sender for the inner agent to push more output.
    pub output_tx: mpsc::UnboundedSender<String>,
    /// Receiver side, held here so the registry can drain output.
    output_rx: Mutex<Option<mpsc::UnboundedReceiver<String>>>,
    /// Tokio handle — used to await completion or cancel.
    handle: Mutex<Option<JoinHandle<()>>>,
    /// Set when a final result (Ok or Err) is captured.  Read by
    /// `task_get` / `task_output` and reported as the task's terminal
    /// status.  Held in an Option so the registry can record the
    /// outcome even after the JoinHandle finishes.
    pub final_result: Mutex<Option<Result<String, String>>>,
}

impl TaskState {
    /// Create a new in-memory task state.  The caller must call
    /// `set_handle` to attach the JoinHandle once the task is spawned.
    pub fn new(
        description: impl Into<String>,
        team: impl Into<String>,
        name: impl Into<String>,
    ) -> (Self, TaskId) {
        let id = TaskId::new();
        let (tx, rx) = mpsc::unbounded_channel();
        let state = Self {
            id: id.clone(),
            description: description.into(),
            team: team.into(),
            name: name.into(),
            started_at: Utc::now(),
            status: Mutex::new(TaskStatus::Running),
            output: Mutex::new(Vec::new()),
            output_tx: tx,
            output_rx: Mutex::new(Some(rx)),
            handle: Mutex::new(None),
            final_result: Mutex::new(None),
        };
        (state, id)
    }

    /// Attach the JoinHandle for cancellation / awaiting.
    pub async fn set_handle(&self, handle: JoinHandle<()>) {
        *self.handle.lock().await = Some(handle);
    }

    /// Read the current status (cloned).
    pub async fn status(&self) -> TaskStatus {
        *self.status.lock().await
    }

    /// Append a single output line to the buffer.
    pub async fn append_output(&self, line: String) {
        self.output.lock().await.push(line);
    }

    /// Read a snapshot of the output buffer.
    pub async fn output_snapshot(&self) -> Vec<String> {
        self.output.lock().await.clone()
    }

    /// Drain any pending output from the inner channel into `self.output`.
    /// Returns the number of new lines drained.
    pub async fn drain_output(&self) -> usize {
        let mut rx_guard = self.output_rx.lock().await;
        let Some(rx) = rx_guard.as_mut() else {
            return 0;
        };
        let mut count = 0;
        // try_recv loop — don't await; we want non-blocking.
        let mut out = self.output.lock().await;
        loop {
            match rx.try_recv() {
                Ok(line) => {
                    out.push(line);
                    count += 1;
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    // Sender dropped — close the receiver so we don't try again.
                    *rx_guard = None;
                    break;
                }
            }
        }
        count
    }

    /// Cancel the task.  Returns `true` if a running handle was found
    /// and aborted.
    pub async fn stop(&self) -> bool {
        let h = self.handle.lock().await;
        if let Some(handle) = h.as_ref() {
            handle.abort();
            *self.status.lock().await = TaskStatus::Stopped;
            true
        } else {
            false
        }
    }

    /// Mark the task as completed with the given output text.
    pub async fn mark_completed(&self, output: String) {
        *self.status.lock().await = TaskStatus::Completed;
        *self.final_result.lock().await = Some(Ok(output));
    }

    /// Mark the task as failed.
    pub async fn mark_failed(&self, err: String) {
        *self.status.lock().await = TaskStatus::Failed;
        *self.final_result.lock().await = Some(Err(err));
    }
}

// ---------------------------------------------------------------------------
// TaskId
// ---------------------------------------------------------------------------

/// Opaque task ID.  Wrapped so callers can't accidentally pass a random
/// `String` where a task ID is expected.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(pub String);

impl TaskId {
    pub fn new() -> Self {
        Self(format!("task-{}", Uuid::new_v4()))
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// TaskRegistry
// ---------------------------------------------------------------------------

/// Process-wide registry of in-flight background tasks.
///
/// Cloneable, backed by `Arc<RwLock<…>>`.  All methods are safe to call
/// concurrently.
#[derive(Clone, Default)]
pub struct TaskRegistry {
    inner: Arc<RwLock<TaskRegistryInner>>,
}

#[derive(Default)]
struct TaskRegistryInner {
    tasks: HashMap<TaskId, Arc<TaskState>>,
    /// Track order of insertion so `task_list` returns tasks in a
    /// stable, predictable order.
    order: Vec<TaskId>,
}

impl TaskRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new task.  Returns a clone of the `Arc<TaskState>`
    /// so callers can write to its output channel.
    pub async fn register(&self, state: TaskState) -> Arc<TaskState> {
        let id = state.id.clone();
        let state = Arc::new(state);
        let mut inner = self.inner.write().await;
        inner.order.push(id.clone());
        inner.tasks.insert(id, state.clone());
        state
    }

    /// Get a task by ID.
    pub async fn get(&self, id: &TaskId) -> Option<Arc<TaskState>> {
        self.inner.read().await.tasks.get(id).cloned()
    }

    /// List all known tasks in insertion order.
    pub async fn list(&self) -> Vec<Arc<TaskState>> {
        let inner = self.inner.read().await;
        inner
            .order
            .iter()
            .filter_map(|id| inner.tasks.get(id).cloned())
            .collect()
    }

    /// Stop a task by ID.  Returns `true` if a running task was found
    /// and aborted.
    pub async fn stop(&self, id: &TaskId) -> bool {
        match self.get(id).await {
            Some(t) => t.stop().await,
            None => false,
        }
    }

    /// Drain output for a single task.
    pub async fn drain_output(&self, id: &TaskId) -> usize {
        match self.get(id).await {
            Some(t) => t.drain_output().await,
            None => 0,
        }
    }

    /// Append a single output line for a task.
    pub async fn append_output(&self, id: &TaskId, line: String) -> bool {
        match self.get(id).await {
            Some(t) => {
                t.append_output(line).await;
                true
            }
            None => false,
        }
    }

    /// Drain output for *all* known tasks.  Useful after waking up to
    /// propagate new state.
    pub async fn drain_all(&self) -> usize {
        let tasks = self.list().await;
        let mut total = 0;
        for t in tasks {
            total += t.drain_output().await;
        }
        total
    }

    /// Number of tasks currently tracked.
    pub async fn len(&self) -> usize {
        self.inner.read().await.tasks.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.inner.read().await.tasks.is_empty()
    }

    /// Remove a task from the registry.  Used by `task_stop` to clean
    /// up after cancellation.  Note: the task state may still be alive
    /// (e.g. if a clone of the `Arc` is held elsewhere), but it will
    /// be detached from lookups.
    pub async fn forget(&self, id: &TaskId) -> bool {
        let mut inner = self.inner.write().await;
        let removed = inner.tasks.remove(id).is_some();
        inner.order.retain(|tid| tid != id);
        removed
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn task_id_unique_and_format() {
        let a = TaskId::new();
        let b = TaskId::new();
        assert_ne!(a, b);
        assert!(a.0.starts_with("task-"));
        assert!(a.to_string().starts_with("task-"));
    }

    #[test]
    fn task_status_as_str_matches_serde() {
        for s in [
            TaskStatus::Running,
            TaskStatus::Completed,
            TaskStatus::Failed,
            TaskStatus::Stopped,
        ] {
            let j = serde_json::to_string(&s).unwrap();
            let back: TaskStatus = serde_json::from_str(&j).unwrap();
            assert_eq!(s, back);
        }
    }

    #[test]
    fn task_status_is_terminal() {
        assert!(!TaskStatus::Running.is_terminal());
        assert!(TaskStatus::Completed.is_terminal());
        assert!(TaskStatus::Failed.is_terminal());
        assert!(TaskStatus::Stopped.is_terminal());
    }

    #[tokio::test]
    async fn register_list_get() {
        let reg = TaskRegistry::new();
        let (state, id) = TaskState::new("test task", "alpha", "researcher");
        let _ = reg.register(state).await;
        assert_eq!(reg.len().await, 1);

        let listed = reg.list().await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);

        let got = reg.get(&id).await.unwrap();
        assert_eq!(got.description, "test task");
        assert_eq!(got.team, "alpha");
        assert_eq!(got.name, "researcher");
        assert_eq!(got.status().await, TaskStatus::Running);
    }

    #[tokio::test]
    async fn drain_output_collects_lines() {
        let reg = TaskRegistry::new();
        let (state, id) = TaskState::new("t", "", "");
        let arc = reg.register(state).await;

        arc.output_tx.send("line 1".into()).unwrap();
        arc.output_tx.send("line 2".into()).unwrap();
        // Drop the Arc (and the contained sender) so the receiver
        // sees a disconnected channel after the next drain.
        drop(arc);

        let drained = reg.drain_output(&id).await;
        assert_eq!(drained, 2);
        let got = reg.get(&id).await.unwrap();
        assert_eq!(got.output_snapshot().await, vec!["line 1", "line 2"]);

        // Second drain is a no-op.
        assert_eq!(reg.drain_output(&id).await, 0);
    }

    #[tokio::test]
    async fn append_output_via_registry() {
        let reg = TaskRegistry::new();
        let (state, id) = TaskState::new("t", "", "");
        let _ = reg.register(state).await;
        assert!(reg.append_output(&id, "x".into()).await);
        assert!(!reg.append_output(&TaskId("ghost".into()), "y".into()).await);
        let task = reg.get(&id).await.unwrap();
        assert_eq!(task.output_snapshot().await, vec!["x"]);
    }

    #[tokio::test]
    async fn mark_completed_and_failed() {
        let reg = TaskRegistry::new();
        let (state, id) = TaskState::new("t", "", "");
        let _ = reg.register(state).await;

        reg.get(&id)
            .await
            .unwrap()
            .mark_completed("done".into())
            .await;
        let got = reg.get(&id).await.unwrap();
        assert_eq!(got.status().await, TaskStatus::Completed);
        let r = got.final_result.lock().await.clone();
        assert_eq!(r, Some(Ok("done".into())));

        reg.get(&id).await.unwrap().mark_failed("oops".into()).await;
        let got = reg.get(&id).await.unwrap();
        assert_eq!(got.status().await, TaskStatus::Failed);
        let r = got.final_result.lock().await.clone();
        assert_eq!(r, Some(Err("oops".into())));
    }

    #[tokio::test]
    async fn stop_cancels_join_handle() {
        let reg = TaskRegistry::new();
        let (state, id) = TaskState::new("t", "", "");
        let arc = reg.register(state).await;

        let handle = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        arc.set_handle(handle).await;

        assert!(reg.stop(&id).await);
        let got = reg.get(&id).await.unwrap();
        assert_eq!(got.status().await, TaskStatus::Stopped);

        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    #[tokio::test]
    async fn forget_removes_from_lookup() {
        let reg = TaskRegistry::new();
        let (state, id) = TaskState::new("t", "", "");
        let _ = reg.register(state).await;
        assert!(reg.forget(&id).await);
        assert!(reg.get(&id).await.is_none());
        assert_eq!(reg.len().await, 0);
        assert!(!reg.forget(&id).await);
    }

    // ── Gap-filling tests ────────────────────────────────────────────────────

    #[test]
    fn task_status_as_str_returns_correct_strings() {
        assert_eq!(TaskStatus::Running.as_str(), "running");
        assert_eq!(TaskStatus::Completed.as_str(), "completed");
        assert_eq!(TaskStatus::Failed.as_str(), "failed");
        assert_eq!(TaskStatus::Stopped.as_str(), "stopped");
    }

    #[test]
    fn task_id_display_format() {
        let id = TaskId("task-abc123".into());
        assert_eq!(id.to_string(), "task-abc123");
        assert_eq!(format!("{id}"), "task-abc123");
    }

    #[tokio::test]
    async fn drain_output_returns_correct_count() {
        let reg = TaskRegistry::new();
        let (state, id) = TaskState::new("t", "", "");
        let arc = reg.register(state).await;

        // Append 3 lines via channel
        arc.output_tx.send("a".into()).unwrap();
        arc.output_tx.send("b".into()).unwrap();
        arc.output_tx.send("c".into()).unwrap();
        drop(arc);

        let count = reg.drain_output(&id).await;
        assert_eq!(
            count, 3,
            "drain must return the exact number of lines drained"
        );

        // A second drain with no new lines returns 0
        assert_eq!(reg.drain_output(&id).await, 0);
    }

    #[tokio::test]
    async fn stop_without_handle_returns_false() {
        let reg = TaskRegistry::new();
        let (state, id) = TaskState::new("t", "", "");
        let _ = reg.register(state).await;

        // No handle set — stop returns false (nothing to cancel)
        let stopped = reg.stop(&id).await;
        assert!(!stopped, "stop without handle must return false");
    }

    #[tokio::test]
    async fn task_state_output_snapshot_empty_initially() {
        let reg = TaskRegistry::new();
        let (state, id) = TaskState::new("task", "team", "name");
        let _ = reg.register(state).await;
        let got = reg.get(&id).await.unwrap();
        let snap = got.output_snapshot().await;
        assert!(snap.is_empty(), "output must be empty before any drain");
    }

    #[tokio::test]
    async fn registry_is_empty_initially_and_nonempty_after_register() {
        // kills `is_empty` → always-true / always-false mutations
        let reg = TaskRegistry::new();
        assert!(reg.is_empty().await, "new registry must be empty");
        let (state, _id) = TaskState::new("t", "", "");
        reg.register(state).await;
        assert!(
            !reg.is_empty().await,
            "registry must not be empty after register"
        );
    }

    #[tokio::test]
    async fn drain_all_drains_multiple_tasks() {
        // kills `drain_all` function-level replacement
        // Note: drain_output reads from output_tx channel, not the direct buffer,
        // so we send via output_tx to test drain_all correctly.
        let reg = TaskRegistry::new();
        let (s1, _id1) = TaskState::new("t1", "", "");
        let (s2, _id2) = TaskState::new("t2", "", "");
        // Send via the channel before registering so we can keep tx
        let tx1 = s1.output_tx.clone();
        let tx2 = s2.output_tx.clone();
        reg.register(s1).await;
        reg.register(s2).await;
        tx1.send("a".into()).unwrap();
        tx2.send("b".into()).unwrap();
        // Give the channel messages time to arrive
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let total = reg.drain_all().await;
        assert_eq!(
            total, 2,
            "drain_all must return total lines drained across tasks"
        );
    }

    #[tokio::test]
    async fn drain_output_returns_zero_for_missing_task() {
        // kills `None => 0` → `None => 1` mutation in drain_output
        let reg = TaskRegistry::new();
        let ghost_id = TaskId::new();
        assert_eq!(
            reg.drain_output(&ghost_id).await,
            0,
            "drain_output must return 0 for missing task"
        );
    }

    #[tokio::test]
    async fn append_output_returns_false_for_missing_task() {
        // kills `None => false` → `None => true` mutation in append_output
        let reg = TaskRegistry::new();
        let ghost_id = TaskId::new();
        let ok = reg.append_output(&ghost_id, "line".into()).await;
        assert!(!ok, "append_output must return false for missing task");
    }
}
