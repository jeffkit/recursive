//! `task_update` tool — set the final result of a running task.
//!
//! Supports two terminal transitions:
//! - `running` → `completed` (requires `result`)
//! - `running` → `failed`    (requires `error`)
//!
//! Other transitions are not supported. Use `task_stop` for cancellation.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::tasks::TaskRegistry;
use crate::tools::{Tool, ToolSideEffect};

use super::task_create::lookup_task_id;

pub struct TaskUpdateTool {
    registry: Arc<TaskRegistry>,
}

impl TaskUpdateTool {
    pub fn new(registry: Arc<TaskRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for TaskUpdateTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "task_update".into(),
            description: concat!(
                "Update a running task with its final result. Allowed transitions: ",
                "running -> completed (requires 'result'); running -> failed ",
                "(requires 'error'). Use task_stop for cancellation."
            )
            .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "The task ID to update."
                    },
                    "status": {
                        "type": "string",
                        "description": "The new terminal status.",
                        "enum": ["completed", "failed"]
                    },
                    "result": {
                        "type": "string",
                        "description": "Required when status='completed'."
                    },
                    "error": {
                        "type": "string",
                        "description": "Required when status='failed'."
                    }
                },
                "required": ["task_id", "status"]
            }),
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        ToolSideEffect::Mutating
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let id = lookup_task_id(&self.registry, &arguments, "task_update").await?;
        let new_status_str = arguments["status"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "task_update".into(),
                message: "missing required parameter: status".to_string(),
            })?;

        let task = self.registry.get(&id).await.unwrap();
        let current = task.status().await;
        // Allow transition from any non-terminal state.
        if current.is_terminal() {
            return Err(Error::BadToolArgs {
                name: "task_update".into(),
                message: format!("task {id} is already in terminal state '{current}'", current = current.as_str()),
            });
        }

        match new_status_str {
            "completed" => {
                let result =
                    arguments["result"]
                        .as_str()
                        .ok_or_else(|| Error::BadToolArgs {
                            name: "task_update".into(),
                            message: "status='completed' requires 'result'".to_string(),
                        })?;
                task.mark_completed(result.to_string()).await;
                Ok(format!("Task {id} marked completed."))
            }
            "failed" => {
                let err = arguments["error"]
                    .as_str()
                    .ok_or_else(|| Error::BadToolArgs {
                        name: "task_update".into(),
                        message: "status='failed' requires 'error'".to_string(),
                    })?;
                task.mark_failed(err.to_string()).await;
                Ok(format!("Task {id} marked failed."))
            }
            other => Err(Error::BadToolArgs {
                name: "task_update".into(),
                message: format!("invalid status: '{other}' (use 'completed' or 'failed')"),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::{TaskState, TaskStatus};

    #[tokio::test]
    async fn completed_with_result() {
        let reg = Arc::new(TaskRegistry::new());
        let (state, id) = TaskState::new("t", "alpha", "r");
        reg.register(state).await;
        let tool = TaskUpdateTool::new(reg.clone());
        let out = tool
            .execute(json!({
                "task_id": id.to_string(),
                "status": "completed",
                "result": "ok"
            }))
            .await
            .unwrap();
        assert!(out.contains("completed"));
        let task = reg.get(&id).await.unwrap();
        let s = task.status().await;
        assert_eq!(s, TaskStatus::Completed);
        let fr = task.final_result.lock().await.clone().unwrap();
        assert!(matches!(fr, Ok(ref v) if v == "ok"));
    }

    #[tokio::test]
    async fn failed_with_error() {
        let reg = Arc::new(TaskRegistry::new());
        let (state, id) = TaskState::new("t", "alpha", "r");
        reg.register(state).await;
        let tool = TaskUpdateTool::new(reg);
        let out = tool
            .execute(json!({
                "task_id": id.to_string(),
                "status": "failed",
                "error": "boom"
            }))
            .await
            .unwrap();
        assert!(out.contains("failed"));
    }

    #[tokio::test]
    async fn completed_without_result_errors() {
        let reg = Arc::new(TaskRegistry::new());
        let (state, id) = TaskState::new("t", "alpha", "r");
        reg.register(state).await;
        let tool = TaskUpdateTool::new(reg);
        let res = tool
            .execute(json!({
                "task_id": id.to_string(),
                "status": "completed"
            }))
            .await;
        assert!(matches!(res, Err(Error::BadToolArgs { .. })));
    }

    #[tokio::test]
    async fn update_terminal_task_errors() {
        let reg = Arc::new(TaskRegistry::new());
        let (state, id) = TaskState::new("t", "alpha", "r");
        let arc = reg.register(state).await;
        arc.mark_completed("done".to_string()).await;
        let tool = TaskUpdateTool::new(reg);
        let res = tool
            .execute(json!({
                "task_id": id.to_string(),
                "status": "completed",
                "result": "x"
            }))
            .await;
        assert!(matches!(res, Err(Error::BadToolArgs { .. })));
    }
}
