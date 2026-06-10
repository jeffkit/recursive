//! `task_list` tool — list all tasks in the registry, with optional filters.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::error::Result;
use crate::llm::ToolSpec;
use crate::tasks::{TaskRegistry, TaskStatus};
use crate::tools::{Tool, ToolSideEffect};

pub struct TaskListTool {
    registry: Arc<TaskRegistry>,
}

impl TaskListTool {
    pub fn new(registry: Arc<TaskRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for TaskListTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "task_list".into(),
            description: concat!(
                "List all tasks in the in-memory task registry. Optionally ",
                "filter by status, team, or teammate name."
            )
            .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "description": "Optional: filter by task status (running, completed, failed, stopped).",
                        "enum": ["running", "completed", "failed", "stopped"]
                    },
                    "team": {
                        "type": "string",
                        "description": "Optional: only include tasks belonging to this team."
                    },
                    "name": {
                        "type": "string",
                        "description": "Optional: only include tasks for this teammate name."
                    }
                }
            }),
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        ToolSideEffect::ReadOnly
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let status_filter =
            arguments
                .get("status")
                .and_then(|v| v.as_str())
                .and_then(|s| match s {
                    "running" => Some(TaskStatus::Running),
                    "completed" => Some(TaskStatus::Completed),
                    "failed" => Some(TaskStatus::Failed),
                    "stopped" => Some(TaskStatus::Stopped),
                    _ => None,
                });
        let team_filter = arguments.get("team").and_then(|v| v.as_str());
        let name_filter = arguments.get("name").and_then(|v| v.as_str());

        let mut tasks = self.registry.list().await;
        let mut out_lines: Vec<(String, TaskStatus, String, String, String)> = Vec::new();
        for t in tasks.drain(..) {
            let s = t.status().await;
            if let Some(sf) = status_filter {
                if s != sf {
                    continue;
                }
            }
            if let Some(team) = team_filter {
                if t.team != team {
                    continue;
                }
            }
            if let Some(name) = name_filter {
                if t.name != name {
                    continue;
                }
            }
            out_lines.push((
                t.id.to_string(),
                s,
                t.team.clone(),
                t.name.clone(),
                t.description.clone(),
            ));
        }

        if out_lines.is_empty() {
            return Ok("(no tasks)".to_string());
        }

        let mut out = format!("{} task(s):\n", out_lines.len());
        for (id, s, team, name, desc) in out_lines {
            out.push_str(&format!(
                "  {id} [{:>9}] team={} name={} desc={}\n",
                s.as_str(),
                if team.is_empty() { "-" } else { &team },
                if name.is_empty() { "-" } else { &name },
                truncate(&desc, 80),
            ));
        }
        Ok(out)
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
    async fn list_empty() {
        let reg = Arc::new(TaskRegistry::new());
        let tool = TaskListTool::new(reg);
        let out = tool.execute(json!({})).await.unwrap();
        assert!(out.contains("(no tasks)"));
    }

    #[tokio::test]
    async fn list_with_filter() {
        let reg = Arc::new(TaskRegistry::new());
        let (s1, _) = TaskState::new("t1", "alpha", "r");
        let (s2, _) = TaskState::new("t2", "beta", "c");
        let (s3, _) = TaskState::new("t3", "alpha", "r");
        reg.register(s1).await;
        reg.register(s2).await;
        reg.register(s3).await;

        let tool = TaskListTool::new(reg.clone());
        let all = tool.execute(json!({})).await.unwrap();
        assert!(all.contains("3 task(s)"));
        let alpha = tool.execute(json!({ "team": "alpha" })).await.unwrap();
        assert!(alpha.contains("2 task(s)"));
        let alpha_r = tool
            .execute(json!({ "team": "alpha", "name": "r" }))
            .await
            .unwrap();
        assert!(alpha_r.contains("2 task(s)"));
    }
}
