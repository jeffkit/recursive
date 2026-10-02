//! `task_create` tool — create a new entry in the in-memory task registry.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::tasks::{TaskId, TaskRegistry, TaskState};
use crate::tools::{Tool, ToolSideEffect};

pub struct TaskCreateTool {
    registry: Arc<TaskRegistry>,
}

impl TaskCreateTool {
    pub fn new(registry: Arc<TaskRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for TaskCreateTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "task_create".into(),
            description: concat!(
                "Create a new task entry in the in-memory task registry. ",
                "Returns the new task's ID.  Note: tasks are in-memory only and ",
                "do not survive process restart.  Use task_stop to cancel a ",
                "running task; the agent loop will read the task's output and ",
                "final result via task_get / task_output."
            )
            .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "description": {
                        "type": "string",
                        "description": "Human-readable description of the task (e.g. the goal or prompt)."
                    },
                    "team": {
                        "type": "string",
                        "description": "Optional team this task belongs to. Empty means no team."
                    },
                    "name": {
                        "type": "string",
                        "description": "Optional teammate name within the team. Empty means not a teammate."
                    }
                },
                "required": ["description"]
            }),
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        // Mutates in-memory state.
        ToolSideEffect::Mutating
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let description = arguments["description"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "task_create".into(),
                message: "missing required parameter: description".to_string(),
            })?
            .to_string();
        let team = arguments
            .get("team")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let name = arguments
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let (state, id) = TaskState::new(description, team, name);
        let _ = self.registry.register(state).await;
        Ok(format!("Task created: {id}"))
    }
}

/// Lookup a task ID from a `task_id` argument, returning a structured error
/// if it's missing or the task doesn't exist.  Used by the other task_*
/// tools.
pub(crate) async fn lookup_task_id(
    registry: &TaskRegistry,
    arguments: &Value,
    tool_name: &str,
) -> Result<TaskId> {
    let id_str = arguments["task_id"]
        .as_str()
        .ok_or_else(|| Error::BadToolArgs {
            name: tool_name.into(),
            message: "missing required parameter: task_id".to_string(),
        })?;
    let id = TaskId(id_str.to_string());
    if registry.get(&id).await.is_none() {
        return Err(Error::NotFound(format!("task '{id_str}'")));
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_and_lookup() {
        let reg = Arc::new(TaskRegistry::new());
        let tool = TaskCreateTool::new(reg.clone());
        let result = tool
            .execute(json!({
                "description": "build the thing",
                "team": "alpha",
                "name": "r"
            }))
            .await
            .unwrap();
        assert!(result.starts_with("Task created: task-"));
        let id_str = result.trim_start_matches("Task created: ");
        let id = TaskId(id_str.to_string());
        assert!(reg.get(&id).await.is_some());

        let got = reg.get(&id).await.unwrap();
        assert_eq!(got.description, "build the thing");
        assert_eq!(got.team, "alpha");
        assert_eq!(got.name, "r");
    }

    #[tokio::test]
    async fn create_minimal() {
        let reg = Arc::new(TaskRegistry::new());
        let tool = TaskCreateTool::new(reg);
        let result = tool
            .execute(json!({ "description": "minimal" }))
            .await
            .unwrap();
        assert!(result.contains("Task created:"));
    }

    #[tokio::test]
    async fn create_missing_description_errors() {
        let reg = Arc::new(TaskRegistry::new());
        let tool = TaskCreateTool::new(reg);
        let res = tool.execute(json!({})).await;
        assert!(matches!(res, Err(Error::BadToolArgs { .. })));
    }

    #[tokio::test]
    async fn lookup_task_id_missing_argument_errors() {
        // kills `ok_or_else(|| Error::BadToolArgs {...})` removal mutation
        let reg = Arc::new(TaskRegistry::new());
        let res = lookup_task_id(&reg, &json!({}), "mytool").await;
        assert!(
            matches!(res, Err(Error::BadToolArgs { .. })),
            "missing task_id must return BadToolArgs"
        );
    }

    #[tokio::test]
    async fn lookup_task_id_not_found_errors() {
        // kills `if registry.get(&id).await.is_none() { return Err(Error::NotFound) }` removal mutation
        let reg = Arc::new(TaskRegistry::new());
        let res = lookup_task_id(&reg, &json!({"task_id": "task-nonexistent"}), "mytool").await;
        assert!(
            matches!(res, Err(Error::NotFound(_))),
            "nonexistent task_id must return NotFound"
        );
    }

    #[tokio::test]
    async fn lookup_task_id_succeeds_for_existing_task() {
        // kills function-level replacement of lookup_task_id
        let reg = Arc::new(TaskRegistry::new());
        let (state, id) = TaskState::new("some task", "", "");
        reg.register(state).await;
        let result = lookup_task_id(&reg, &json!({"task_id": id.to_string()}), "mytool").await;
        assert!(result.is_ok(), "existing task must return Ok");
        assert_eq!(result.unwrap(), id, "returned id must match registered id");
    }

    #[tokio::test]
    async fn team_and_name_default_to_empty() {
        // kills `unwrap_or("")` removal mutations for `team` and `name`
        let reg = Arc::new(TaskRegistry::new());
        let tool = TaskCreateTool::new(reg.clone());
        let result = tool
            .execute(json!({ "description": "no team or name" }))
            .await
            .unwrap();
        let id_str = result.trim_start_matches("Task created: ");
        let id = TaskId(id_str.to_string());
        let task = reg.get(&id).await.unwrap();
        assert_eq!(task.team, "", "team must default to empty string");
        assert_eq!(task.name, "", "name must default to empty string");
    }
}
