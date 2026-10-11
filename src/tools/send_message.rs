//! `send_message` tool — coordinator ↔ worker (or task) messaging.
//!
//! # Resolution order
//!
//! Callers can address the target in two ways:
//! 1. **`task_id`** — preferred, used when the worker was spawned via the
//!    Phase D `agent` tool with `team_name+name` (or `run_in_background`).
//!    The message is pushed into the task's output channel, which the
//!    worker's prompt loop can poll between steps (or, more practically,
//!    which the human can read on the next `task_output` call). For a
//!    background worker this also drives a new turn on its retained runtime.
//!    If the task is registered but no live continuation handle exists (the
//!    worker exited or was stopped), this is an **error** — never a
//!    "delivered" success (issue #176).
//! 2. **`worker_id`** — legacy, addresses a worker registered in
//!    `WorkerRegistry` (from pre-Phase D `spawn_worker`), or a background
//!    worker by its `WorkerTable` key.
//!
//! If both are present, `task_id` wins.  If neither resolves, the tool
//! returns a helpful message listing active workers and tasks.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::tasks::TaskRegistry;
use crate::tools::{Tool, ToolSideEffect};

// ---------------------------------------------------------------------------
// WorkerMailbox
// ---------------------------------------------------------------------------

/// A FIFO queue of pending messages for a single worker agent.
///
/// The coordinator pushes messages here; the worker's kernel drains them
/// between steps.
///
/// The queue alone carries no "peer is gone" signal (a `VecDeque` behind a
/// mutex has no close state), so a `done` flag travels alongside it: the
/// worker (or, on the abort path, the parallel aggregator) flips it at the
/// worker's terminal state, and `WorkerRegistry::register` uses it to sweep
/// stale entries (goal-408). `Clone` shares the flag (and the queue), so every
/// handle to the same mailbox observes the same liveness bit.
#[derive(Clone, Default)]
pub struct WorkerMailbox {
    queue: Arc<tokio::sync::Mutex<VecDeque<String>>>,
    done: Arc<AtomicBool>,
}

impl WorkerMailbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark this worker as finished (normal exit, cut-off, or abort).
    ///
    /// Only an `AtomicBool::store`, so it is safe to call from a `Drop` guard
    /// and is idempotent — a mailbox may be marked by both the worker's own
    /// exit guard and the parallel aggregator that aborted it.
    pub fn mark_done(&self) {
        self.done.store(true, Ordering::Release);
    }

    /// Whether this worker has reached a terminal state.
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    /// Push a message into the mailbox.
    pub async fn push(&self, msg: String) {
        self.queue.lock().await.push_back(msg);
    }

    /// Pop the oldest pending message, or `None` if empty.
    pub async fn pop(&self) -> Option<String> {
        self.queue.lock().await.pop_front()
    }

    /// Drain all pending messages into a Vec.
    pub async fn drain_all(&self) -> Vec<String> {
        let mut q = self.queue.lock().await;
        q.drain(..).collect()
    }

    /// Return the number of pending messages without consuming them.
    pub async fn len(&self) -> usize {
        self.queue.lock().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.queue.lock().await.is_empty()
    }
}

// ---------------------------------------------------------------------------
// WorkerRegistry
// ---------------------------------------------------------------------------

/// Global registry mapping `worker_id` → `WorkerMailbox`.
#[derive(Clone, Default)]
pub struct WorkerRegistry {
    inner: Arc<RwLock<std::collections::HashMap<String, WorkerMailbox>>>,
}

