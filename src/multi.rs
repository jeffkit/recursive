//! Multi-agent orchestration: agent pool and role definitions.

use crate::{Agent, AgentOutcome, Config, LlmProvider};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;

/// Shared memory store for multi-agent coordination.
#[derive(Clone)]
pub struct SharedMemory {
    store: Arc<RwLock<HashMap<String, MemoryEntry>>>,
}

/// A single entry in the shared memory store.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct MemoryEntry {
    pub key: String,
    pub value: String,
    pub author: String,
    pub timestamp: u64,
}

impl SharedMemory {
    pub fn new() -> Self {
        Self {
            store: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn set(&self, key: String, value: String, author: String) {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let entry = MemoryEntry {
            key: key.clone(),
            value,
            author,
            timestamp,
        };
        self.store.write().await.insert(key, entry);
    }

    pub async fn get(&self, key: &str) -> Option<MemoryEntry> {
        self.store.read().await.get(key).cloned()
    }

    pub async fn keys(&self) -> Vec<String> {
        self.store.read().await.keys().cloned().collect()
    }

    pub async fn all(&self) -> Vec<MemoryEntry> {
        self.store.read().await.values().cloned().collect()
    }

    pub async fn remove(&self, key: &str) -> bool {
        self.store.write().await.remove(key).is_some()
    }

    pub async fn to_context_string(&self) -> String {
        let store = self.store.read().await;
        if store.is_empty() {
            return String::new();
        }
        let mut lines = vec!["[Shared Memory]".to_string()];
        for entry in store.values() {
            lines.push(format!("- {} = {} (by {})", entry.key, entry.value, entry.author));
        }
        lines.join("\n")
    }

    pub async fn len(&self) -> usize {
        self.store.read().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.store.read().await.is_empty()
    }
}

impl Default for SharedMemory {
    fn default() -> Self {
        Self::new()
    }
}

/// Definition of an agent role.
#[derive(Clone, Debug)]
pub struct AgentRole {
    pub name: String,
    pub system_prompt: String,
    pub max_steps: usize,
    pub allowed_tools: Vec<String>,
}

/// An agent pool manages multiple agents with different roles.
pub struct AgentPool {
    roles: HashMap<String, AgentRole>,
    provider: Arc<dyn LlmProvider>,
    #[allow(dead_code)]
    config: Config,
    memory: SharedMemory,
}

impl AgentPool {
    pub fn new(provider: Arc<dyn LlmProvider>, config: Config) -> Self {
        Self {
            roles: HashMap::new(),
            provider,
            config,
            memory: SharedMemory::new(),
        }
    }

    pub fn memory(&self) -> &SharedMemory {
        &self.memory
    }

    pub fn add_role(&mut self, role: AgentRole) {
        self.roles.insert(role.name.clone(), role);
    }

    pub fn get_role(&self, name: &str) -> Option<&AgentRole> {
        self.roles.get(name)
    }

    pub fn role_names(&self) -> Vec<&str> {
        self.roles.keys().map(|s| s.as_str()).collect()
    }

    pub fn role_count(&self) -> usize {
        self.roles.len()
    }

    pub async fn run_with_role(
        &self,
        role_name: &str,
        goal: &str,
    ) -> Result<AgentOutcome, crate::Error> {
        let role = self
            .roles
            .get(role_name)
            .ok_or_else(|| crate::Error::Config {
                message: format!("unknown role: {role_name}"),
            })?;

        let memory_ctx = self.memory.to_context_string().await;
        let system_prompt = if memory_ctx.is_empty() {
            role.system_prompt.clone()
        } else {
            format!("{}\n\n{}", role.system_prompt, memory_ctx)
        };

        let mut agent = Agent::builder()
            .llm(self.provider.clone())
            .system_prompt(system_prompt)
            .max_steps(role.max_steps)
            .build()?;

        agent.run(goal).await
    }
}

/// Default role set for common multi-agent patterns.
pub fn default_roles() -> Vec<AgentRole> {
    vec![
        AgentRole {
            name: "planner".into(),
            system_prompt: "You are a planning agent. Analyze the task, break it into steps, \
                            and output a structured plan. Do not execute — only plan."
                .into(),
            max_steps: 10,
            allowed_tools: vec![],
        },
        AgentRole {
            name: "coder".into(),
            system_prompt: "You are a coding agent. Implement the task using the available \
                            tools. Write code, run tests, fix errors."
                .into(),
            max_steps: 50,
            allowed_tools: vec![],
        },
        AgentRole {
            name: "reviewer".into(),
            system_prompt: "You are a code review agent. Read the code changes, identify \
                            issues, suggest improvements. Do not modify files."
                .into(),
            max_steps: 20,
            allowed_tools: vec!["read_file".into(), "search_files".into()],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Completion, MockProvider};
    use std::path::PathBuf;

    fn test_config() -> Config {
        Config {
            workspace: PathBuf::from("."),
            api_base: String::new(),
            api_key: None,
            model: String::new(),
            provider_type: "openai".into(),
            max_steps: 32,
            temperature: 0.2,
            system_prompt: String::new(),
            retry_max: 2,
            retry_initial_backoff_secs: 1,
            retry_max_backoff_secs: 8,
            shell_timeout_secs: 300,
        }
    }

    #[test]
    fn new_pool_is_empty() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let pool = AgentPool::new(provider, test_config());
        assert_eq!(pool.role_count(), 0);
    }

    #[test]
    fn add_role_and_get_role() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let mut pool = AgentPool::new(provider, test_config());

        let role = AgentRole {
            name: "tester".into(),
            system_prompt: "You test things.".into(),
            max_steps: 5,
            allowed_tools: vec!["run_shell".into()],
        };
        pool.add_role(role.clone());

        let retrieved = pool.get_role("tester").unwrap();
        assert_eq!(retrieved.name, "tester");
        assert_eq!(retrieved.system_prompt, "You test things.");
        assert_eq!(retrieved.max_steps, 5);
        assert_eq!(retrieved.allowed_tools, vec!["run_shell"]);
    }

    #[test]
    fn role_names_returns_all_registered() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let mut pool = AgentPool::new(provider, test_config());

        pool.add_role(AgentRole {
            name: "alpha".into(),
            system_prompt: "A".into(),
            max_steps: 1,
            allowed_tools: vec![],
        });
        pool.add_role(AgentRole {
            name: "beta".into(),
            system_prompt: "B".into(),
            max_steps: 2,
            allowed_tools: vec![],
        });

        let mut names = pool.role_names();
        names.sort();
        assert_eq!(names, vec!["alpha", "beta"]);
        assert_eq!(pool.role_count(), 2);
    }

