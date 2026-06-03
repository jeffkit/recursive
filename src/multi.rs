//! Multi-agent orchestration: agent pool, role definitions, and message bus.

use crate::agent::PlanningMode;
use crate::kernel::{AgentKernel, TurnContext, TurnOutcome};
use crate::message::Message;
use crate::permissions::PermissionMode;
use crate::{Config, LlmProvider};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, RwLock};

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
            lines.push(format!(
                "- {} = {} (by {})",
                entry.key, entry.value, entry.author
            ));
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

// --- Inter-agent messaging ---

/// Message type for inter-agent communication.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub enum MessageType {
    Task,
    Result,
    Question,
    Feedback,
    Broadcast,
}

/// A message exchanged between agents via the message bus.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AgentMessage {
    pub id: String,
    pub from: String,
    pub to: String,
    pub content: String,
    pub msg_type: MessageType,
    pub timestamp: u64,
}

/// Generate a unique message ID using blake3 hash of timestamp + atomic counter.
fn generate_message_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let input = format!("msg-{}-{}", now.as_nanos(), count);
    let hash = blake3::hash(input.as_bytes());
    hash.to_hex()[..16].to_string()
}

/// Get current timestamp as seconds since UNIX epoch.
fn now_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// An inter-agent message bus supporting publish/subscribe and history.
#[derive(Clone)]
pub struct MessageBus {
    messages: Arc<RwLock<Vec<AgentMessage>>>,
    subscribers: Arc<RwLock<HashMap<String, broadcast::Sender<AgentMessage>>>>,
}

