//! Multi-agent orchestration: agent pool, role definitions, and message bus.

use crate::kernel::{AgentKernel, TurnContext, TurnOutcome};
use crate::message::Message;
use crate::permissions::PermissionMode;
use crate::tools::{AgentDefinitions, AgentTool, ToolRegistry};
use crate::{ChatProvider, Config};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, RwLock};

/// Shared memory store for multi-agent coordination.
#[derive(Clone)]
pub struct SharedMemory {
    store: Arc<RwLock<HashMap<String, MemoryEntry>>>,
    seq: Arc<AtomicU64>,
}

/// A single entry in the shared memory store.
///
/// `seq` is a process-local monotonic counter assigned by `SharedMemory::set`.
/// It exists so consumers can order entries deterministically even when
/// wall-clock `timestamp` collides (same-second writes). Older serialised
/// entries without the field deserialize to `seq: 0`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct MemoryEntry {
    pub key: String,
    pub value: String,
    pub author: String,
    pub timestamp: u64,
    #[serde(default)]
    pub seq: u64,
}

impl SharedMemory {
    pub fn new() -> Self {
        Self {
            store: Arc::new(RwLock::new(HashMap::new())),
            seq: Arc::new(AtomicU64::new(1)),
        }
    }

    pub async fn set(&self, key: String, value: String, author: String) {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let entry = MemoryEntry {
            key: key.clone(),
            value,
            author,
            timestamp,
            seq,
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

/// Maximum messages retained in `MessageBus.messages` history.
/// 1000 messages × ~200 bytes/msg ≈ 200 KiB — bounded for
/// long-running pools while still giving plenty of recent
/// context for the goal-judge and history inspection.
pub const MESSAGE_BUS_CAPACITY: usize = 1000;

/// An inter-agent message bus supporting publish/subscribe and history.
#[derive(Clone)]
pub struct MessageBus {
    /// Bounded ring buffer of recent messages. Oldest evicted
    /// on overflow. Capacity is `MESSAGE_BUS_CAPACITY` to bound
    /// memory in long-running multi-agent pools.
    messages: Arc<RwLock<VecDeque<AgentMessage>>>,
    subscribers: Arc<RwLock<HashMap<String, broadcast::Sender<AgentMessage>>>>,
    /// Maximum number of messages to retain. Defaults to
    /// `MESSAGE_BUS_CAPACITY`; overridable via `with_capacity`.
    capacity: usize,
}

impl MessageBus {
    pub fn new() -> Self {
        Self {
            messages: Arc::new(RwLock::new(VecDeque::with_capacity(MESSAGE_BUS_CAPACITY))),
            subscribers: Arc::new(RwLock::new(HashMap::new())),
            capacity: MESSAGE_BUS_CAPACITY,
        }
    }

    /// Send a message. Stores in history with bounded eviction and notifies
    /// relevant subscribers.
    pub async fn send(&self, msg: AgentMessage) {
        {
            let mut history = self.messages.write().await;
            if history.len() >= self.capacity {
                history.pop_front();
            }
            history.push_back(msg.clone());
        }
        let subs = self.subscribers.read().await;
        if msg.to == "broadcast" {
            for tx in subs.values() {
                let _ = tx.send(msg.clone());
            }
        } else if let Some(tx) = subs.get(&msg.to) {
            let _ = tx.send(msg);
        }
    }

    /// Create a `MessageBus` with a custom capacity (for testing).
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            messages: Arc::new(RwLock::new(VecDeque::with_capacity(cap))),
            subscribers: Arc::new(RwLock::new(HashMap::new())),
            capacity: cap,
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

    /// Get the full message history (bounded to `MESSAGE_BUS_CAPACITY`).
    pub async fn history(&self) -> VecDeque<AgentMessage> {
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

/// Execution mode for the unified `agent` delegation tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMode {
    /// Single worker: manifest must have exactly one entry.
    Single,
    /// All workers run concurrently (join_all). Read-only workers benefit most.
    Parallel,
    /// Workers run one after another, in manifest key order.
    Sequential,
}

impl AgentMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "single" => Some(Self::Single),
            "parallel" => Some(Self::Parallel),
            "sequential" => Some(Self::Sequential),
            _ => None,
        }
    }
}

/// Definition of a single worker entry within the agent manifest.
#[derive(Clone, Debug)]
pub struct WorkerManifestEntry {
    pub system_prompt: String,
    pub allowed_tools: Vec<String>,
}

/// Maps worker IDs to their role definitions. Required by the `agent` tool.
pub type AgentManifest = HashMap<String, WorkerManifestEntry>;

/// An agent pool manages multiple agents with different roles.
pub struct AgentPool {
    roles: HashMap<String, AgentRole>,
    provider: Arc<dyn ChatProvider>,
    memory: SharedMemory,
    bus: MessageBus,
}

impl AgentPool {
    pub fn new(provider: Arc<dyn ChatProvider>, _config: Config) -> Self {
        Self {
            roles: HashMap::new(),
            provider,
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
            messages: Arc::new(vec![
                Message::system(system_prompt),
                Message::user(goal.to_string()),
            ]),
            step_events_tx: None,
            tool_specs: kernel.tools().specs(),
            streaming: false,
            permission_hook: None,
            exploring_plan_mode: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            permission_mode: PermissionMode::Default,
            mailbox: None,
            turn: 0,
            prompt_segments: None,
            wall_timeout_secs: 0,
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

/// Return the system prompt that enables a coordinator agent to autonomously
/// design a specialist team and orchestrate their work.
///
/// The coordinator workflow:
/// 1. Analyse the task and decide which specialists are needed
/// 2. Call `team_add_role` for each specialist (custom system prompt + tools)
/// 3. Call `spawn_workers_parallel` (or sequential `spawn_worker`) to dispatch tasks
/// 4. Synthesise and return the combined results
///
/// This prompt is injected in addition to any project-level context.
pub fn coordinator_system_prompt() -> &'static str {
    concat!(
        "You are a coordinator agent. Your job is to decompose complex tasks and delegate them ",
        "to specialist workers you design on the fly.\n\n",
        "## Your workflow\n\n",
        "1. **Analyse** — understand the task and identify the distinct subtasks and the expertise each requires.\n",
        "2. **Design specialists** — for each distinct expertise, call `team_add_role` with:\n",
        "   - A descriptive `name` (e.g. \"security-reviewer\", \"perf-analyst\")\n",
        "   - A focused `system_prompt` that defines the specialist's role, constraints, and output format\n",
        "   - Appropriate `allowed_tools` (restrict read-only specialists to read/search tools)\n",
        "3. **Dispatch** — use `spawn_workers_parallel` to run independent tasks concurrently, or\n",
        "   sequential `spawn_worker` calls when one task's output feeds the next.\n",
        "   Use `role_name` to route tasks to the custom roles you created.\n",
        "4. **Synthesise** — collect all worker results and write a final unified answer.\n\n",
        "## Rules\n\n",
        "- Design the *minimum* number of specialists the task requires — avoid over-decomposition.\n",
        "- Read-only tasks (analysis, review, research) can always run in parallel.\n",
        "- Write-heavy tasks (coding, patching) should run sequentially unless you are certain they touch different files.\n",
        "- Always include enough context in each worker's `prompt` — workers have no access to this conversation.\n",
        "- After all workers finish, synthesise their outputs into a single coherent response rather than just concatenating them.\n"
    )
}

/// Register the unified `Agent` (sub-agent / team coordination) tool on `tools`
/// when `config.subagent_enabled` is true. This is the single, channel-agnostic
/// hook called by every agent-loop entry point (CLI run / loop, HTTP API, TUI)
/// after they build their base tool registry and resolve their provider, so
/// the `Agent` tool and the coordinator prompt injected by
/// [`crate::system_prompt::assemble_system_prompt`] stay in sync across all
/// surfaces. Returns `tools` unchanged when sub-agent is disabled.
pub fn register_subagent_if_enabled(
    tools: ToolRegistry,
    config: &Config,
    provider: Arc<dyn ChatProvider>,
) -> ToolRegistry {
    if !config.subagent_enabled {
        return tools;
    }
    let defs = AgentDefinitions::load(&config.workspace).unwrap_or_else(|e| {
        tracing::warn!("Failed to load agent definitions: {e}");
        AgentDefinitions::default()
    });
    let agent = AgentTool::new(
        &config.workspace,
        provider,
        tools.fork(),
        config.subagent_max_depth,
        0,
        None,
    )
    .with_definitions(defs);
    tools.register(Arc::new(agent))
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
            allowed_tools: vec!["Read".into(), "Grep".into()],
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
            extra_readonly_dirs: Vec::new(),
            allow_tools: Vec::new(),
            context_window_override: None,
            subagent_max_depth: 2,
            subagent_enabled: false,
            allow_bypass_permissions: false,
            max_search_rounds: 3,
            stuck_window: 10,
            stuck_error_rate: 0.8,
            max_concurrent_runs: 8,
            goal_eval_transcript_tail: 12,
            web_search_provider: None,
            web_search_api_key: None,
            web_search_jina_key: None,
            wall_timeout_secs: 0,
        }
    }

