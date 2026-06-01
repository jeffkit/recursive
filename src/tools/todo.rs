//! `todo_write` tool — agent task-list management.
//!
//! Allows the agent to maintain a structured checklist for complex tasks.
//! State is kept in a shared `Arc<RwLock<Vec<TodoItem>>>` that can be
//! read back via [`AgentRuntime::current_todos`].

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::event::{AgentEvent, EventSink};
use crate::llm::ToolSpec;
use crate::tools::{Tool, ToolSideEffect};

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// Status of a single task in the agent's to-do list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

/// A single task item in the agent's to-do list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TodoItem {
    /// Task description in imperative form, e.g. "Run tests".
    pub content: String,
    /// Current status of this task.
    pub status: TodoStatus,
    /// Optional present-continuous form shown while `in_progress`,
    /// e.g. "Running tests". Falls back to `content` when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
}

// ---------------------------------------------------------------------------
// TodoWriteTool
// ---------------------------------------------------------------------------

/// Tool that lets the agent create and update a structured task checklist.
///
/// Holds a shared reference to the task list so the runtime can read it
/// back via [`AgentRuntime::current_todos`]. Emits
/// [`AgentEvent::TodoUpdated`] via the supplied `event_sink` on every
/// successful write so downstream consumers (TUI, HTTP API, …) can react
/// without polling.
pub struct TodoWriteTool {
    todo_list: Arc<RwLock<Vec<TodoItem>>>,
    event_sink: Arc<dyn EventSink>,
}

impl TodoWriteTool {
    /// Create a new `TodoWriteTool`.
    ///
    /// * `todo_list` — shared state; pass the same `Arc` to
    ///   [`AgentRuntime`] so `current_todos()` returns up-to-date data.
    /// * `event_sink` — receives [`AgentEvent::TodoUpdated`] on every
    ///   successful write. Pass [`crate::event::NullSink`] to suppress.
    pub fn new(todo_list: Arc<RwLock<Vec<TodoItem>>>, event_sink: Arc<dyn EventSink>) -> Self {
        Self {
            todo_list,
            event_sink,
        }
    }
}

#[async_trait]
impl Tool for TodoWriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "todo_write".into(),
            description: "Create and manage a structured task list for the current session. \
                Use proactively for tasks with 3 or more distinct steps. \
                Update status in real-time as you work. \
                Mark exactly ONE task as in_progress at a time."
                .into(),
            parameters: json!({
                "type": "object",
                "required": ["todos"],
                "properties": {
                    "todos": {
                        "type": "array",
                        "description": "Complete replacement for the task list. \
                            Pass an empty array to clear all tasks.",
                        "items": {
                            "type": "object",
                            "required": ["content", "status"],
                            "properties": {
                                "content": {
                                    "type": "string",
                                    "description": "Task description in imperative form, e.g. \"Run tests\"."
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed", "cancelled"],
                                    "description": "Current status of this task."
                                },
                                "active_form": {
                                    "type": "string",
                                    "description": "Optional present-continuous label shown while in_progress, \
                                        e.g. \"Running tests\"."
                                }
                            }
                        }
                    }
                }
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        // Parse the todos array.
        let raw_todos = arguments.get("todos").ok_or_else(|| Error::BadToolArgs {
            name: "todo_write".into(),
            message: "missing required field `todos`".into(),
        })?;

        let items: Vec<TodoItem> =
            serde_json::from_value(raw_todos.clone()).map_err(|e| Error::BadToolArgs {
                name: "todo_write".into(),
                message: format!("failed to parse `todos`: {e}"),
            })?;

        // Validate: at most one item may have status `in_progress`.
        let in_progress_count = items
            .iter()
            .filter(|t| t.status == TodoStatus::InProgress)
            .count();
        if in_progress_count > 1 {
            return Err(Error::BadToolArgs {
                name: "todo_write".into(),
                message: format!(
                    "at most one task may have status `in_progress`; found {in_progress_count}"
                ),
            });
        }

        // Compute summary stats before we move `items`.
        let count = items.len();
        let in_progress_label = items
            .iter()
            .find(|t| t.status == TodoStatus::InProgress)
            .map(|t| t.active_form.clone().unwrap_or_else(|| t.content.clone()));
        let remaining = items
            .iter()
            .filter(|t| matches!(t.status, TodoStatus::Pending | TodoStatus::InProgress))
            .count();

        // Commit to shared state.
        {
            let mut list = self.todo_list.write().map_err(|_| Error::Tool {
                name: "todo_write".into(),
                message: "todo list lock poisoned".into(),
            })?;
            *list = items.clone();
        }

        // Emit event so TUI / HTTP consumers can react.
        self.event_sink
            .emit(AgentEvent::TodoUpdated {
                todos: items.clone(),
            })
            .await;

        // Return a JSON summary; include the full list so the TUI backend
        // can reconstruct state by parsing this output.
        let result = json!({
            "updated": true,
            "count": count,
            "in_progress": in_progress_label,
            "remaining": remaining,
            "todos": items,
        });
        Ok(result.to_string())
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        // Mutates in-memory state only — safe to re-apply on resume.
        ToolSideEffect::Mutating
    }

    fn is_readonly(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::NullSink;

    fn make_tool() -> (TodoWriteTool, Arc<RwLock<Vec<TodoItem>>>) {
        let list = Arc::new(RwLock::new(vec![]));
        let tool = TodoWriteTool::new(list.clone(), Arc::new(NullSink));
        (tool, list)
    }

    #[tokio::test]
    async fn basic_write_and_read() {
        let (tool, list) = make_tool();
        let args = json!({
            "todos": [
                {"content": "Step 1", "status": "pending"},
                {"content": "Step 2", "status": "in_progress"},
            ]
        });
        let result = tool.execute(args).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["count"], 2);
        assert_eq!(parsed["remaining"], 2);
        assert_eq!(parsed["in_progress"], "Step 2");
        assert_eq!(parsed["updated"], true);

        let stored = list.read().unwrap();
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].content, "Step 1");
    }

    #[tokio::test]
    async fn rejects_multiple_in_progress() {
        let (tool, _) = make_tool();
        let args = json!({
            "todos": [
                {"content": "A", "status": "in_progress"},
                {"content": "B", "status": "in_progress"},
            ]
        });
        let err = tool.execute(args).await.unwrap_err();
        assert!(err.to_string().contains("at most one task"));
    }

    #[tokio::test]
    async fn clears_list_with_empty_array() {
        let (tool, list) = make_tool();
        // Seed with one item.
        let _ = tool
            .execute(json!({"todos": [{"content": "X", "status": "pending"}]}))
            .await
            .unwrap();
        assert_eq!(list.read().unwrap().len(), 1);

        // Clear.
        let result = tool.execute(json!({"todos": []})).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["count"], 0);
        assert!(list.read().unwrap().is_empty());
    }

    #[tokio::test]
    async fn active_form_used_when_present() {
        let (tool, _) = make_tool();
        let args = json!({
            "todos": [
                {
                    "content": "Run tests",
                    "status": "in_progress",
                    "active_form": "Running tests"
                }
            ]
        });
        let result = tool.execute(args).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["in_progress"], "Running tests");
    }

    #[test]
    fn side_effect_is_mutating() {
        let (tool, _) = make_tool();
        assert_eq!(tool.side_effect_class(), ToolSideEffect::Mutating);
        assert!(!tool.is_readonly());
    }
}