impl MessageBus {
    pub fn new() -> Self {
        Self {
            messages: Arc::new(RwLock::new(Vec::new())),
            subscribers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Send a message. Stores in history and notifies relevant subscribers.
    pub async fn send(&self, msg: AgentMessage) {
        self.messages.write().await.push(msg.clone());
        let subs = self.subscribers.read().await;
        if msg.to == "broadcast" {
            for tx in subs.values() {
                let _ = tx.send(msg.clone());
            }
        } else if let Some(tx) = subs.get(&msg.to) {
            let _ = tx.send(msg);
        }
    }

    /// Subscribe to messages for a given role. Returns a broadcast receiver.
    pub async fn subscribe(&self, role: &str) -> broadcast::Receiver<AgentMessage> {
        let mut subs = self.subscribers.write().await;
        let tx = subs.entry(role.to_string()).or_insert_with(|| {
            let (tx, _) = broadcast::channel(64);
            tx
        });
        tx.subscribe()
    }

    /// Get all messages addressed to this role (including broadcasts).
    pub async fn inbox(&self, role: &str) -> Vec<AgentMessage> {
        self.messages
            .read()
            .await
            .iter()
            .filter(|m| m.to == role || m.to == "broadcast")
            .cloned()
            .collect()
    }

    /// Get all messages sent by this role.
    pub async fn outbox(&self, role: &str) -> Vec<AgentMessage> {
        self.messages
            .read()
            .await
            .iter()
            .filter(|m| m.from == role)
            .cloned()
            .collect()
    }

    /// Get the full message history.
    pub async fn history(&self) -> Vec<AgentMessage> {
        self.messages.read().await.clone()
    }

    /// Clear all stored messages.
    pub async fn clear(&self) {
        self.messages.write().await.clear();
    }
}

impl Default for MessageBus {
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
    bus: MessageBus,
}

impl AgentPool {
    pub fn new(provider: Arc<dyn LlmProvider>, config: Config) -> Self {
        Self {
            roles: HashMap::new(),
            provider,
            config,
            memory: SharedMemory::new(),
            bus: MessageBus::new(),
        }
    }

    pub fn memory(&self) -> &SharedMemory {
        &self.memory
    }

    pub fn bus(&self) -> &MessageBus {
        &self.bus
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

    /// Remove a role from the pool.  Returns `true` if the role existed.
    pub fn remove_role(&mut self, name: &str) -> bool {
        self.roles.remove(name).is_some()
    }

    pub async fn run_with_role(
        &self,
        role_name: &str,
        goal: &str,
    ) -> Result<TurnOutcome, crate::Error> {
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

        let kernel = AgentKernel::builder()
            .llm(self.provider.clone())
            .max_steps(role.max_steps)
            .build()?;

        let ctx = TurnContext {
            messages: vec![
                Message::system(system_prompt),
                Message::user(goal.to_string()),
            ],
            step_events_tx: None,
            plan_confirmed: false,
            plan_buffer: None,
            tool_specs: kernel.tools().specs(),
            streaming: false,
            permission_hook: None,
            planning_mode: PlanningMode::default(),
            exploring_plan_mode: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            permission_mode: PermissionMode::Default,
            mailbox: None,
        };

        kernel.run(ctx).await
    }

    /// Send a task message from one agent role to another.
    pub async fn send_task(&self, from: &str, to: &str, content: &str) {
        self.bus
            .send(AgentMessage {
                id: generate_message_id(),
                from: from.to_string(),
                to: to.to_string(),
                content: content.to_string(),
                msg_type: MessageType::Task,
                timestamp: now_timestamp(),
            })
            .await;
    }

    /// Send a result message from one agent role to another.
    pub async fn send_result(&self, from: &str, to: &str, content: &str) {
        self.bus
            .send(AgentMessage {
                id: generate_message_id(),
                from: from.to_string(),
                to: to.to_string(),
                content: content.to_string(),
                msg_type: MessageType::Result,
                timestamp: now_timestamp(),
            })
            .await;
    }
}

/// A pipeline chains multiple agent roles in sequence.
pub struct Pipeline {
    stages: Vec<String>,
}

impl Pipeline {
    pub fn new(stages: Vec<String>) -> Self {
        Self { stages }
    }

    /// Execute the pipeline. Each stage's output becomes next stage's input.
    pub async fn execute(
        &self,
        pool: &AgentPool,
        initial_goal: &str,
    ) -> Result<PipelineResult, crate::Error> {
        let mut current_input = initial_goal.to_string();
        let mut stage_outcomes = Vec::new();

        for role_name in &self.stages {
            let outcome = pool.run_with_role(role_name, &current_input).await?;

            let output = outcome
                .new_messages
                .iter()
                .rev()
                .find(|m| m.role == crate::message::Role::Assistant)
                .map(|m| m.content.clone())
                .unwrap_or_default();

            stage_outcomes.push(StageOutcome {
                role: role_name.clone(),
                output: output.clone(),
                steps: outcome.steps,
            });

            current_input = output;
        }

        Ok(PipelineResult {
            stages: stage_outcomes,
        })
    }
}

/// The result of running a full pipeline.
#[derive(Debug)]
pub struct PipelineResult {
    pub stages: Vec<StageOutcome>,
}

/// The outcome of a single pipeline stage.
#[derive(Debug)]
pub struct StageOutcome {
    pub role: String,
    pub output: String,
    pub steps: usize,
}

impl PipelineResult {
    pub fn final_output(&self) -> &str {
        self.stages.last().map(|s| s.output.as_str()).unwrap_or("")
    }
    pub fn stage_count(&self) -> usize {
        self.stages.len()
    }
}

/// A team orchestrator uses a lead agent to dynamically assign work
/// to specialist agents.
pub struct TeamOrchestrator {
    lead_role: String,
    available_roles: Vec<String>,
}

impl TeamOrchestrator {
    pub fn new(lead_role: String, available_roles: Vec<String>) -> Self {
        Self {
            lead_role,
            available_roles,
        }
    }

    /// Run orchestration: lead plans delegations, specialists execute, lead synthesizes.
    pub async fn run(&self, pool: &AgentPool, goal: &str) -> Result<TeamResult, crate::Error> {
        // Phase 1: Ask lead to plan
        let delegation_prompt = format!(
            "{}\n\nAvailable specialists: {}\n\nTo delegate, use: DELEGATE:<role>:<task>\nWhen done, provide your final answer.",
            goal,
            self.available_roles.join(", ")
        );

        let lead_outcome = pool
            .run_with_role(&self.lead_role, &delegation_prompt)
            .await?;
        let lead_response = lead_outcome
            .new_messages
            .iter()
            .rev()
            .find(|m| m.role == crate::message::Role::Assistant)
            .map(|m| m.content.clone())
            .unwrap_or_default();

        let delegations = parse_delegations(&lead_response);

        // Phase 2: Execute delegations
        let mut delegation_results = Vec::new();
        for (role, task) in &delegations {
            if self.available_roles.contains(role) {
                match pool.run_with_role(role, task).await {
                    Ok(outcome) => {
                        let result = outcome
                            .new_messages
                            .iter()
                            .rev()
                            .find(|m| m.role == crate::message::Role::Assistant)
                            .map(|m| m.content.clone())
                            .unwrap_or_default();
                        delegation_results.push(DelegationResult {
                            role: role.clone(),
                            task: task.clone(),
                            output: result,
                            success: true,
                        });
                    }
                    Err(e) => {
                        delegation_results.push(DelegationResult {
                            role: role.clone(),
                            task: task.clone(),
                            output: format!("Error: {e}"),
                            success: false,
                        });
                    }
                }
            }
        }

        // Phase 3: If delegations happened, synthesize
        let final_output = if delegation_results.is_empty() {
            lead_response
        } else {
            let results_summary = delegation_results
                .iter()
                .map(|r| {
                    format!(
                        "- {} ({}): {}",
                        r.role,
                        if r.success { "ok" } else { "failed" },
                        r.output
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");

            let synthesis_prompt = format!(
                "Results from delegated tasks:\n\n{}\n\nProvide a final synthesis.",
                results_summary
            );

            let synthesis = pool
                .run_with_role(&self.lead_role, &synthesis_prompt)
                .await?;
            synthesis
                .new_messages
                .iter()
                .rev()
                .find(|m| m.role == crate::message::Role::Assistant)
                .map(|m| m.content.clone())
                .unwrap_or_default()
        };

        Ok(TeamResult {
            delegations: delegation_results,
            final_output,
        })
    }
}

/// Parse "DELEGATE:<role>:<task>" lines from text.
pub fn parse_delegations(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("DELEGATE:") {
                let parts: Vec<&str> = rest.splitn(2, ':').collect();
                if parts.len() == 2 {
                    Some((parts[0].to_string(), parts[1].to_string()))
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect()
}

/// The result of a team orchestration run.
#[derive(Debug)]
pub struct TeamResult {
    pub delegations: Vec<DelegationResult>,
    pub final_output: String,
}

/// The result of a single delegation to a specialist.
#[derive(Debug)]
pub struct DelegationResult {
    pub role: String,
    pub task: String,
    pub output: String,
    pub success: bool,
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
            preset: None,
            max_steps: 32,
            temperature: 0.2,
            system_prompt: String::new(),
            retry_max: 2,
            retry_initial_backoff_secs: 1,
            retry_max_backoff_secs: 8,
            shell_timeout_secs: 300,
            headless: false,
            memory_summary_limit: 5,
            thinking_budget: None,
            session_name: None,
            max_budget_usd: None,
            extra_dirs: Vec::new(),
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
            outcome.finish_reason,
            crate::agent::FinishReason::NoMoreToolCalls
        );
        assert!(outcome.final_text.unwrap().contains("Plan:"));
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
        assert_eq!(
            outcome.finish_reason,
            crate::agent::FinishReason::NoMoreToolCalls
        );
        // The run succeeded with memory context injected — no error means integration works
        assert!(outcome.final_text.is_some());
    }

    // --- MessageBus tests ---

    fn make_msg(from: &str, to: &str, content: &str, msg_type: MessageType) -> AgentMessage {
        AgentMessage {
            id: generate_message_id(),
            from: from.to_string(),
            to: to.to_string(),
            content: content.to_string(),
            msg_type,
            timestamp: now_timestamp(),
        }
    }

    #[tokio::test]
    async fn message_bus_send_and_inbox() {
        let bus = MessageBus::new();
        let msg = make_msg("planner", "coder", "implement feature X", MessageType::Task);
        bus.send(msg).await;

        let inbox = bus.inbox("coder").await;
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].content, "implement feature X");
        assert_eq!(inbox[0].from, "planner");
        assert_eq!(inbox[0].msg_type, MessageType::Task);

        // Other roles see empty inbox
        let empty = bus.inbox("reviewer").await;
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn message_bus_outbox() {
        let bus = MessageBus::new();
        bus.send(make_msg(
            "coder",
            "reviewer",
            "done coding",
            MessageType::Result,
        ))
        .await;
        bus.send(make_msg(
            "coder",
            "planner",
            "need clarification",
            MessageType::Question,
        ))
        .await;
        bus.send(make_msg(
            "planner",
            "coder",
            "here is the plan",
            MessageType::Task,
        ))
        .await;

        let outbox = bus.outbox("coder").await;
        assert_eq!(outbox.len(), 2);
        assert!(outbox.iter().all(|m| m.from == "coder"));

        let planner_outbox = bus.outbox("planner").await;
        assert_eq!(planner_outbox.len(), 1);
    }

    #[tokio::test]
    async fn message_bus_broadcast_reaches_all() {
        let bus = MessageBus::new();
        bus.send(make_msg(
            "admin",
            "broadcast",
            "system update",
            MessageType::Broadcast,
        ))
        .await;

        let coder_inbox = bus.inbox("coder").await;
        let reviewer_inbox = bus.inbox("reviewer").await;
        let planner_inbox = bus.inbox("planner").await;

        assert_eq!(coder_inbox.len(), 1);
        assert_eq!(reviewer_inbox.len(), 1);
        assert_eq!(planner_inbox.len(), 1);
        assert_eq!(coder_inbox[0].content, "system update");
    }

    #[tokio::test]
    async fn message_bus_subscribe_receives() {
        let bus = MessageBus::new();
        let mut rx = bus.subscribe("coder").await;

        // Send after subscribing
        let msg = make_msg("planner", "coder", "task for you", MessageType::Task);
        bus.send(msg).await;

        let received = rx.recv().await.unwrap();
        assert_eq!(received.content, "task for you");
        assert_eq!(received.from, "planner");
    }

    #[tokio::test]
    async fn message_bus_history() {
        let bus = MessageBus::new();
        bus.send(make_msg("a", "b", "msg1", MessageType::Task))
            .await;
        bus.send(make_msg("b", "a", "msg2", MessageType::Result))
            .await;
        bus.send(make_msg("a", "broadcast", "msg3", MessageType::Broadcast))
            .await;

        let history = bus.history().await;
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].content, "msg1");
        assert_eq!(history[1].content, "msg2");
        assert_eq!(history[2].content, "msg3");
    }

    #[tokio::test]
    async fn message_bus_clear() {
        let bus = MessageBus::new();
        bus.send(make_msg("a", "b", "hello", MessageType::Task))
            .await;
        assert_eq!(bus.history().await.len(), 1);

        bus.clear().await;
        assert!(bus.history().await.is_empty());
        assert!(bus.inbox("b").await.is_empty());
    }

    #[tokio::test]
    async fn agent_pool_send_task_convenience() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let pool = AgentPool::new(provider, test_config());

        pool.send_task("planner", "coder", "build module Y").await;
        pool.send_result("coder", "planner", "module Y complete")
            .await;

        let inbox = pool.bus().inbox("coder").await;
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].content, "build module Y");
        assert_eq!(inbox[0].msg_type, MessageType::Task);

        let planner_inbox = pool.bus().inbox("planner").await;
        assert_eq!(planner_inbox.len(), 1);
        assert_eq!(planner_inbox[0].content, "module Y complete");
        assert_eq!(planner_inbox[0].msg_type, MessageType::Result);

        let history = pool.bus().history().await;
        assert_eq!(history.len(), 2);
    }