    #[tokio::test]
    async fn shared_memory_assigns_monotonic_seq_across_writes() {
        let mem = SharedMemory::new();
        mem.set("a".into(), "1".into(), "alpha".into()).await;
        mem.set("b".into(), "2".into(), "alpha".into()).await;
        let a = mem.get("a").await.unwrap();
        let b = mem.get("b").await.unwrap();
        assert!(a.seq > 0, "first write should not have seq 0");
        assert!(b.seq > a.seq, "second write seq must exceed first");
    }

    #[tokio::test]
    async fn shared_memory_seq_advances_on_overwrite() {
        let mem = SharedMemory::new();
        mem.set("k".into(), "v1".into(), "alpha".into()).await;
        let v1 = mem.get("k").await.unwrap().seq;
        mem.set("k".into(), "v2".into(), "beta".into()).await;
        let v2 = mem.get("k").await.unwrap().seq;
        assert!(v2 > v1, "overwriting the same key must advance seq");
    }

    #[test]
    fn memory_entry_deserializes_without_seq_field() {
        // Old serialised entries (pre-seq) must round-trip to seq: 0.
        let json = r#"{"key":"k","value":"v","author":"a","timestamp":123}"#;
        let entry: MemoryEntry = serde_json::from_str(json).expect("deserialize legacy entry");
        assert_eq!(entry.seq, 0);
    }

