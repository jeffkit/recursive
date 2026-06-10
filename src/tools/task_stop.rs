//! `task_stop` tool — cancel a running task.
//!
//! Cancellation is cooperative: the task's `tokio::task::JoinHandle` is
//! aborted, the task's status is set to `Stopped`.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::tasks::TaskRegistry;
use crate::tools::{Tool, ToolSideEffect};

use super::task_create::lookup_task_id;

pub struct TaskStopTool {
    registry: Arc<TaskRegistry>,
}

impl TaskStopTool {
    pub fn new(registry: Arc<TaskRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for TaskStopTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "task_stop".into(),
            description: concat!(
                "Cancel a running task. The task's status is set to 'stopped' ",
                "and any pending work is interrupted. Idempotent: stopping a ",
                "task that is already in a terminal state is a no-op."
            )
            .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "The task ID to cancel."
                    }
                },
                "required": ["task_id"]
            }),
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        ToolSideEffect::Mutating
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let id = lookup_task_id(&self.registry, &arguments, "task_stop").await?;
        let task = self
            .registry
            .get(&id)
            .await
            .ok_or_else(|| Error::NotFound(format!("task '{id}'")))?;
        let s = task.status().await;
        if s.is_terminal() {
            return Ok(format!("Task {id} is already {}.", s.as_str()));
        }
        let stopped = task.stop().await;
        if stopped {
            Ok(format!("Task {id} cancellation requested."))
        } else {
            // No JoinHandle attached (e.g. it already finished, or the
            // task was never spawned). Still report success.
            Ok(format!(
                "Task {id} has no live handle to stop (already finished?)."
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::{TaskState, TaskStatus};

    #[tokio::test]
    async fn stop_running_task() {
        let reg = Arc::new(TaskRegistry::new());
        let (state, id) = TaskState::new("t", "alpha", "r");
        let arc = reg.register(state).await;
        // Attach a long-lived handle so stop() has something to abort.
        let handle = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        });
        arc.set_handle(handle).await;

        let tool = TaskStopTool::new(reg.clone());
        let result = tool
            .execute(json!({ "task_id": id.to_string() }))
            .await
            .unwrap();
        assert!(result.contains("cancellation requested"));
        let task = reg.get(&id).await.unwrap();
        assert_eq!(task.status().await, TaskStatus::Stopped);
    }

    #[tokio::test]
    async fn stop_terminal_task_is_idempotent() {
        let reg = Arc::new(TaskRegistry::new());
        let (state, id) = TaskState::new("t", "alpha", "r");
        let _ = reg.register(state).await;
        let task = reg.get(&id).await.unwrap();
        task.mark_completed("done".to_string()).await;
        let tool = TaskStopTool::new(reg);
        let result = tool
            .execute(json!({ "task_id": id.to_string() }))
            .await
            .unwrap();
        assert!(result.contains("already"));
        assert!(result.contains("completed"));
    }
}
