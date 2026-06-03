//! Dynamic team management tools: `team_add_role`, `team_remove_role`, `team_list_roles`.
//!
//! These tools give a coordinator agent the ability to create and destroy
//! specialist roles at runtime — analogous to Fake CC's `TeamCreateTool` and
//! `TeamDeleteTool`.
//!
//! The shared state is an `Arc<tokio::sync::RwLock<AgentPool>>` passed into
//! each tool constructor so all three tools see the same pool.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::multi::{AgentPool, AgentRole};
use crate::tools::{Tool, ToolSideEffect};

// ---------------------------------------------------------------------------
// TeamAddRole
// ---------------------------------------------------------------------------

/// Add or update a role in the shared AgentPool at runtime.
pub struct TeamAddRole {
    pool: Arc<RwLock<AgentPool>>,
}

impl TeamAddRole {
    pub fn new(pool: Arc<RwLock<AgentPool>>) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl Tool for TeamAddRole {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "team_add_role".into(),
            description: concat!(
                "Add (or replace) a specialist role in the coordinator's agent pool. ",
                "After calling this, spawn_worker can use the new role name. ",
                "Use this to dynamically create specialists for specific subtasks."
            )
            .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Unique role name (e.g. 'sql-expert', 'ui-designer')."
                    },
                    "system_prompt": {
                        "type": "string",
                        "description": "The system prompt that defines this role's personality and focus."
                    },
                    "max_steps": {
                        "type": "integer",
                        "description": "Maximum steps for this role's agent (default 30).",
                        "default": 30
                    },
                    "allowed_tools": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional tool allowlist. Empty / absent means all tools."
                    }
                },
                "required": ["name", "system_prompt"]
            }),
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        ToolSideEffect::ReadOnly
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let name = arguments["name"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "team_add_role".into(),
                message: "missing required parameter: name".to_string(),
            })?
            .to_string();

        let system_prompt = arguments["system_prompt"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "team_add_role".into(),
                message: "missing required parameter: system_prompt".to_string(),
            })?
            .to_string();

        let max_steps = arguments["max_steps"].as_i64().unwrap_or(30).clamp(1, 200) as usize;

        let allowed_tools: Vec<String> = arguments["allowed_tools"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let role = AgentRole {
            name: name.clone(),
            system_prompt,
            max_steps,
            allowed_tools,
        };

        self.pool.write().await.add_role(role);
        Ok(format!("Role '{name}' added to team pool."))
    }
}

// ---------------------------------------------------------------------------
// TeamRemoveRole
// ---------------------------------------------------------------------------

/// Remove a role from the shared AgentPool.
pub struct TeamRemoveRole {
    pool: Arc<RwLock<AgentPool>>,
}

impl TeamRemoveRole {
    pub fn new(pool: Arc<RwLock<AgentPool>>) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl Tool for TeamRemoveRole {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "team_remove_role".into(),
            description: concat!(
                "Remove a specialist role from the coordinator's agent pool. ",
                "Use this to clean up roles that are no longer needed."
            )
            .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The role name to remove."
                    }
                },
                "required": ["name"]
            }),
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        ToolSideEffect::ReadOnly
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let name = arguments["name"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "team_remove_role".into(),
                message: "missing required parameter: name".to_string(),
            })?;

        let removed = self.pool.write().await.remove_role(name);
        if removed {
            Ok(format!("Role '{name}' removed from team pool."))
        } else {
            Ok(format!("Role '{name}' not found in team pool."))
        }
    }
}

// ---------------------------------------------------------------------------
// TeamListRoles
// ---------------------------------------------------------------------------

/// List all roles currently registered in the shared AgentPool.
pub struct TeamListRoles {
    pool: Arc<RwLock<AgentPool>>,
}

impl TeamListRoles {
    pub fn new(pool: Arc<RwLock<AgentPool>>) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl Tool for TeamListRoles {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "team_list_roles".into(),
            description: "List all specialist roles currently registered in the agent pool.".into(),
            parameters: json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        ToolSideEffect::ReadOnly
    }

    async fn execute(&self, _arguments: Value) -> Result<String> {
        let pool = self.pool.read().await;
        let names = pool.role_names();
        if names.is_empty() {
            Ok("No roles registered in team pool.".to_string())
        } else {
            let mut sorted = names;
            sorted.sort_unstable();
            Ok(format!(
                "Roles in team pool ({}):\n{}",
                sorted.len(),
                sorted.join("\n")
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::MockProvider;
    use crate::{Config, LlmProvider};

    fn mock_pool() -> Arc<RwLock<AgentPool>> {
        let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider::new(vec![]));
        let config = Config::from_env().unwrap_or_else(|_| Config {
            workspace: std::path::PathBuf::from("."),
            api_base: String::new(),
            api_key: None,
            model: "mock".into(),
            provider_type: "mock".into(),
            preset: None,
            max_steps: 30,
            temperature: 0.0,
            system_prompt: String::new(),
            retry_max: 0,
            retry_initial_backoff_secs: 1,
            retry_max_backoff_secs: 10,
            shell_timeout_secs: 30,
            headless: false,
            memory_summary_limit: 5,
            thinking_budget: None,
            session_name: None,
        });
        Arc::new(RwLock::new(AgentPool::new(provider, config)))
    }

    #[tokio::test]
    async fn team_add_role_registers_role() {
        let pool = mock_pool();
        let tool = TeamAddRole::new(pool.clone());

        let result = tool
            .execute(json!({
                "name": "sql-expert",
                "system_prompt": "You are an expert in SQL databases."
            }))
            .await
            .unwrap();

        assert!(result.contains("sql-expert"));
        assert!(result.contains("added"));
        assert!(pool.read().await.get_role("sql-expert").is_some());
    }

    #[tokio::test]
    async fn team_remove_role_removes_existing() {
        let pool = mock_pool();
        // Pre-populate
        pool.write().await.add_role(AgentRole {
            name: "test-role".into(),
            system_prompt: "Test.".into(),
            max_steps: 10,
            allowed_tools: vec![],
        });

        let tool = TeamRemoveRole::new(pool.clone());
        let result = tool.execute(json!({"name": "test-role"})).await.unwrap();

        assert!(result.contains("removed"));
        assert!(pool.read().await.get_role("test-role").is_none());
    }

    #[tokio::test]
    async fn team_remove_role_missing_returns_not_found() {
        let pool = mock_pool();
        let tool = TeamRemoveRole::new(pool.clone());
        let result = tool.execute(json!({"name": "nonexistent"})).await.unwrap();
        assert!(result.contains("not found"));
    }

    #[tokio::test]
    async fn team_list_roles_empty() {
        let pool = mock_pool();
        let tool = TeamListRoles::new(pool);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.contains("No roles"));
    }

    #[tokio::test]
    async fn team_list_roles_with_entries() {
        let pool = mock_pool();
        pool.write().await.add_role(AgentRole {
            name: "alpha".into(),
            system_prompt: "Alpha.".into(),
            max_steps: 10,
            allowed_tools: vec![],
        });
        pool.write().await.add_role(AgentRole {
            name: "beta".into(),
            system_prompt: "Beta.".into(),
            max_steps: 10,
            allowed_tools: vec![],
        });

        let tool = TeamListRoles::new(pool);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.contains("alpha"));
        assert!(result.contains("beta"));
        assert!(result.contains("2"));
    }

    #[tokio::test]
    async fn team_add_role_missing_name_errors() {
        let pool = mock_pool();
        let tool = TeamAddRole::new(pool);
        let result = tool.execute(json!({"system_prompt": "Something"})).await;
        assert!(result.is_err());
    }
}