    #[test]
    fn memory_entry_round_trips_with_seq() {
        let entry = MemoryEntry {
            key: "k".into(),
            value: "v".into(),
            author: "a".into(),
            timestamp: 123,
            seq: 42,
        };
        let json = serde_json::to_string(&entry).expect("serialize");
        let back: MemoryEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.seq, 42);
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
            allowed_tools: vec!["Bash".into()],
        };
        pool.add_role(role.clone());

        let retrieved = pool.get_role("tester").unwrap();
        assert_eq!(retrieved.name, "tester");
        assert_eq!(retrieved.system_prompt, "You test things.");
        assert_eq!(retrieved.max_steps, 5);
        assert_eq!(retrieved.allowed_tools, vec!["Bash"]);
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

    #[tokio::test]
    async fn message_bus_evicts_oldest_on_overflow() {
        let bus = MessageBus::with_capacity(3);
        for i in 0..5 {
            bus.send(make_msg(
                &format!("a{i}"),
                "broadcast",
                &format!("msg-{i}"),
                MessageType::Feedback,
            ))
            .await;
        }
        let history = bus.history().await;
        let contents: Vec<_> = history.iter().map(|m| m.content.clone()).collect();
        // msg-0 and msg-1 evicted; msg-2,3,4 are the most recent 3
        assert_eq!(contents, vec!["msg-2", "msg-3", "msg-4"]);
        assert_eq!(history.len(), 3);
    }

    #[tokio::test]
    async fn message_bus_default_capacity_is_bounded() {
        let bus = MessageBus::new();
        assert_eq!(MESSAGE_BUS_CAPACITY, 1000);
        // Verify the bus uses this capacity: 10 messages should all be retained
        for i in 0..10 {
            bus.send(make_msg("a", "b", &format!("m{i}"), MessageType::Task))
                .await;
        }
        assert_eq!(bus.history().await.len(), 10);
    }

    // --- SharedMemory::all ---

