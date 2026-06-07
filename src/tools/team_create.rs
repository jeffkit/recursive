//! `team_create` tool — create a new team roster file.
//!
//! # Inputs
//!
//! ```json
//! { "name": "alpha" }
//! ```
//!
//! Optionally:
//! ```json
//! { "name": "alpha", "members": [
//!     { "name": "researcher", "agent_type": "general" },
//!     { "name": "coder", "agent_type": "general", "model": "claude-opus-4-7" }
//! ] }
//! ```
//!
//! # Behavior
//!
//! Writes a `TeamFile` to `~/.claude/teams/{name}.json` (atomic). If
//! a file with that name already exists, the call **fails** — use
//! `team_delete` first to recreate.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::team::{TeamFile, TeamMember, TeamRegistry};
use crate::tools::{Tool, ToolSideEffect};

/// The `team_create` tool.
pub struct TeamCreateTool {
    /// Shared in-memory team registry. The tool writes the freshly
    /// created team to disk AND registers it here so the same process
    /// can immediately observe it via `team_list` / `team_get`.
    registry: Arc<TeamRegistry>,
}

impl TeamCreateTool {
    pub fn new(registry: Arc<TeamRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for TeamCreateTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "team_create".into(),
            description: concat!(
                "Create a new team. Persists a roster file at ",
                "~/.claude/teams/{name}.json. Optionally pre-populates the team ",
                "with members. Fails if a team with that name already exists; ",
                "use team_delete first to recreate."
            )
            .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The team name. Used as the filename stem and must be unique within the teams directory."
                    },
                    "members": {
                        "type": "array",
                        "description": "Optional list of members to pre-populate the team with.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": { "type": "string" },
                                "agent_type": { "type": "string" },
                                "model": { "type": "string" }
                            },
                            "required": ["name"]
                        }
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
                name: "team_create".into(),
                message: "missing required parameter: name".to_string(),
            })?
            .to_string();

        // Validate name (no slashes, no path traversal).
        if name.is_empty()
            || name.contains('/')
            || name.contains('\\')
            || name.contains("..")
            || name.starts_with('.')
        {
            return Err(Error::BadToolArgs {
                name: "team_create".into(),
                message: format!("invalid team name: '{name}'"),
            });
        }

        // Reject if team already exists on disk.
        if crate::team::team_file_path(&name).exists() {
            return Err(Error::BadToolArgs {
                name: "team_create".into(),
                message: format!("team '{name}' already exists; use team_delete first to recreate"),
            });
        }

        // Optionally pre-populate members.
        let members: Vec<TeamMember> = arguments
            .get("members")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        let n = m.get("name")?.as_str()?.to_string();
                        let t = m
                            .get("agent_type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("general")
                            .to_string();
                        let mut member = TeamMember::new(n, t);
                        if let Some(model) = m.get("model").and_then(|v| v.as_str()) {
                            member = member.with_model(model);
                        }
                        Some(member)
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Build the in-memory team, add any pre-populated members, then
        // both persist it to disk and register it in the held registry so
        // subsequent `team_list` / `team_get` calls in the same process
        // can see it.
        let mut team = TeamFile::new(&name);
        for m in members {
            team.add_member(m);
        }
        TeamRegistry::save_team(&team)?;
        self.registry.register_team(team.clone()).await;

        let team = self.registry.get(&name).await.unwrap();
        Ok(format!(
            "Created team '{name}' ({} members).",
            team.member_count()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static LOCK: Mutex<()> = Mutex::new(());

    /// RAII guard: redirect `RECURSIVE_TEAMS_DIR` to a fresh tempdir for
    /// the lifetime of the guard.
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
    async fn create_empty_team() {
        let _g = with_temp_teams_dir();
        let reg = Arc::new(TeamRegistry::new());
        let tool = TeamCreateTool::new(reg.clone());
        let result = tool
            .execute(json!({ "name": "alpha" }))
            .await
            .unwrap();
        assert!(result.contains("alpha"));
        assert!(result.contains("0 members"));
        assert!(crate::team::team_file_path("alpha").exists());
    }

    #[tokio::test]
    async fn create_with_members() {
        let _g = with_temp_teams_dir();
        let reg = Arc::new(TeamRegistry::new());
        let tool = TeamCreateTool::new(reg.clone());
        let result = tool
            .execute(json!({
                "name": "beta",
                "members": [
                    { "name": "r", "agent_type": "general" },
                    { "name": "c", "model": "claude-opus-4-7" }
                ]
            }))
            .await
            .unwrap();
        assert!(result.contains("2 members"));
        let team = reg.get("beta").await.unwrap();
        assert_eq!(team.member_count(), 2);
        assert!(team.get_member("r").is_some());
        assert!(team.get_member("c").is_some());
    }

    #[tokio::test]
    async fn create_rejects_existing() {
        let _g = with_temp_teams_dir();
        let reg = Arc::new(TeamRegistry::new());
        let tool = TeamCreateTool::new(reg);
        tool.execute(json!({ "name": "alpha" })).await.unwrap();
        let reg2 = Arc::new(TeamRegistry::new());
        let tool2 = TeamCreateTool::new(reg2);
        let res = tool2.execute(json!({ "name": "alpha" })).await;
        assert!(res.is_err(), "second create should fail");
    }

    #[tokio::test]
    async fn create_rejects_invalid_name() {
        let _g = with_temp_teams_dir();
        let reg = Arc::new(TeamRegistry::new());
        let tool = TeamCreateTool::new(reg);
        for bad in ["", "foo/bar", "../etc", ".hidden"] {
            let res = tool.execute(json!({ "name": bad })).await;
            assert!(res.is_err(), "name '{bad}' should be rejected");
        }
    }
}