impl WorkerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new worker and return its mailbox.
    ///
    /// While holding the write lock this also sweeps entries whose mailbox is
    /// already marked done: `deregister_sync` hands off to `try_write` and
    /// gives up on contention (Drop context — no waiting allowed), so a
    /// finished worker's entry can linger until the next registration. The
    /// sweep runs here rather than on a timer so the registry self-heals
    /// lazily, with zero standing cost (goal-408).
    pub async fn register(&self, worker_id: &str) -> WorkerMailbox {
        let mailbox = WorkerMailbox::new();
        let mut guard = self.inner.write().await;
        guard.retain(|_, existing| !existing.is_done());
        guard.insert(worker_id.to_string(), mailbox.clone());
        mailbox
    }

    /// Deregister a worker (called when it finishes).
    pub async fn deregister(&self, worker_id: &str) {
        self.inner.write().await.remove(worker_id);
    }

    /// Synchronous deregister for use from `Drop` (e.g. the parallel-mode
    /// abort path of issue #40, where the async deregister after the run
    /// would be skipped by the task abort). Uses `try_write`; on contention
    /// the entry is left behind, and is swept by the next
    /// [`WorkerRegistry::register`] (it must be marked done for that to
    /// happen) — acceptable because the table is best-effort presence info.
    pub fn deregister_sync(&self, worker_id: &str) {
        if let Ok(mut guard) = self.inner.try_write() {
            guard.remove(worker_id);
        }
    }

    /// Get the mailbox for a worker, or `None` if not registered.
    pub async fn get(&self, worker_id: &str) -> Option<WorkerMailbox> {
        self.inner.read().await.get(worker_id).cloned()
    }

    /// List active worker IDs.
    pub async fn active_workers(&self) -> Vec<String> {
        self.inner.read().await.keys().cloned().collect()
    }
}

// ---------------------------------------------------------------------------
// SendMessageTool
// ---------------------------------------------------------------------------

/// The `send_message` tool — push a follow-up message to a running
/// worker (task or legacy worker).
pub struct SendMessageTool {
    registry: WorkerRegistry,
    task_registry: Arc<TaskRegistry>,
    /// Background worker continuation table. When a message targets a
    /// task_id whose worker is registered here, the message is pushed onto
    /// the worker's continuation channel, driving a new turn on its
    /// long-lived runtime (cross-turn continuation).
    workers: crate::tools::agent::WorkerTable,
}

impl SendMessageTool {
    pub fn new(
        registry: WorkerRegistry,
        task_registry: Arc<TaskRegistry>,
        workers: crate::tools::agent::WorkerTable,
    ) -> Self {
        Self {
            registry,
            task_registry,
            workers,
        }
    }
}

