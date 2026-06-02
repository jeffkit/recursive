//! `send_message` tool — bidirectional coordinator ↔ worker messaging.
//!
//! # Design
//!
//! Each worker spawned via `spawn_worker` (when operating in coordinator mode)
//! registers a `WorkerMailbox` in the global `WorkerRegistry`.  The coordinator
//! can then call `send_message` to push follow-up instructions into a running
//! worker's mailbox.  The worker's agent loop drains the mailbox between steps
//! and appends any pending messages as new user turns, allowing mid-run guidance
//! without restarting the worker.
//!
//! # Current status
//!
//! This file provides:
//! - `WorkerMailbox` — a thread-safe FIFO queue of pending messages.
//! - `WorkerRegistry` — a shared map from `worker_id` → `WorkerMailbox`.
//! - `SendMessageTool` — the LLM-callable tool that pushes messages.
//!
//! The kernel integration (polling the mailbox between steps) is the next
//! phase; see `.dev/goals/180-send-message-tool.md`.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
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
///
/// Shared between `spawn_worker` (registers on spawn, deregisters on finish)
/// and `send_message` (looks up the mailbox to push messages).
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

/// The `send_message` tool — push a follow-up message to a running worker.
pub struct SendMessageTool {
    registry: WorkerRegistry,
}

impl SendMessageTool {
    pub fn new(registry: WorkerRegistry) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for SendMessageTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "send_message".into(),
            description: concat!(
                "Send a follow-up message to a running worker agent. ",
                "The worker will receive the message between steps and incorporate it ",
                "into its ongoing task. Use this to provide additional guidance, ",
                "corrections, or new information to a worker without restarting it."
            )
            .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "worker_id": {
                        "type": "string",
                        "description": "The worker ID returned by spawn_worker."
                    },
                    "message": {
                        "type": "string",
                        "description": "The message to send to the worker."
                    }
                },
                "required": ["worker_id", "message"]
            }),
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        ToolSideEffect::ReadOnly
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let worker_id = arguments["worker_id"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "send_message".into(),
                message: "missing required parameter: worker_id".to_string(),
            })?;

        let message = arguments["message"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "send_message".into(),
                message: "missing required parameter: message".to_string(),
            })?
            .to_string();

        match self.registry.get(worker_id).await {
            Some(mailbox) => {
                mailbox.push(message).await;
                Ok(format!("Message delivered to worker '{worker_id}'."))
            }
            None => {
                let active = self.registry.active_workers().await;
                if active.is_empty() {
                    Ok(format!(
                        "Worker '{worker_id}' not found. No active workers currently registered."
                    ))
                } else {
                    let mut sorted = active;
                    sorted.sort_unstable();
                    Ok(format!(
                        "Worker '{worker_id}' not found. Active workers: {}",
                        sorted.join(", ")
                    ))
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
        let mailbox = reg.register("w1").await;

        let tool = SendMessageTool::new(reg);
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
    async fn send_message_unknown_worker() {
        let reg = WorkerRegistry::new();
        let tool = SendMessageTool::new(reg);
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
        reg.register("active-1").await;
        let tool = SendMessageTool::new(reg);
        let result = tool
            .execute(json!({"worker_id": "missing", "message": "hello"}))
            .await
            .unwrap();
        assert!(result.contains("active-1"));
    }
}