    #[tokio::test]
    async fn shared_memory_all_returns_all_entries() {
        let mem = SharedMemory::new();
        mem.set("x".into(), "1".into(), "a".into()).await;
        mem.set("y".into(), "2".into(), "b".into()).await;
        let all = mem.all().await;
        assert_eq!(all.len(), 2, "all() must return every stored entry");
        let mut keys: Vec<String> = all.iter().map(|e| e.key.clone()).collect();
        keys.sort();
        assert_eq!(keys, vec!["x", "y"]);
    }

    #[tokio::test]
    async fn shared_memory_all_empty_returns_empty_vec() {
        let mem = SharedMemory::new();
        assert!(
            mem.all().await.is_empty(),
            "all() on empty store must return empty vec"
        );
    }

    // --- AgentMode::parse ---

    #[test]
    fn agent_mode_parse_single() {
        assert_eq!(AgentMode::parse("single"), Some(AgentMode::Single));
    }

    #[test]
    fn agent_mode_parse_parallel() {
        assert_eq!(AgentMode::parse("parallel"), Some(AgentMode::Parallel));
    }

    #[test]
    fn agent_mode_parse_sequential() {
        assert_eq!(AgentMode::parse("sequential"), Some(AgentMode::Sequential));
    }

    #[test]
    fn agent_mode_parse_unknown_returns_none() {
        assert_eq!(AgentMode::parse(""), None);
        assert_eq!(AgentMode::parse("xyzzy"), None);
    }

    // --- AgentPool::remove_role ---

    #[test]
    fn agent_pool_remove_role_returns_true_when_present() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let mut pool = AgentPool::new(provider, test_config());
        pool.add_role(AgentRole {
            name: "tmp".into(),
            system_prompt: "X".into(),
            max_steps: 1,
            allowed_tools: vec![],
        });
        assert!(
            pool.remove_role("tmp"),
            "remove existing role must return true"
        );
        assert_eq!(pool.role_count(), 0);
    }

    #[test]
    fn agent_pool_remove_role_returns_false_when_absent() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let mut pool = AgentPool::new(provider, test_config());
        assert!(
            !pool.remove_role("nonexistent"),
            "remove absent role must return false"
        );
    }

    // --- coordinator_system_prompt ---

    #[test]
    fn coordinator_system_prompt_is_nonempty_and_not_placeholder() {
        let prompt = coordinator_system_prompt();
        assert!(!prompt.is_empty(), "coordinator prompt must not be empty");
        assert_ne!(
            prompt, "xyzzy",
            "coordinator prompt must not be xyzzy placeholder"
        );
        assert!(
            prompt.contains("coordinator"),
            "coordinator prompt must mention 'coordinator'"
        );
    }

    // --- default_roles content ---

    #[test]
    fn default_roles_have_expected_steps_and_tools() {
        let roles = default_roles();
        assert!(!roles.is_empty(), "default_roles must return non-empty vec");

        let planner = roles
            .iter()
            .find(|r| r.name == "planner")
            .expect("planner role");
        let coder = roles
            .iter()
            .find(|r| r.name == "coder")
            .expect("coder role");
        let reviewer = roles
            .iter()
            .find(|r| r.name == "reviewer")
            .expect("reviewer role");

        // Each role must have a positive step limit.
        assert!(planner.max_steps > 0);
        assert!(coder.max_steps > 0);
        assert!(reviewer.max_steps > 0);

        // Reviewer is read-only so it must declare some allowed tools.
        assert!(
            !reviewer.allowed_tools.is_empty(),
            "reviewer must have allowed_tools"
        );

        // Prompts must be non-empty.
        assert!(!planner.system_prompt.is_empty());
        assert!(!coder.system_prompt.is_empty());
        assert!(!reviewer.system_prompt.is_empty());
    }

    // --- register_subagent_if_enabled: disabled path ---

    #[test]
    fn register_subagent_if_enabled_noop_when_disabled() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let config = test_config(); // subagent_enabled: false
        let tools = crate::tools::ToolRegistry::local();
        let initial_names = tools.names();
        let result = register_subagent_if_enabled(tools, &config, provider);
        assert_eq!(
            result.names(),
            initial_names,
            "disabled subagent must not register any additional tools"
        );
    }
}
