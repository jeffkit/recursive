//! `team_delete` tool — remove a team roster file.
//!
//! # Inputs
//!
//! ```json
//! { "name": "alpha" }
//! ```
//!
//! Idempotent: deleting a non-existent team returns a helpful
//! message but does not error.  This is a deliberate choice so
//! coordinator scripts can use `team_delete` as a "make sure it
//! doesn't exist" step.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::team::TeamRegistry;
use crate::tools::{Tool, ToolSideEffect};

/// The `team_delete` tool.
pub struct TeamDeleteTool;

impl TeamDeleteTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for TeamDeleteTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for TeamDeleteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "team_delete".into(),
            description: concat!(
                "Delete a team and its roster file. Idempotent: deleting a ",
                "non-existent team is a no-op (returns an informative message)."
            )
            .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The team name to delete."
                    }
                },
                "required": ["name"]
            }),
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        ToolSideEffect::Mutating
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let name = arguments["name"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "team_delete".into(),
                message: "missing required parameter: name".to_string(),
            })?
            .to_string();

        // Reject path-traversal names defensively.
        if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
            return Err(Error::BadToolArgs {
                name: "team_delete".into(),
                message: format!("invalid team name: '{name}'"),
            });
        }

        let existed = TeamRegistry::delete(&name).await?;
        if existed {
            Ok(format!("Deleted team '{name}'."))
        } else {
            Ok(format!("Team '{name}' did not exist (no-op)."))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static LOCK: Mutex<()> = Mutex::new(());

    struct TeamsDirGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        _tmp: tempfile::TempDir,
        prev: Option<std::ffi::OsString>,
    }

    fn with_temp_teams_dir() -> TeamsDirGuard {
        let lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var_os("RECURSIVE_TEAMS_DIR");
        std::env::set_var("RECURSIVE_TEAMS_DIR", tmp.path());
        TeamsDirGuard {
            _lock: lock,
            _tmp: tmp,
            prev,
        }
    }

    impl Drop for TeamsDirGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var("RECURSIVE_TEAMS_DIR", v),
                None => std::env::remove_var("RECURSIVE_TEAMS_DIR"),
            }
        }
    }

    #[tokio::test]
    async fn delete_existing_team() {
        let _g = with_temp_teams_dir();
        // First create the team.
        TeamRegistry::create("alpha").await.unwrap();
        assert!(crate::team::team_file_path("alpha").exists());

        // Then delete it.
        let tool = TeamDeleteTool::new();
        let result = tool.execute(json!({ "name": "alpha" })).await.unwrap();
        assert!(result.contains("Deleted"));
        assert!(!crate::team::team_file_path("alpha").exists());
    }

    #[tokio::test]
    async fn delete_nonexistent_is_idempotent() {
        let _g = with_temp_teams_dir();
        let tool = TeamDeleteTool::new();
        let result = tool.execute(json!({ "name": "ghost" })).await.unwrap();
        assert!(result.contains("did not exist"));
        // Second delete is still a no-op.
        let result2 = tool.execute(json!({ "name": "ghost" })).await.unwrap();
        assert!(result2.contains("did not exist"));
    }

    #[tokio::test]
    async fn delete_rejects_invalid_name() {
        let _g = with_temp_teams_dir();
        let tool = TeamDeleteTool::new();
        for bad in ["", "../etc", "a/b"] {
            let res = tool.execute(json!({ "name": bad })).await;
            assert!(res.is_err(), "name '{bad}' should be rejected");
        }
    }

    #[tokio::test]
    async fn delete_missing_name_field_errors() {
        // kills `ok_or_else(|| Error::BadToolArgs {...})` removal mutation
        let _g = with_temp_teams_dir();
        let tool = TeamDeleteTool::new();
        let res = tool.execute(json!({})).await;
        assert!(
            matches!(res, Err(Error::BadToolArgs { .. })),
            "missing 'name' field must return BadToolArgs"
        );
    }

    #[tokio::test]
    async fn delete_rejects_backslash_name() {
        // kills `name.contains('\\')` removal mutation
        let _g = with_temp_teams_dir();
        let tool = TeamDeleteTool::new();
        let res = tool.execute(json!({ "name": "a\\b" })).await;
        assert!(res.is_err(), "backslash in name must be rejected");
    }
}