    // --- Pipeline tests ---

    #[tokio::test]
    async fn pipeline_empty_returns_empty_result() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let pool = AgentPool::new(provider, test_config());

        let pipeline = Pipeline::new(vec![]);
        let result = pipeline.execute(&pool, "hello").await.unwrap();
        assert_eq!(result.stage_count(), 0);
        assert_eq!(result.final_output(), "");
    }

    #[tokio::test]
    async fn pipeline_single_stage() {
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "stage one output".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));

        let mut pool = AgentPool::new(provider, test_config());
        pool.add_role(AgentRole {
            name: "writer".into(),
            system_prompt: "You write things.".into(),
            max_steps: 5,
            allowed_tools: vec![],
        });

        let pipeline = Pipeline::new(vec!["writer".into()]);
        let result = pipeline.execute(&pool, "write something").await.unwrap();

        assert_eq!(result.stage_count(), 1);
        assert_eq!(result.stages[0].role, "writer");
        assert_eq!(result.stages[0].output, "stage one output");
        assert_eq!(result.final_output(), "stage one output");
    }

    #[tokio::test]
    async fn pipeline_multi_stage_passes_output() {
        let provider = Arc::new(MockProvider::new(vec![
            Completion {
                content: "draft text".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "polished text".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));

        let mock_ref = provider.clone();
        let mut pool = AgentPool::new(provider, test_config());
        pool.add_role(AgentRole {
            name: "drafter".into(),
            system_prompt: "You draft text.".into(),
            max_steps: 5,
            allowed_tools: vec![],
        });
        pool.add_role(AgentRole {
            name: "editor".into(),
            system_prompt: "You polish text.".into(),
            max_steps: 5,
            allowed_tools: vec![],
        });

        let pipeline = Pipeline::new(vec!["drafter".into(), "editor".into()]);
        let result = pipeline.execute(&pool, "original goal").await.unwrap();

        assert_eq!(result.stage_count(), 2);
        assert_eq!(result.stages[0].output, "draft text");
        assert_eq!(result.stages[1].output, "polished text");
        assert_eq!(result.final_output(), "polished text");

        // Verify second stage received first stage's output as its goal
        let calls = mock_ref.calls();
        assert_eq!(calls.len(), 2);
        // The second call's user message should contain "draft text"
        let second_call_user_msg = calls[1]
            .iter()
            .find(|m| m.role == crate::message::Role::User);
        assert!(second_call_user_msg.unwrap().content.contains("draft text"));
    }

    #[tokio::test]
    async fn pipeline_fails_on_unknown_role() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let pool = AgentPool::new(provider, test_config());

        let pipeline = Pipeline::new(vec!["nonexistent".into()]);
        let result = pipeline.execute(&pool, "hello").await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("unknown role"));
    }

    #[tokio::test]
    async fn pipeline_final_output() {
        let provider = Arc::new(MockProvider::new(vec![
            Completion {
                content: "first".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "second".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "final answer".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));

        let mut pool = AgentPool::new(provider, test_config());
        pool.add_role(AgentRole {
            name: "a".into(),
            system_prompt: "A".into(),
            max_steps: 5,
            allowed_tools: vec![],
        });
        pool.add_role(AgentRole {
            name: "b".into(),
            system_prompt: "B".into(),
            max_steps: 5,
            allowed_tools: vec![],
        });
        pool.add_role(AgentRole {
            name: "c".into(),
            system_prompt: "C".into(),
            max_steps: 5,
            allowed_tools: vec![],
        });

        let pipeline = Pipeline::new(vec!["a".into(), "b".into(), "c".into()]);
        let result = pipeline.execute(&pool, "start").await.unwrap();

        assert_eq!(result.final_output(), "final answer");
        assert_eq!(result.stage_count(), 3);
    }

    // --- TeamOrchestrator / parse_delegations tests ---

    #[test]
    fn parse_delegations_extracts_role_and_task() {
        let text = "DELEGATE:coder:write hello";
        let result = parse_delegations(text);
        assert_eq!(
            result,
            vec![("coder".to_string(), "write hello".to_string())]
        );
    }

    #[test]
    fn parse_delegations_ignores_non_delegation() {
        let text = "Here is my plan:\n- Think about it\nDELEGATE:coder:implement feature\nSome other text\nDELEGATE:reviewer:check code";
        let result = parse_delegations(text);
        assert_eq!(result.len(), 2);
        assert_eq!(
            result[0],
            ("coder".to_string(), "implement feature".to_string())
        );
        assert_eq!(
            result[1],
            ("reviewer".to_string(), "check code".to_string())
        );
    }

    #[test]
    fn parse_delegations_handles_colons_in_task() {
        let text = "DELEGATE:coder:write file:test.rs";
        let result = parse_delegations(text);
        assert_eq!(
            result,
            vec![("coder".to_string(), "write file:test.rs".to_string())]
        );
    }

    #[tokio::test]
    async fn orchestrator_no_delegations_returns_lead_response() {
        // Lead responds without any DELEGATE lines
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "I will handle this myself. The answer is 42.".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));

        let mut pool = AgentPool::new(provider, test_config());
        pool.add_role(AgentRole {
            name: "lead".into(),
            system_prompt: "You are the lead.".into(),
            max_steps: 5,
            allowed_tools: vec![],
        });
        pool.add_role(AgentRole {
            name: "coder".into(),
            system_prompt: "You code.".into(),
            max_steps: 5,
            allowed_tools: vec![],
        });

        let orchestrator = TeamOrchestrator::new("lead".into(), vec!["coder".into()]);
        let result = orchestrator
            .run(&pool, "What is the meaning of life?")
            .await
            .unwrap();

        assert!(result.delegations.is_empty());
        assert_eq!(
            result.final_output,
            "I will handle this myself. The answer is 42."
        );
    }

    #[tokio::test]
    async fn orchestrator_with_delegations_executes_them() {
        // Completions: 1) lead delegates, 2) specialist responds, 3) lead synthesizes
        let provider = Arc::new(MockProvider::new(vec![
            Completion {
                content: "Let me delegate this.\nDELEGATE:coder:write a hello world program".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "fn main() { println!(\"Hello, world!\"); }".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "The coder produced a working hello world program in Rust.".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));

        let mut pool = AgentPool::new(provider, test_config());
        pool.add_role(AgentRole {
            name: "lead".into(),
            system_prompt: "You are the lead.".into(),
            max_steps: 5,
            allowed_tools: vec![],
        });
        pool.add_role(AgentRole {
            name: "coder".into(),
            system_prompt: "You write code.".into(),
            max_steps: 5,
            allowed_tools: vec![],
        });

        let orchestrator = TeamOrchestrator::new("lead".into(), vec!["coder".into()]);
        let result = orchestrator
            .run(&pool, "Create a hello world program")
            .await
            .unwrap();

        assert_eq!(result.delegations.len(), 1);
        assert_eq!(result.delegations[0].role, "coder");
        assert_eq!(result.delegations[0].task, "write a hello world program");
        assert!(result.delegations[0].success);
        assert!(result.delegations[0].output.contains("Hello, world!"));
        assert_eq!(
            result.final_output,
            "The coder produced a working hello world program in Rust."
        );
    }
}
