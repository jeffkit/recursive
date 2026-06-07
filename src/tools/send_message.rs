//! `send_message` tool — coordinator ↔ worker (or task) messaging.
//!
//! # Resolution order
//!
//! Callers can address the target in two ways:
//! 1. **`task_id`** — preferred, used when the worker was spawned via the
//!    Phase D `agent` tool with `team_name+name` (or `run_in_background`).
//!    The message is pushed into the task's output channel, which the
//!    worker's prompt loop can poll between steps (or, more practically,
//!    which the human can read on the next `task_output` call).
//! 2. **`worker_id`** — legacy, addresses a worker registered in
//!    `WorkerRegistry` (from pre-Phase-D `spawn_worker`).
//!
//! If both are present, `task_id` wins.  If neither resolves, the tool
//! returns a helpful message listing active workers and tasks.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::VecDeque;
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
#[derive(Clone, Default)]
pub struct WorkerMailbox {
    queue: Arc<tokio::sync::Mutex<VecDeque<String>>>,
}

impl WorkerMailbox {
    pub fn new() -> Self {
        Self::default()
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
    pub async fn register(&self, worker_id: &str) -> WorkerMailbox {
        let mailbox = WorkerMailbox::new();
        self.inner
            .write()
            .await
            .insert(worker_id.to_string(), mailbox.clone());
        mailbox
    }

    /// Deregister a worker (called when it finishes).
    pub async fn deregister(&self, worker_id: &str) {
        self.inner.write().await.remove(worker_id);
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
}

impl SendMessageTool {
    pub fn new(registry: WorkerRegistry, task_registry: Arc<TaskRegistry>) -> Self {
        Self {
            registry,
            task_registry,
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
            let task = self
                .task_registry
                .get(&crate::tasks::TaskId(task_id_str.to_string()))
                .await
                .ok_or_else(|| Error::NotFound(format!("task '{task_id_str}'")))?;
            // Use the public output_tx so the worker (or a human reading
            // task_output) can see the message.
            // `output_tx` is an mpsc::UnboundedSender<String>; send returns
            // an Err if the receiver is gone.
            let _ = task.output_tx.send(message.clone());
            return Ok(format!("Message delivered to task '{task_id_str}'."));
        }

        // Fall back to worker_id (legacy).
        if let Some(worker_id) = arguments.get("worker_id").and_then(|v| v.as_str()) {
            match self.registry.get(worker_id).await {
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
        let tool = SendMessageTool::new(reg, tr);
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

    #[tokio::test]
    async fn send_message_delivers_to_worker() {
        let reg = WorkerRegistry::new();
        let tr = Arc::new(TaskRegistry::new());
        let mailbox = reg.register("w1").await;

        let tool = SendMessageTool::new(reg, tr);
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
        let reg = WorkerRegistry::new();
        let tr = Arc::new(TaskRegistry::new());
        let (state, id) = TaskState::new("t", "alpha", "r");
        tr.register(state).await;

        let tool = SendMessageTool::new(reg, tr);
        let result = tool
            .execute(json!({
                "task_id": id.to_string(),
                "message": "do thing"
            }))
            .await
            .unwrap();
        assert!(result.contains(&id.to_string()));
        assert!(result.contains("delivered"));
    }

    #[tokio::test]
    async fn send_message_unknown_task_errors() {
        let reg = WorkerRegistry::new();
        let tr = Arc::new(TaskRegistry::new());
        let tool = SendMessageTool::new(reg, tr);
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
        let tool = SendMessageTool::new(reg, tr);
        let res = tool.execute(json!({ "message": "hi" })).await;
        assert!(matches!(res, Err(Error::BadToolArgs { .. })));
    }

    #[tokio::test]
    async fn send_message_unknown_worker() {
        let reg = WorkerRegistry::new();
        let tr = Arc::new(TaskRegistry::new());
        let tool = SendMessageTool::new(reg, tr);
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
        let tool = SendMessageTool::new(reg, tr);
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
}