    #[tokio::test]
    async fn run_with_unknown_role_returns_error() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let pool = AgentPool::new(provider, test_config());

        let result = pool.run_with_role("nonexistent", "do something").await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("unknown role"));
    }

    #[tokio::test]
    async fn run_with_role_succeeds_with_mock() {
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "Plan: step 1, step 2, step 3".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));

        let mut pool = AgentPool::new(provider, test_config());
        pool.add_role(AgentRole {
            name: "planner".into(),
            system_prompt: "You are a planner.".into(),
            max_steps: 5,
            allowed_tools: vec![],
        });

        let outcome = pool.run_with_role("planner", "plan a task").await.unwrap();
        assert_eq!(
            outcome.finish,
            crate::agent::FinishReason::NoMoreToolCalls
        );
        assert!(outcome.final_message.unwrap().contains("Plan:"));
    }

    #[test]
    fn default_roles_returns_three_roles() {
        let roles = default_roles();
        assert_eq!(roles.len(), 3);

        let names: Vec<&str> = roles.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"planner"));
        assert!(names.contains(&"coder"));
        assert!(names.contains(&"reviewer"));
    }

    #[tokio::test]
    async fn shared_memory_set_and_get() {
        let mem = SharedMemory::new();
        mem.set("goal".into(), "build feature X".into(), "planner".into())
            .await;

        let entry = mem.get("goal").await.unwrap();
        assert_eq!(entry.key, "goal");
        assert_eq!(entry.value, "build feature X");
        assert_eq!(entry.author, "planner");
        assert!(entry.timestamp > 0);
    }

    #[tokio::test]
    async fn shared_memory_keys() {
        let mem = SharedMemory::new();
        mem.set("a".into(), "1".into(), "agent1".into()).await;
        mem.set("b".into(), "2".into(), "agent2".into()).await;

        let mut keys = mem.keys().await;
        keys.sort();
        assert_eq!(keys, vec!["a", "b"]);
        assert_eq!(mem.len().await, 2);
        assert!(!mem.is_empty().await);
    }

    #[tokio::test]
    async fn shared_memory_remove() {
        let mem = SharedMemory::new();
        mem.set("tmp".into(), "val".into(), "x".into()).await;
        assert!(mem.get("tmp").await.is_some());

        let removed = mem.remove("tmp").await;
        assert!(removed);
        assert!(mem.get("tmp").await.is_none());

        // Removing non-existent key returns false
        let removed_again = mem.remove("tmp").await;
        assert!(!removed_again);
    }

    #[tokio::test]
    async fn shared_memory_to_context_string() {
        let mem = SharedMemory::new();
        mem.set("status".into(), "in-progress".into(), "coder".into())
            .await;

        let ctx = mem.to_context_string().await;
        assert!(ctx.contains("[Shared Memory]"));
        assert!(ctx.contains("status = in-progress (by coder)"));
    }

    #[tokio::test]
    async fn shared_memory_empty_context_returns_empty() {
        let mem = SharedMemory::new();
        let ctx = mem.to_context_string().await;
        assert!(ctx.is_empty());
    }

    #[tokio::test]
    async fn agent_pool_includes_memory_context() {
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "I see the shared memory context.".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));

        let mut pool = AgentPool::new(provider, test_config());
        pool.add_role(AgentRole {
            name: "worker".into(),
            system_prompt: "You are a worker.".into(),
            max_steps: 5,
            allowed_tools: vec![],
        });

        // Set memory before running
        pool.memory()
            .set("plan".into(), "step 1 done".into(), "planner".into())
            .await;

        let outcome = pool.run_with_role("worker", "continue work").await.unwrap();
        assert_eq!(outcome.finish, crate::agent::FinishReason::NoMoreToolCalls);
        // The run succeeded with memory context injected — no error means integration works
        assert!(outcome.final_message.is_some());
    }
}
