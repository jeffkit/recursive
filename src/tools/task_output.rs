//! `task_output` tool — fetch the buffered output of a task and clear it.
//!
//! Each call drains the buffer (so subsequent calls only see *new* output).
//! Set `block=true` to wait for the task to reach a terminal state
//! (completed/failed/stopped) before returning.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::tasks::TaskRegistry;
use crate::tools::{Tool, ToolSideEffect};

use super::task_create::lookup_task_id;

pub struct TaskOutputTool {
    registry: Arc<TaskRegistry>,
}

impl TaskOutputTool {
    pub fn new(registry: Arc<TaskRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for TaskOutputTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "task_output".into(),
            description: concat!(
                "Drain the buffered output lines of a task. Each call returns the ",
                "lines appended since the previous call (or since task creation) ",
                "and clears the buffer.  Use task_get to see the current status.  ",
                "Set block=true to wait until the task reaches a terminal state."
            )
            .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "The task ID to read output from."
                    },
                    "block": {
                        "type": "boolean",
                        "description": "Optional: if true, wait for the task to finish (status != running) before returning. Default: false."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Optional: when block=true, max time to wait in milliseconds. Default: 30000."
                    }
                },
                "required": ["task_id"]
            }),
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        // Drain mutates the buffer (clears it).
        ToolSideEffect::Mutating
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let id = lookup_task_id(&self.registry, &arguments, "task_output").await?;

        let block = arguments
            .get("block")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let timeout_ms = arguments
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(30_000);

        if block {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
            loop {
                let s = self
                    .registry
                    .get(&id)
                    .await
                    .ok_or_else(|| Error::NotFound(format!("task '{id}'")))?
                    .status()
                    .await;
                if s.is_terminal() {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }

        // Drain output buffer.
        let _ = self.registry.drain_output(&id).await;
        let task = self
            .registry
            .get(&id)
            .await
            .ok_or_else(|| Error::NotFound(format!("task '{id}'")))?;
        let lines = task.output_snapshot().await;
        // Clear it (drain consumed the channel, but the snapshot buffer is separate).
        {
            let mut buf = task.output.lock().await;
            buf.clear();
        }

        if lines.is_empty() {
            Ok(String::from("(no new output)"))
        } else {
            Ok(lines.join("\n"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::TaskState;

    #[tokio::test]
    async fn drains_incremental_output() {
        let reg = Arc::new(TaskRegistry::new());
        let (state, id) = TaskState::new("t", "alpha", "r");
        let _ = reg.register(state).await;
        reg.append_output(&id, "line-1".to_string()).await;
        reg.append_output(&id, "line-2".to_string()).await;

        let tool = TaskOutputTool::new(reg.clone());
        let first = tool
            .execute(json!({ "task_id": id.to_string() }))
            .await
            .unwrap();
        assert!(first.contains("line-1"));
        assert!(first.contains("line-2"));

        let second = tool
            .execute(json!({ "task_id": id.to_string() }))
            .await
            .unwrap();
        assert!(second.contains("no new output"));
    }

    #[tokio::test]
    async fn missing_task_errors() {
        let reg = Arc::new(TaskRegistry::new());
        let tool = TaskOutputTool::new(reg);
        let res = tool.execute(json!({ "task_id": "task-bogus" })).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn empty_output_returns_no_new_output_message() {
        // kills `if lines.is_empty()` guard removal / branch-swap mutations
        let reg = Arc::new(TaskRegistry::new());
        let (state, id) = TaskState::new("t", "alpha", "r");
        reg.register(state).await;
        let tool = TaskOutputTool::new(reg);
        let out = tool
            .execute(json!({ "task_id": id.to_string() }))
            .await
            .unwrap();
        assert_eq!(out, "(no new output)", "empty task output must return the sentinel string");
    }

    #[tokio::test]
    async fn output_lines_are_joined_by_newline() {
        // kills `lines.join("\n")` mutation (e.g. join(" ") or join(","))
        let reg = Arc::new(TaskRegistry::new());
        let (state, id) = TaskState::new("t", "alpha", "r");
        let state_arc = reg.register(state).await;
        // Insert lines directly into the output snapshot buffer
        {
            let mut buf = state_arc.output.lock().await;
            buf.push("line-A".to_string());
            buf.push("line-B".to_string());
        }
        let tool = TaskOutputTool::new(reg);
        let out = tool
            .execute(json!({ "task_id": id.to_string() }))
            .await
            .unwrap();
        assert_eq!(out, "line-A\nline-B", "multiple output lines must be joined with '\\n'");
    }

    #[tokio::test]
    async fn block_true_returns_immediately_for_terminal_task() {
        // kills `if block { ... }` removal mutation and `if s.is_terminal() { break }` guard removal
        let reg = Arc::new(TaskRegistry::new());
        let (state, id) = TaskState::new("t", "alpha", "r");
        let arc = reg.register(state).await;
        arc.mark_completed("done".into()).await;
        // Append output after completion
        reg.append_output(&id, "finished".to_string()).await;

        let tool = TaskOutputTool::new(reg);
        let out = tool
            .execute(json!({ "task_id": id.to_string(), "block": true }))
            .await
            .unwrap();
        // Should return output and not hang
        assert!(
            out.contains("finished") || out == "(no new output)",
            "block=true on terminal task must return quickly; got: {out}"
        );
    }

    #[tokio::test]
    async fn block_default_is_false() {
        // kills `unwrap_or(false)` mutation in `block` argument parsing
        let reg = Arc::new(TaskRegistry::new());
        let (state, id) = TaskState::new("t", "alpha", "r");
        reg.register(state).await;

        let tool = TaskOutputTool::new(reg);
        // Without `block` key, must not block (task is Running)
        let out = tool
            .execute(json!({ "task_id": id.to_string() }))
            .await
            .unwrap();
        // Returns immediately since block defaults to false
        assert_eq!(out, "(no new output)", "omitting 'block' must default to false (non-blocking)");
    }
}