#[async_trait]
impl Tool for SendMessageTool {
    fn is_deferred(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "send_message".into(),
            description: concat!(
                "Send a follow-up message to a running worker. Two addressing ",
                "modes: 'task_id' (preferred, returned by task_create / agent ",
                "tool) or 'worker_id' (legacy, from spawn_worker).  Messages ",
                "are queued and delivered to the worker on its next step."
            )
            .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "The task ID (preferred, returned by task_create or the agent tool when run_in_background=true or team_name+name are set)."
                    },
                    "worker_id": {
                        "type": "string",
                        "description": "Legacy: the worker ID returned by spawn_worker."
                    },
                    "message": {
                        "type": "string",
                        "description": "The message to send to the worker."
                    }
                },
                "required": ["message"]
            }),
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        // Pushing a message into a worker mailbox mutates shared external state.
        ToolSideEffect::External
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let message = arguments["message"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "send_message".into(),
                message: "missing required parameter: message".to_string(),
            })?
            .to_string();

        // Prefer task_id (Phase D).
        if let Some(task_id_str) = arguments.get("task_id").and_then(|v| v.as_str()) {
            let task_id = crate::tasks::TaskId(task_id_str.to_string());
            let task = self
                .task_registry
                .get(&task_id)
                .await
                .ok_or_else(|| Error::NotFound(format!("task '{task_id_str}'")))?;
            // Use the public output_tx so the worker (or a human reading
            // task_output) can see the message.
            let _ = task.output_tx.send(message.clone());

            // If this task is a background worker, drive a new turn by
            // pushing the message onto the worker's continuation channel.
            // The worker runtime is retained across turns, so this continues
            // the same conversation (transcript preserved).
            //
            // Issue #176: "no handle" must NOT be reported as success. The
            // task registry and the worker table are separate records — a
            // task can outlive its continuation handle (the worker exited,
            // its deregister guard ran, or `task_stop` aborted it) — and the
            // old fall-through answered `Message delivered to task ...` while
            // nothing would ever run the message. That silent false success
            // is exactly what makes a caller believe a dead worker is working.
            {
                let workers = self.workers.lock().unwrap_or_else(|e| e.into_inner());
                for handle in workers.values() {
                    if handle.task_id == task_id {
                        match handle.tx.send(message.clone()) {
                            Ok(()) => {
                                return Ok(format!(
                                    "Message delivered to task '{task_id_str}'; worker will run it as a new turn."
                                ));
                            }
                            Err(_) => {
                                return Err(Error::ToolRejected {
                                    name: "send_message".into(),
                                    reason: format!(
                                        "task '{task_id_str}' has no live worker: its continuation \
                                         channel is closed, so the message was not delivered and no \
                                         turn will run (the message was written to task output only)"
                                    ),
                                });
                            }
                        }
                    }
                }
            }
            return Err(Error::ToolRejected {
                name: "send_message".into(),
                reason: format!(
                    "task '{task_id_str}' has no live worker: no continuation handle is registered \
                     (it may have exited or been stopped), so no turn will run (the message was \
                     written to task output only)"
                ),
            });
        }

        // Fall back to worker_id. For a background worker registered in the
        // worker table, drive a new turn on its runtime (cross-turn
        // continuation). Otherwise push to the legacy WorkerMailbox.
        if let Some(worker_id) = arguments.get("worker_id").and_then(|v| v.as_str()) {
            // Background worker continuation: push to the mpsc channel so the
            // long-lived worker runs this as a new turn on the same runtime.
            // The std MutexGuard is !Send, so it is scoped to drop before the
            // awaits below (goal-379 switched the table to a sync Mutex so a
            // Drop guard can deregister on the abort path).
            {
                let workers = self.workers.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(handle) = workers.get(worker_id) {
                    match handle.tx.send(message.clone()) {
                        Ok(()) => {
                            return Ok(format!(
                                "Message delivered to worker '{worker_id}'; worker will run it as a new turn."
                            ));
                        }
                        // Issue #176 (review rework): a closed continuation
                        // channel means the worker is gone — the same dead
                        // state the `task_id` branch rejects. Reporting Ok
                        // ("queued") here let a caller believe a dead worker
                        // would still run the message.
                        Err(_) => {
                            return Err(Error::ToolRejected {
                                name: "send_message".into(),
                                reason: format!(
                                    "worker '{worker_id}' has no live worker: its continuation \
                                     channel is closed, so the message was not delivered and no \
                                     turn will run"
                                ),
                            });
                        }
                    }
                }
            }

            // Legacy in-memory mailbox path. A mailbox marked `done` is a
            // worker that has finished: `WorkerMailboxDoneGuard` flips the flag
            // when `run_worker` ends, and `ParallelDeregister`'s
            // `deregister_sync` only removes the entry if its `try_write` wins
            // (see `WorkerRegistry::deregister_sync`), so a done mailbox can
            // legitimately still be present here. Pushing into it and answering
            // "delivered" is the same false success as the case above — nothing
            // will ever drain that queue — so it is reported as a failure too.
            match self.registry.get(worker_id).await {
                Some(mailbox) if mailbox.is_done() => {
                    return Err(Error::ToolRejected {
                        name: "send_message".into(),
                        reason: format!(
                            "worker '{worker_id}' has no live worker: its mailbox is marked done \
                             (the worker finished), so the message was not delivered and no turn \
                             will run"
                        ),
                    });
                }
                Some(mailbox) => {
                    mailbox.push(message).await;
                    return Ok(format!("Message delivered to worker '{worker_id}'."));
                }
                None => {
                    let active = self.registry.active_workers().await;
                    let tasks = self.task_registry.list().await;
                    if active.is_empty() && tasks.is_empty() {
                        return Ok(format!(
                            "Worker '{worker_id}' not found. No active workers or tasks currently registered."
                        ));
                    }
                    let mut sorted = active;
                    sorted.sort_unstable();
                    let task_ids: Vec<String> =
                        tasks.into_iter().map(|t| t.id.to_string()).collect();
                    return Ok(format!(
                        "Worker '{worker_id}' not found. Active workers: [{}]. Active task IDs: [{}].",
                        sorted.join(", "),
                        task_ids.join(", ")
                    ));
                }
            }
        }

        // Neither present.
        Err(Error::BadToolArgs {
            name: "send_message".into(),
            message: "must provide one of: task_id, worker_id".to_string(),
        })
    }
}

