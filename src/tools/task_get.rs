//! `task_get` tool — fetch a single task's status and metadata by ID.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::error::Result;
use crate::llm::ToolSpec;
use crate::tasks::TaskRegistry;
use crate::tools::{Tool, ToolSideEffect};

use super::task_create::lookup_task_id;

pub struct TaskGetTool {
    registry: Arc<TaskRegistry>,
}

impl TaskGetTool {
    pub fn new(registry: Arc<TaskRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for TaskGetTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "task_get".into(),
            description: "Fetch a single task's status and metadata by ID.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "The task ID (returned by task_create)."
                    }
                },
                "required": ["task_id"]
            }),
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        ToolSideEffect::ReadOnly
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let id = lookup_task_id(&self.registry, &arguments, "task_get").await?;
        let task = self.registry.get(&id).await.unwrap();
        // Drain any pending output so the snapshot is current.
        let _ = self.registry.drain_output(&id).await;
        let status = task.status().await;
        let output = task.output_snapshot().await;
        let last = task.final_result.lock().await.clone();
        let mut s = format!(
            "Task {id}\n  description: {}\n  team: {}\n  name: {}\n  started_at: {}\n  status: {}\n",
            task.description,
            if task.team.is_empty() { "(none)" } else { &task.team },
            if task.name.is_empty() { "(none)" } else { &task.name },
            task.started_at.to_rfc3339(),
            status.as_str(),
        );
        if !output.is_empty() {
            s.push_str(&format!("  output_lines: {}\n", output.len()));
        }
        if let Some(result) = last {
            match result {
                Ok(text) => s.push_str(&format!("  result: {}\n", truncate(&text, 200))),
                Err(e) => s.push_str(&format!("  error: {}\n", truncate(&e, 200))),
            }
        }
        Ok(s)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::TaskState;

    #[tokio::test]
    async fn get_existing_task() {
        let reg = Arc::new(TaskRegistry::new());
        let (state, id) = TaskState::new("hello", "alpha", "r");
        reg.register(state).await;
        let tool = TaskGetTool::new(reg);
        let result = tool
            .execute(json!({ "task_id": id.to_string() }))
            .await
            .unwrap();
        assert!(result.contains(&id.to_string()));
        assert!(result.contains("hello"));
        assert!(result.contains("alpha"));
        assert!(result.contains("running"));
    }

    #[tokio::test]
    async fn get_missing_task_errors() {
        let reg = Arc::new(TaskRegistry::new());
        let tool = TaskGetTool::new(reg);
        let res = tool
            .execute(json!({ "task_id": "task-does-not-exist" }))
            .await;
        assert!(matches!(res, Err(crate::error::Error::NotFound(_))));
    }
}