// ---------------------------------------------------------------------------
// ListWorkers
// ---------------------------------------------------------------------------

/// List all currently active workers and tasks registered.
pub struct ListWorkersTool {
    registry: WorkerRegistry,
    task_registry: Arc<TaskRegistry>,
}

impl ListWorkersTool {
    pub fn new(registry: WorkerRegistry, task_registry: Arc<TaskRegistry>) -> Self {
        Self {
            registry,
            task_registry,
        }
    }
}

#[async_trait]
impl Tool for ListWorkersTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list_workers".into(),
            description: concat!(
                "List all currently active worker IDs and task IDs. Use this to ",
                "discover peer workers and tasks you can communicate with via ",
                "send_message."
            )
            .into(),
            parameters: json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        ToolSideEffect::ReadOnly
    }

    async fn execute(&self, _arguments: Value) -> Result<String> {
        let mut workers = self.registry.active_workers().await;
        workers.sort_unstable();
        let tasks = self.task_registry.list().await;
        let mut out = String::new();
        if workers.is_empty() {
            out.push_str("Active workers: (none)\n");
        } else {
            out.push_str(&format!("Active workers ({}):\n", workers.len()));
            for w in &workers {
                out.push_str(&format!("  {w}\n"));
            }
        }
        if tasks.is_empty() {
            out.push_str("Active tasks: (none)\n");
        } else {
            out.push_str(&format!("Active tasks ({}):\n", tasks.len()));
            for t in tasks {
                let s = t.status().await;
                out.push_str(&format!("  {} [{}] {}\n", t.id, s.as_str(), t.description));
            }
        }
        Ok(out.trim_end().to_string())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::TaskState;

    #[test]
    fn send_message_tool_is_not_readonly() {
        use crate::tools::Tool;
        let reg = WorkerRegistry::new();
        let tr = Arc::new(TaskRegistry::new());
        let tool = SendMessageTool::new(reg, tr, crate::tools::agent::WorkerTable::default());
        assert!(
            !tool.is_readonly(),
            "SendMessageTool must not be ReadOnly: it pushes messages to worker mailboxes"
        );
        assert_eq!(
            tool.side_effect_class(),
            super::ToolSideEffect::External,
            "SendMessageTool side_effect_class must be External"
        );
    }

    #[tokio::test]
    async fn mailbox_push_pop() {
        let mb = WorkerMailbox::new();
        mb.push("hello".into()).await;
        mb.push("world".into()).await;
        assert_eq!(mb.len().await, 2);
        assert_eq!(mb.pop().await, Some("hello".into()));
        assert_eq!(mb.pop().await, Some("world".into()));
        assert_eq!(mb.pop().await, None);
    }

    #[tokio::test]
    async fn mailbox_drain_all() {
        let mb = WorkerMailbox::new();
        mb.push("a".into()).await;
        mb.push("b".into()).await;
        let msgs = mb.drain_all().await;
        assert_eq!(msgs, vec!["a", "b"]);
        assert!(mb.is_empty().await);
    }

    #[tokio::test]
    async fn registry_register_and_get() {
        let reg = WorkerRegistry::new();
        reg.register("w1").await;
        assert!(reg.get("w1").await.is_some());
        assert!(reg.get("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn registry_deregister() {
        let reg = WorkerRegistry::new();
        reg.register("w1").await;
        reg.deregister("w1").await;
        assert!(reg.get("w1").await.is_none());
    }

    // ── goal-408: alive flag + lazy sweep ─────────────────────────────────────

    #[tokio::test]
    async fn register_sweeps_done_entries() {
        // The registry self-heals: a finished worker's entry is dropped by the
        // next `register`, so a stale id never looks live to `send_message` /
        // `list_workers`.
        let reg = WorkerRegistry::new();
        let a = reg.register("worker-a").await;
        assert!(!a.is_done(), "a freshly registered mailbox is alive");
        a.mark_done();

        let b = reg.register("worker-b").await;

        assert!(
            reg.get("worker-a").await.is_none(),
            "done entry 'worker-a' must be swept by the next register"
        );
        assert!(
            reg.get("worker-b").await.is_some(),
            "the newly registered worker must survive the sweep"
        );
        assert!(!b.is_done(), "the new mailbox must start alive");
    }

    #[tokio::test]
    async fn register_does_not_sweep_live_entries() {
        // The sweep must key on the done flag only — live workers are kept.
        let reg = WorkerRegistry::new();
        reg.register("alive-1").await;
        reg.register("alive-2").await;
        reg.register("alive-3").await;

        let mut active = reg.active_workers().await;
        active.sort_unstable();
        assert_eq!(
            active,
            vec![
                "alive-1".to_string(),
                "alive-2".to_string(),
                "alive-3".to_string()
            ],
            "live entries must not be swept"
        );
    }

    #[tokio::test]
    async fn register_sweeps_entry_marked_through_registry_clone() {
        // The done flag is shared by `Clone` (queue + Arc<AtomicBool>), so
        // marking the mailbox obtained from `get` marks the registered entry.
        let reg = WorkerRegistry::new();
        reg.register("ghost").await;
        let handle = reg.get("ghost").await.expect("registered");
        handle.mark_done();

        reg.register("fresh").await;

        assert!(
            reg.get("ghost").await.is_none(),
            "entry marked done via a registry-obtained clone must be swept"
        );
        assert!(reg.get("fresh").await.is_some());
    }

    #[tokio::test]
    async fn deregister_sync_removes_entry_without_marking_done() {
        // The Drop-context fast path still removes outright (no sweep needed).
        let reg = WorkerRegistry::new();
        let mb = reg.register("w-sync").await;
        assert!(!mb.is_done());

        reg.deregister_sync("w-sync");

        assert!(
            reg.get("w-sync").await.is_none(),
            "deregister_sync must remove the entry on the uncontended path"
        );
        assert!(
            !mb.is_done(),
            "deregister_sync removes the entry; it must not flip the alive flag"
        );
    }

    #[test]
    fn mailbox_done_flag_is_shared_and_defaults_alive() {
        let mb = WorkerMailbox::new();
        let clone = mb.clone();
        assert!(!mb.is_done(), "a new mailbox starts alive");
        assert!(!clone.is_done());

        clone.mark_done();

        assert!(mb.is_done(), "Clone must share the done flag");
        assert!(clone.is_done());
        assert!(
            !WorkerMailbox::new().is_done(),
            "marking one mailbox must not affect an unrelated one"
        );
    }

    #[tokio::test]
    async fn send_message_delivers_to_worker() {
        let reg = WorkerRegistry::new();
        let tr = Arc::new(TaskRegistry::new());
        let mailbox = reg.register("w1").await;

        let tool = SendMessageTool::new(reg, tr, crate::tools::agent::WorkerTable::default());
        let result = tool
            .execute(json!({
                "worker_id": "w1",
                "message": "please add error handling"
            }))
            .await
            .unwrap();

        assert!(result.contains("w1"));
        assert!(result.contains("delivered"));
        assert_eq!(
            mailbox.pop().await.as_deref(),
            Some("please add error handling")
        );
    }

    #[tokio::test]
    async fn send_message_to_task() {
        // A task that IS backed by a live continuation handle is delivered to
        // and drives a new turn.
        let reg = WorkerRegistry::new();
        let tr = Arc::new(TaskRegistry::new());
        let (state, id) = TaskState::new("t", "alpha", "r");
        tr.register(state).await;

        let workers: crate::tools::agent::WorkerTable = Default::default();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        workers.lock().unwrap_or_else(|e| e.into_inner()).insert(
            "alpha".to_string(),
            Arc::new(crate::tools::agent::WorkerHandle {
                tx,
                task_id: id.clone(),
                worker_id: Arc::new("alpha".to_string()),
            }),
        );

        let tool = SendMessageTool::new(reg, tr, workers);
        let result = tool
            .execute(json!({
                "task_id": id.to_string(),
                "message": "do thing"
            }))
            .await
            .unwrap();
        assert!(result.contains(&id.to_string()));
        assert!(result.contains("new turn"), "got: {result}");
        assert_eq!(rx.try_recv().as_deref(), Ok("do thing"));
    }

    /// Issue #176: a `task_id` with no live continuation handle must report a
    /// failure. The task registry and the worker table are independent, so a
    /// task can outlive its worker; the old code answered
    /// `Message delivered to task ...` while no turn would ever run.
    #[tokio::test]
    async fn send_message_to_dead_worker_reports_failure() {
        let reg = WorkerRegistry::new();
        let tr = Arc::new(TaskRegistry::new());
        let (state, id) = TaskState::new("t", "alpha", "r");
        tr.register(state).await;

        // Task is registered but nothing backs it (worker exited / was
        // stopped) — the exact shape of the #176 false success.
        let tool = SendMessageTool::new(reg, tr, crate::tools::agent::WorkerTable::default());
        let res = tool
            .execute(json!({
                "task_id": id.to_string(),
                "message": "anyone home?"
            }))
            .await;

        let err = res.expect_err("no live handle must be reported as a failure, not delivered");
        assert!(
            err.to_string().contains("no live worker"),
            "error must say there is no live worker, got: {err}"
        );
        assert!(
            !err.to_string().contains("delivered"),
            "error must not claim delivery, got: {err}"
        );
    }

    /// Issue #176 (review rework): the `worker_id` path must agree with the
    /// `task_id` path. A handle that is still in the `WorkerTable` while its
    /// continuation channel is closed is a dead worker, and the tool must
    /// report a failure — not `Ok("Message queued …")`, which reads as "your
    /// message is on its way".
    ///
    /// The state is built directly (handle in the table + receiver already
    /// dropped) because that is what a real race window looks like. The
    /// deregister guard is a *body local* of the worker task and `rx` is an
    /// `async move` capture, so locals are dropped before captures: if the
    /// task ever runs, the guard removes the entry before the channel closes.
    /// But a task aborted *before its first poll* never constructs the guard
    /// at all, while its captured receiver is still dropped with the future —
    /// leaving exactly this entry-with-a-closed-channel state behind.
    #[tokio::test]
    async fn send_message_to_dead_worker_by_worker_id_reports_failure() {
        let reg = WorkerRegistry::new();
        let tr = Arc::new(TaskRegistry::new());
        let workers: crate::tools::agent::WorkerTable = Default::default();

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        workers.lock().unwrap_or_else(|e| e.into_inner()).insert(
            "w-dead".to_string(),
            Arc::new(crate::tools::agent::WorkerHandle {
                tx,
                task_id: crate::tasks::TaskId("task-dead".to_string()),
                worker_id: Arc::new("w-dead".to_string()),
            }),
        );
        // The worker's receiver is gone: `tx.send` now fails.
        drop(rx);

        let tool = SendMessageTool::new(reg, tr, workers);
        let res = tool
            .execute(json!({"worker_id": "w-dead", "message": "are you there?"}))
            .await;

        let err = res.expect_err("worker_id with a closed channel must fail, not report queued");
        assert!(
            err.to_string().contains("no live worker"),
            "error must say there is no live worker, got: {err}"
        );
        assert!(
            !err.to_string().contains("Message delivered") && !err.to_string().contains("queued"),
            "error must not claim the message was delivered or queued, got: {err}"
        );
    }

    /// Issue #176 (review rework): the legacy `WorkerRegistry` mailbox path has
    /// the same failure mode. A finished worker's mailbox is marked `done` by
    /// `WorkerMailboxDoneGuard`, while `ParallelDeregister::drop` only removes
    /// the entry when `deregister_sync`'s `try_write` wins — so a done mailbox
    /// can still be returned by `get`. The old code pushed into it and answered
    /// "Message delivered to worker …", although nothing would ever drain it.
    #[tokio::test]
    async fn send_message_to_done_legacy_mailbox_reports_failure() {
        let reg = WorkerRegistry::new();
        let tr = Arc::new(TaskRegistry::new());
        let mailbox = reg.register("w-done").await;
        mailbox.mark_done();

        let tool = SendMessageTool::new(reg, tr, crate::tools::agent::WorkerTable::default());
        let res = tool
            .execute(json!({"worker_id": "w-done", "message": "hello?"}))
            .await;

        let err = res.expect_err("a done legacy mailbox must not report delivery");
        assert!(
            err.to_string().contains("no live worker"),
            "error must say there is no live worker, got: {err}"
        );
        assert!(
            mailbox.is_empty().await,
            "a message must not be queued into a done mailbox"
        );
    }

    #[tokio::test]
    async fn send_message_unknown_task_errors() {
        let reg = WorkerRegistry::new();
        let tr = Arc::new(TaskRegistry::new());
        let tool = SendMessageTool::new(reg, tr, crate::tools::agent::WorkerTable::default());
        let res = tool
            .execute(json!({
                "task_id": "task-bogus",
                "message": "hi"
            }))
            .await;
        assert!(matches!(res, Err(Error::NotFound(_))));
    }

    #[tokio::test]
    async fn send_message_requires_one_of_ids() {
        let reg = WorkerRegistry::new();
        let tr = Arc::new(TaskRegistry::new());
        let tool = SendMessageTool::new(reg, tr, crate::tools::agent::WorkerTable::default());
        let res = tool.execute(json!({ "message": "hi" })).await;
        assert!(matches!(res, Err(Error::BadToolArgs { .. })));
    }

    #[tokio::test]
    async fn send_message_unknown_worker() {
        let reg = WorkerRegistry::new();
        let tr = Arc::new(TaskRegistry::new());
        let tool = SendMessageTool::new(reg, tr, crate::tools::agent::WorkerTable::default());
        let result = tool
            .execute(json!({"worker_id": "ghost", "message": "hi"}))
            .await
            .unwrap();
        assert!(result.contains("ghost"));
        assert!(result.contains("not found"));
    }

    #[tokio::test]
    async fn send_message_shows_active_workers() {
        let reg = WorkerRegistry::new();
        let tr = Arc::new(TaskRegistry::new());
        reg.register("active-1").await;
        let tool = SendMessageTool::new(reg, tr, crate::tools::agent::WorkerTable::default());
        let result = tool
            .execute(json!({"worker_id": "missing", "message": "hello"}))
            .await
            .unwrap();
        assert!(result.contains("active-1"));
    }

    #[tokio::test]
    async fn list_workers_includes_tasks() {
        let reg = WorkerRegistry::new();
        let tr = Arc::new(TaskRegistry::new());
        reg.register("w1").await;
        let (state, id) = TaskState::new("t", "alpha", "r");
        tr.register(state).await;
        let tool = ListWorkersTool::new(reg, tr);
        let out = tool.execute(json!({})).await.unwrap();
        assert!(out.contains("w1"));
        assert!(out.contains(&id.to_string()));
    }

    // ── Additional targeted tests ─────────────────────────────────────────────

    #[tokio::test]
    async fn mailbox_is_empty_false_when_nonempty() {
        // kills `replace WorkerMailbox::is_empty -> bool with true`
        let mb = WorkerMailbox::new();
        assert!(mb.is_empty().await, "fresh mailbox must be empty");
        mb.push("msg".into()).await;
        assert!(
            !mb.is_empty().await,
            "mailbox with a message must NOT be empty"
        );
    }

    #[test]
    fn send_message_tool_is_deferred() {
        // kills `replace SendMessageTool::is_deferred -> bool with false`
        let reg = WorkerRegistry::new();
        let tr = Arc::new(TaskRegistry::new());
        let tool = SendMessageTool::new(reg, tr, crate::tools::agent::WorkerTable::default());
        assert!(
            tool.is_deferred(),
            "SendMessageTool must be deferred (depends on available workers)"
        );
    }

    #[tokio::test]
    async fn registry_active_workers_lists_all_registered() {
        // kills `replace WorkerRegistry::active_workers -> Vec<String> with vec![]`
        let reg = WorkerRegistry::new();
        reg.register("w-alpha").await;
        reg.register("w-beta").await;
        let workers = reg.active_workers().await;
        assert!(
            workers.contains(&"w-alpha".to_string()),
            "active_workers must include w-alpha"
        );
        assert!(
            workers.contains(&"w-beta".to_string()),
            "active_workers must include w-beta"
        );
        assert_eq!(workers.len(), 2, "expected exactly 2 active workers");
    }
}
