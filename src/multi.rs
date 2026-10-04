//! Multi-agent orchestration: agent pool, role definitions, and message bus.

use crate::kernel::{AgentKernel, TurnContext, TurnOutcome};
use crate::message::Message;
use crate::permissions::PermissionMode;
use crate::storage::StorageBackend;
use crate::tasks::TaskRegistry;
use crate::tools::{
    AgentDefinitions, AgentTool, ListWorkersTool, SendMessageTool, ToolRegistry, WorkerRegistry,
};
#[cfg(feature = "coordinator-mode")]
use crate::tools::{
    TaskCreateTool, TaskGetTool, TaskListTool, TaskOutputTool, TaskStopTool, TaskUpdateTool,
};
use crate::{ChatProvider, Config};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, RwLock};

/// Storage keys under which the collaboration state persists. Both live in
/// the backend's memory-key namespace (`LocalStorageBackend` writes them to
/// `<root>/.recursive/memory/<key>`; Redis/S3 backends use `memory/<key>`).
pub(crate) const SHARED_MEMORY_KEY: &str = "multi/shared-memory.json";
pub(crate) const MESSAGE_BUS_KEY: &str = "multi/message-bus.json";

/// Best-effort persist helper: log-and-degrade. Persistence failures must
/// never break an in-flight collaboration — the in-memory copy stays the
/// source of truth for the current process; the disk copy is the
/// crash-recovery snapshot.
async fn persist_json(backend: &Arc<dyn StorageBackend>, key: &str, value: &impl serde::Serialize) {
    match serde_json::to_string(value) {
        Ok(json) => {
            if let Err(e) = backend.save_memory(key, &json).await {
                tracing::warn!("multi: failed to persist {key}: {e}");
            }
        }
        Err(e) => tracing::warn!("multi: failed to serialize {key}: {e}"),
    }
}

/// Restore `SharedMemory` entries written by a previous process. Best-effort:
/// a missing/corrupt snapshot yields an empty store, never an error.
async fn restore_shared_memory(backend: &Arc<dyn StorageBackend>) -> Vec<MemoryEntry> {
    match backend.load_memory(SHARED_MEMORY_KEY).await {
        Ok(Some(json)) => match serde_json::from_str::<Vec<MemoryEntry>>(&json) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!("multi: corrupt shared-memory snapshot ignored: {e}");
                Vec::new()
            }
        },
        Ok(None) => Vec::new(),
        Err(e) => {
            tracing::warn!("multi: failed to load shared-memory snapshot: {e}");
            Vec::new()
        }
    }
}

/// Restore `MessageBus` history written by a previous process. Best-effort in
/// the same way as [`restore_shared_memory`].
async fn restore_bus_history(backend: &Arc<dyn StorageBackend>) -> Vec<AgentMessage> {
    match backend.load_memory(MESSAGE_BUS_KEY).await {
        Ok(Some(json)) => match serde_json::from_str::<Vec<AgentMessage>>(&json) {
            Ok(msgs) => msgs,
            Err(e) => {
                tracing::warn!("multi: corrupt message-bus snapshot ignored: {e}");
                Vec::new()
            }
        },
        Ok(None) => Vec::new(),
        Err(e) => {
            tracing::warn!("multi: failed to load message-bus snapshot: {e}");
            Vec::new()
        }
    }
}

/// Per-turn cancellation token slot (issue #40). Hosts that mint a fresh
/// CancellationToken at each turn start (the TUI) store the current token
/// here instead of attaching one static token; the `Agent` tool clones it
/// out at dispatch time so parallel workers receive a child token of the
/// CURRENT turn's interrupt token. A static `with_shutdown_token` token, if
/// attached, takes precedence.
pub type SharedTokenSlot = Arc<std::sync::Mutex<Option<tokio_util::sync::CancellationToken>>>;

/// Shared memory store for multi-agent coordination.
///
/// The in-memory map stays the hot path (same-process workers see writes
/// immediately). When a [`StorageBackend`] is attached
/// ([`SharedMemory::with_backend`]), every mutation is also written through
/// under a single storage key, so a crash/restart (or another coordinator
/// replica) can restore the last snapshot via [`SharedMemory::restore`].
#[derive(Clone)]
pub struct SharedMemory {
    store: Arc<RwLock<HashMap<String, MemoryEntry>>>,
    seq: Arc<AtomicU64>,
    backend: Option<Arc<dyn StorageBackend>>,
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
            backend: None,
        }
    }

    /// Attach a storage backend and enable write-through persistence.
    pub fn with_backend(mut self, backend: Arc<dyn StorageBackend>) -> Self {
        self.backend = Some(backend);
        self
    }

    /// Load the last persisted snapshot (if any) into the in-memory store.
    /// No-op when no backend is attached. Entries already in memory win:
    /// restore only fills keys that are absent, so a live store is never
    /// clobbered by a stale snapshot.
    pub async fn restore(&self) {
        let Some(backend) = &self.backend else {
            return;
        };
        let entries = restore_shared_memory(backend).await;
        if entries.is_empty() {
            return;
        }
        let mut store = self.store.write().await;
        let mut max_seq = 0u64;
        for entry in entries {
            max_seq = max_seq.max(entry.seq);
            store.entry(entry.key.clone()).or_insert(entry);
        }
        // Keep the seq counter ahead of every restored entry so new writes
        // keep ordering monotonic across the restart boundary.
        self.seq.fetch_max(max_seq + 1, Ordering::Relaxed);
    }

    /// Snapshot the current entries to the backend (best-effort). Called
    /// internally after every mutation; public so hosts can force a flush
    /// before shutdown.
    pub async fn persist(&self) {
        if let Some(backend) = &self.backend {
            let snapshot = self
                .store
                .read()
                .await
                .values()
                .cloned()
                .collect::<Vec<_>>();
            persist_json(backend, SHARED_MEMORY_KEY, &snapshot).await;
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
        self.persist().await;
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
        let removed = self.store.write().await.remove(key).is_some();
        if removed {
            self.persist().await;
        }
        removed
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
///
/// The in-process ring buffer + broadcast channels stay the hot path. When a
/// [`StorageBackend`] is attached ([`MessageBus::with_backend`]), every send
/// also appends to a single persisted history key so the most recent
/// `capacity` messages survive a process restart
/// ([`MessageBus::restore`]).
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
    backend: Option<Arc<dyn StorageBackend>>,
}

impl MessageBus {
    pub fn new() -> Self {
        Self {
            messages: Arc::new(RwLock::new(VecDeque::with_capacity(MESSAGE_BUS_CAPACITY))),
            subscribers: Arc::new(RwLock::new(HashMap::new())),
            capacity: MESSAGE_BUS_CAPACITY,
            backend: None,
        }
    }

    /// Attach a storage backend and enable write-through history persistence.
    pub fn with_backend(mut self, backend: Arc<dyn StorageBackend>) -> Self {
        self.backend = Some(backend);
        self
    }

    /// Load the persisted history (if any) into the ring buffer. No-op when
    /// no backend is attached. Live in-memory messages always win: snapshot
    /// entries are merged in as "older" messages (prepended, deduped by
    /// message id, re-evicted down to `capacity`), so the buffer never
    /// exceeds capacity and live messages are never clobbered.
    pub async fn restore(&self) {
        let Some(backend) = self.backend.clone() else {
            return;
        };
        let snapshot = restore_bus_history(&backend).await;
        if snapshot.is_empty() {
            return;
        }
        let mut history = self.messages.write().await;
        let live_ids: std::collections::HashSet<String> =
            history.iter().map(|m| m.id.clone()).collect();
        // Insert oldest-first so chronology is preserved inside the ring.
        let mut missing: Vec<AgentMessage> = snapshot
            .into_iter()
            .filter(|m| !live_ids.contains(&m.id))
            .collect();
        missing.reverse();
        for msg in missing {
            history.push_front(msg);
        }
        while history.len() > self.capacity {
            history.pop_front();
        }
    }

    /// Snapshot the current history to the backend (best-effort). The write
    /// MERGES with the previously persisted snapshot (older entries first,
    /// deduped by message id, capped to `capacity`) so two coordinator
    /// replicas — or a restart that sends before restoring — never silently
    /// drop each other's messages. Called internally after every send;
    /// public so hosts can force a flush before shutdown.
    pub async fn persist(&self) {
        let Some(backend) = self.backend.clone() else {
            return;
        };
        let current: Vec<AgentMessage> = self.messages.read().await.iter().cloned().collect();
        if current.is_empty() {
            // Empty live history (fresh bus, or clear()) must OVERWRITE the
            // snapshot, not merge with it — otherwise a clear() followed by a
            // restart resurrects every previously persisted message. A fresh
            // (never-restored) bus also lands here once, which is harmless:
            // its snapshot is whatever another process persisted, and this
            // process has nothing to add.
            persist_json(&backend, MESSAGE_BUS_KEY, &Vec::<AgentMessage>::new()).await;
            return;
        }
        let persisted = restore_bus_history(&backend).await;
        let mut merged: Vec<AgentMessage> = Vec::with_capacity(persisted.len() + current.len());
        let mut seen = std::collections::HashSet::new();
        for msg in persisted.into_iter().chain(current) {
            if seen.insert(msg.id.clone()) {
                merged.push(msg);
            }
        }
        while merged.len() > self.capacity {
            merged.remove(0);
        }
        persist_json(&backend, MESSAGE_BUS_KEY, &merged).await;
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
        self.persist().await;
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
            backend: None,
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

    /// Clear all stored messages (in-memory + persisted snapshot).
    pub async fn clear(&self) {
        self.messages.write().await.clear();
        self.persist().await;
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
    /// Goal 399: wall-clock budget (seconds) inherited from the parent
    /// session's `Config`. Sub-agents previously ran with `0` (unlimited),
    /// so a wide manifest could pin admission permits indefinitely; they now
    /// inherit the parent's budget so the pool can never outlive the session
    /// that spawned it. 0 = parent was unlimited → sub-agents stay unlimited.
    wall_timeout_secs: u64,
    /// Goal #106: set once the persisted snapshot has been rehydrated. The
    /// registration site (`register_subagent_if_enabled`) is synchronous and
    /// cannot await, so the restore is deferred to the first async use via
    /// [`AgentPool::ensure_restored`].
    restored: Arc<AtomicBool>,
}

impl AgentPool {
    pub fn new(provider: Arc<dyn ChatProvider>, config: Config) -> Self {
        Self {
            roles: HashMap::new(),
            provider,
            memory: SharedMemory::new(),
            bus: MessageBus::new(),
            wall_timeout_secs: config.wall_timeout_secs,
            restored: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Attach a storage backend: `SharedMemory` writes and `MessageBus`
    /// history become write-through persistent, and a fresh process can
    /// restore the last snapshot via [`AgentPool::restore`].
    pub fn with_backend(mut self, backend: Arc<dyn StorageBackend>) -> Self {
        self.memory = self.memory.with_backend(backend.clone());
        self.bus = self.bus.with_backend(backend);
        self
    }

    /// Restore the last persisted collaboration snapshot into the pool's
    /// shared memory and bus. Best-effort; no-op without a backend.
    pub async fn restore(&self) {
        self.memory.restore().await;
        self.bus.restore().await;
    }

    /// Rehydrate the persisted snapshot exactly once (best-effort). Safe to
    /// call on every agent dispatch: the first call restores, later calls are
    /// a single relaxed atomic check.
    pub async fn ensure_restored(&self) {
        if self.restored.swap(true, Ordering::AcqRel) {
            return;
        }
        self.restore().await;
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
            .wall_timeout_secs(self.wall_timeout_secs)
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
            // Goal 399: sub-agents inherit the parent session's wall-clock
            // budget (never unlimited while the parent is bounded).
            wall_timeout_secs: self.wall_timeout_secs,
            // Issue #115: sub-agent turns are not billed separately.
            failure_usage: None,
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
/// 2. Call the `agent` tool with a `manifest` describing each specialist
///    (`system_prompt` + `allowed_tools`, or a `definition` referencing a built-in
///    role from `.recursive/agents/*.md`)
/// 3. Dispatch via the tool's `mode`: `single`, `parallel`, or `sequential`
/// 4. Synthesise and return the combined results
///
/// This prompt is injected in addition to any project-level context. It only
/// teaches tools that are actually registered on the coordinator's tool
/// registry by [`register_subagent_if_enabled`].
pub fn coordinator_system_prompt() -> &'static str {
    concat!(
        "You are a coordinator agent. Your job is to decompose complex tasks and delegate them ",
        "to specialist workers you design on the fly.\n\n",
        "## Your tools\n\n",
        "- **`agent`** — your primary tool. Spawns one or more specialist workers. Each worker ",
        "  runs its own agent loop with a restricted tool set and an isolated transcript.\n",
        "- **`send_message`** — push a follow-up message to a worker. For a background worker this ",
        "drives a NEW turn on the same runtime, preserving its transcript/context (cross-turn ",
        "continuation). Use `task_id` (returned by a background `agent` call) to address it.\n",
        "- If present, **`task_list` / `task_output` / `task_stop`** let you inspect running tasks, ",
        "drain their buffered output, or cancel them. (These are optional and may not be registered ",
        "in every build; if a tool call is rejected as unknown, simply rely on `agent` and ",
        "`send_message` instead.)\n\n",
        "## Your workflow\n\n",
        "1. **Analyse** — understand the task and identify the distinct subtasks and the expertise each requires.\n",
        "2. **Design specialists** — for each distinct expertise, add an entry to the `agent` tool's ",
        "`manifest`. Each entry is a worker_id → role mapping. A role is defined by:\n",
        "   - A focused `system_prompt` that defines the specialist's role, constraints, and output format\n",
        "   - Optional `allowed_tools` (omit or leave empty for the read-only set: ",
        "Read, Grep, Glob, WebFetch, SearchFiles; list tool names to grant write tools)\n",
        "   - Or a `definition` referencing a built-in role from `.recursive/agents/*.md` ",
        "(e.g. `explore`, `plan`, `verification`, `general-purpose`)\n",
        "3. **Dispatch** — set the `agent` tool's `mode`:\n",
        "   - `single` — exactly one worker, runs to completion and returns its result.\n",
        "   - `parallel` — all workers run concurrently. Use for independent read-only tasks ",
        "(research, review).\n",
        "   - `sequential` — workers run one after another. Use when one task's output feeds the next, ",
        "or for write-heavy tasks that may touch the same files.\n",
        "   - For a long-running task you want to continue later, set `background: true` with ",
        "`mode: \"single\"`. The worker is spawned in the background and you get a `task_id` immediately ",
        "(instead of waiting). Use `send_message(task_id=...)` to run follow-up turns on the same worker, ",
        "and `task_output` / `task_stop` to inspect or cancel it.\n",
        "4. **Synthesise** — collect all worker results and write a final unified answer.\n\n",
        "## Rules\n\n",
        "- Design the *minimum* number of specialists the task requires — avoid over-decomposition.\n",
        "- Read-only tasks (analysis, review, research) can always run in parallel.\n",
        "- Write-heavy tasks (coding, patching) should run sequentially unless you are certain they touch different files.\n",
        "- After all workers finish, synthesise their outputs into a single coherent response rather than just concatenating them.\n\n",
        "## Writing worker prompts\n\n",
        "**Workers cannot see this conversation.** Every `prompt` you pass to the `agent` tool, ",
        "and every `message` you send via `send_message`, must be self-contained — it is the only ",
        "context the worker has.\n\n",
        "### Never delegate understanding\n\n",
        "When a worker reports research findings, **you** must understand them before directing\n",
        "follow-up work. Read the findings, identify the approach, then write a prompt that *proves*\n",
        "you understood it by naming the exact `file:line`, the specific change, and what \"done\" looks\n",
        "like. Never write \"based on your findings, fix the bug\" or \"based on the research, implement\n",
        "it\" — those phrases push the synthesis onto the worker instead of doing it yourself. You never\n",
        "hand off understanding to another worker.\n\n",
        "```\n",
        "// Bad — lazy delegation (workers can't see this conversation)\n",
        "agent({ mode: \"single\",\n",
        "  manifest: { fixer: { system_prompt: \"You fix bugs.\" } },\n",
        "  prompt: \"Based on your findings, fix the auth bug\" })\n",
        "\n",
        "// Good — synthesised spec: file:line, root cause, exact change, done-criterion\n",
        "agent({ mode: \"single\",\n",
        "  manifest: { coder: { system_prompt: \"You implement tasks using the available tools.\",\n",
        "                        allowed_tools: [\"Read\", \"Edit\", \"Bash\"] } },\n",
        "  prompt: \"Fix the null deref in src/auth/validate.rs:42. The `user` field on Session\n",
        "    (src/auth/types.rs:15) is None when a session expires but the token stays cached.\n",
        "    Add a None check before `user.id` access — if None, return 401 with 'Session expired'.\n",
        "    Run `cargo test -p auth` and report the result.\" })\n",
        "```\n\n",
        "### What every worker prompt must contain\n\n",
        "- **Target**: concrete `file:line` references and the exact change to make — not \"the auth\n",
        "  module\", but `src/auth/validate.rs:42`.\n",
        "- **Root cause, not symptom**: guide the worker toward a durable fix, not a patch that\n",
        "  silences the error.\n",
        "- **Done-criterion**: state what \"done\" looks like. For implementation: \"run the relevant\n",
        "  tests and typecheck, then report the result\". For research: \"report file paths, line\n",
        "  numbers, and signatures — do not modify files\".\n",
        "- **Purpose**: one line on why the task matters, so the worker can calibrate depth (\"this\n",
        "  informs a PR description — focus on user-facing changes\").\n\n",
        "### Continue vs spawn — decide by context overlap\n\n",
        "A background worker (`background: true`) stays alive across turns: `send_message(task_id=...)` ",
        "runs a new turn on the same runtime, so the worker keeps its loaded files and prior findings. ",
        "Choose between continuing an existing background worker and spawning a fresh one by how much of ",
        "its loaded context overlaps the next task:\n\n",
        "| Situation | Mechanism | Why |\n",
        "|-----------|-----------|-----|\n",
        "| Follow-up refines the background worker's current task | **Continue** (`send_message`) | Worker already has the files loaded AND now gets a clearer plan |\n",
        "| Research was broad but the next task is narrow | **Spawn fresh** (`agent`) | Focused context is cleaner than dragging along exploration noise |\n",
        "| Correcting a failure or extending recent work | **Continue** (`send_message`) | Worker has the error context and knows what it just tried |\n",
        "| Verifying code a different worker just wrote | **Spawn fresh** (`agent`) | A verifier should see the code with fresh eyes, not carry the implementer's assumptions |\n",
        "| First attempt used the wrong approach entirely | **Spawn fresh** | Wrong-approach context pollutes the retry; clean slate avoids anchoring on the failed path |\n",
        "| Unrelated task | **Spawn fresh** | No useful context to reuse |\n\n",
        "There is no universal default. High overlap → continue with `send_message`. Low overlap → spawn fresh.\n\n",
        "### Verification — prove it works, don't confirm it exists\n\n",
        "When a worker (or you) claims a change is done, verification means **proving the code works**,\n",
        "not confirming that it exists. A verifier that rubber-stamps weak work undermines everything.\n",
        "- Run tests **with the feature exercised**, not just \"the suite passes overall\".\n",
        "- Run typecheck/clippy and **investigate** errors — don't dismiss them as \"unrelated\".\n",
        "- Be sceptical: if something looks off, dig in. Try edge cases and error paths, not just the\n",
        "  happy path the implementer already ran.\n"
    )
}

/// Register the unified `Agent` (sub-agent / team coordination) tool — plus the
/// coordinator-side coordination tools (`send_message`, `list_workers`,
/// `task_create` / `task_get` / `task_list` / `task_output` / `task_stop` /
/// `task_update`) — on `tools` when `config.subagent_enabled` is true.
///
/// This is the single, channel-agnostic hook called by every agent-loop entry
/// point (CLI run / loop, HTTP API, TUI) after they build their base tool
/// registry and resolve their provider, so the `Agent` tool and the coordinator
/// prompt injected by [`crate::system_prompt::assemble_system_prompt`] stay in
/// sync across all surfaces. Returns `tools` unchanged when sub-agent is
/// disabled.
///
/// The coordination tools share a single `TaskRegistry` and `WorkerRegistry`
/// with the `Agent` tool, so a coordinator can dispatch a worker via `agent`
/// and then inspect / message / cancel it. Without this wiring the coordinator
/// prompt would advertise tools it cannot actually call.
///
/// Cancellation wiring (issue #40): pass a static token via `shutdown_token`
/// (CLI loop / HTTP-serve mint the token once), or a per-turn
/// [`SharedTokenSlot`] (TUI refreshes it each turn), or both (static wins).
///
/// Telemetry wiring (issue #119): pass a [`WorkerTelemetrySlot`] that the
/// owning runtime also holds, so worker runtimes emit through the parent's
/// event sink (attributed) and their usage is billed to the parent turn.
/// Pass `None` when the caller cannot route worker telemetry per session
/// (the HTTP server shares one `Agent` tool across sessions) — workers then
/// keep the previous [`crate::event::NullSink`] behaviour.
pub fn register_subagent_if_enabled(
    tools: ToolRegistry,
    config: &Config,
    provider: Arc<dyn ChatProvider>,
    shutdown_token: Option<SharedTokenSlot>,
    telemetry: Option<crate::tools::WorkerTelemetrySlot>,
) -> ToolRegistry {
    if !config.subagent_enabled {
        return tools;
    }
    let defs = AgentDefinitions::load(&config.workspace).unwrap_or_else(|e| {
        tracing::warn!("Failed to load agent definitions: {e}");
        AgentDefinitions::default()
    });
    // Goal #106: collaboration state (shared memory, bus history, artifacts)
    // persists through a local-storage backend rooted at the per-workspace
    // data dir, so a crash/restart rehydrates instead of losing everything.
    // Cloud deployments override via the S3/Redis backends; tests construct
    // their own pools and are unaffected.
    let collab_backend: Arc<dyn StorageBackend> =
        Arc::new(crate::storage::LocalStorageBackend::new(
            crate::paths::user_workspace_dir(&config.workspace)
                .unwrap_or_else(|_| config.workspace.join(".recursive").join("collab")),
        ));
    let artifact_store = Arc::new(crate::tools::artifacts::ArtifactStore::new(
        collab_backend.clone(),
    ));
    let pool = Arc::new(tokio::sync::RwLock::new(
        AgentPool::new(provider.clone(), config.clone()).with_backend(collab_backend),
    ));
    // A single shared registry pair so the `agent` tool and the coordinator-side
    // task/message tools observe the same workers.
    let task_registry = Arc::new(TaskRegistry::new());
    let worker_registry = WorkerRegistry::new();
    let worker_table: crate::tools::agent::WorkerTable =
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));

    let agent = AgentTool::new(
        &config.workspace,
        provider,
        // Goal 394: sub-agents get session-isolated tool state (read guard,
        // touched files, background jobs, sandbox roots) instead of sharing
        // the coordinator's.
        tools.fork_session(),
        config.subagent_max_depth,
        0,
        None,
    )
    .with_definitions(defs)
    .with_task_registry(task_registry.clone())
    .with_registry(worker_registry.clone())
    .with_workers(worker_table.clone())
    .with_pool(pool)
    .with_artifact_store(artifact_store.clone())
    // Issue #40: workers inherit the parent's wall budget and cancellation
    // token so a stalled worker LLM call can never park the parent turn.
    .with_wall_timeout_secs(config.wall_timeout_secs);
    let agent = match shutdown_token {
        Some(slot) => agent.with_shutdown_token_slot(slot),
        None => agent,
    };
    // Issue #119: wire the parent runtime's telemetry bridge so worker events
    // and usage reach the parent instead of being dropped.
    let agent = match telemetry {
        Some(slot) => agent.with_worker_telemetry(slot),
        None => agent,
    };

    // Register the `Agent` tool first, then the coordination tools that the
    // coordinator prompt teaches alongside it. Each is built from the same
    // shared registries. The task_* family is gated behind the
    // `coordinator-mode` feature; when it is disabled only `agent` +
    // `send_message` + `list_workers` are registered.
    let tools = tools
        .register(Arc::new(agent))
        .register(Arc::new(SendMessageTool::new(
            worker_registry.clone(),
            task_registry.clone(),
            worker_table.clone(),
        )))
        .register(Arc::new(ListWorkersTool::new(
            worker_registry.clone(),
            task_registry.clone(),
        )))
        // Goal #106: the coordinator is the primary consumer of artifact
        // references embedded in worker results, so it needs the loader tools
        // too — without them the advertised `artifact_read` is uncallable.
        .register(Arc::new(crate::tools::artifacts::ArtifactReadTool::new(
            artifact_store.clone(),
        )))
        .register(Arc::new(crate::tools::artifacts::ArtifactListTool::new(
            artifact_store,
        )));

    #[cfg(feature = "coordinator-mode")]
    {
        tools
            .register(Arc::new(TaskCreateTool::new(task_registry.clone())))
            .register(Arc::new(TaskGetTool::new(task_registry.clone())))
            .register(Arc::new(TaskListTool::new(task_registry.clone())))
            .register(Arc::new(TaskOutputTool::new(task_registry.clone())))
            .register(Arc::new(TaskStopTool::new(task_registry.clone())))
            .register(Arc::new(TaskUpdateTool::new(task_registry.clone())))
    }
    #[cfg(not(feature = "coordinator-mode"))]
    {
        // Suppress unused warnings for the shared registries when the task_*
        // tools are compiled out: send_message/list_workers above already
        // consume `worker_registry`, and `task_registry` is still passed to
        // the AgentTool via `with_task_registry` on the coordinator's behalf.
        let _ = &task_registry;
        tools
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
            allowed_tools: vec!["Read".into(), "Grep".into()],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Completion, MockProvider};
    use std::path::PathBuf;

    pub(super) fn test_config() -> Config {
        Config {
            workspace: PathBuf::from("."),
            api_base: String::new(),
            api_key: None,
            model: String::new(),
            provider_type: "openai".into(),
            preset: None,
            max_steps: 32,
            max_tokens: 65536,
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

    // --- Write-through persistence (goal #106) ---

    #[tokio::test]
    async fn shared_memory_persists_and_restores_across_instances() {
        let dir = tempfile::tempdir().unwrap();
        let backend: Arc<dyn StorageBackend> = Arc::new(crate::storage::LocalStorageBackend::new(
            dir.path().to_path_buf(),
        ));

        // Process 1: write two entries (write-through fires on each set).
        let mem = SharedMemory::new().with_backend(backend.clone());
        mem.set("goal".into(), "build feature X".into(), "planner".into())
            .await;
        mem.set("status".into(), "in-progress".into(), "coder".into())
            .await;

        // Process 2: a fresh instance restores the snapshot.
        let revived = SharedMemory::new().with_backend(backend);
        assert!(
            revived.get("goal").await.is_none(),
            "fresh instance starts empty until restore() runs"
        );
        revived.restore().await;
        let goal = revived.get("goal").await.expect("restored entry");
        assert_eq!(goal.value, "build feature X");
        assert_eq!(goal.author, "planner");
        assert_eq!(revived.get("status").await.unwrap().value, "in-progress");
    }

    #[tokio::test]
    async fn shared_memory_restore_never_clobbers_live_entries() {
        let dir = tempfile::tempdir().unwrap();
        let backend: Arc<dyn StorageBackend> = Arc::new(crate::storage::LocalStorageBackend::new(
            dir.path().to_path_buf(),
        ));

        let mem = SharedMemory::new().with_backend(backend.clone());
        mem.set("k".into(), "old".into(), "p1".into()).await;

        // A live instance writes a NEWER value for the same key before
        // restoring — the in-memory copy must win over the stale snapshot.
        let live = SharedMemory::new().with_backend(backend);
        live.set("k".into(), "newer".into(), "p2".into()).await;
        live.restore().await;
        assert_eq!(live.get("k").await.unwrap().value, "newer");
    }

    #[tokio::test]
    async fn shared_memory_seq_stays_monotonic_after_restore() {
        let dir = tempfile::tempdir().unwrap();
        let backend: Arc<dyn StorageBackend> = Arc::new(crate::storage::LocalStorageBackend::new(
            dir.path().to_path_buf(),
        ));

        let mem = SharedMemory::new().with_backend(backend.clone());
        mem.set("a".into(), "1".into(), "x".into()).await;
        mem.set("b".into(), "2".into(), "x".into()).await;
        let max_seq = mem.all().await.iter().map(|e| e.seq).max().unwrap_or(0);

        let revived = SharedMemory::new().with_backend(backend);
        revived.restore().await;
        let a_seq = revived.get("a").await.unwrap().seq;
        assert_eq!(a_seq, 1, "restored entries keep their original seq");
        revived.set("c".into(), "3".into(), "y".into()).await;
        let fresh = revived.get("c").await.unwrap().seq;
        assert!(
            fresh > max_seq,
            "post-restore writes must keep seq monotonic across the restart boundary ({fresh} must exceed {max_seq})"
        );
    }

    #[tokio::test]
    async fn shared_memory_persist_failure_degrades_not_errors() {
        // A backend that always fails must not break set()/remove(): the
        // in-memory store stays the source of truth.
        struct FailingBackend;
        #[async_trait::async_trait]
        impl StorageBackend for FailingBackend {
            async fn load_transcript(
                &self,
                _session_id: &str,
            ) -> crate::error::Result<Vec<Message>> {
                Ok(vec![])
            }
            async fn save_transcript(
                &self,
                _session_id: &str,
                _messages: &[Message],
            ) -> crate::error::Result<()> {
                Err(crate::error::Error::Storage {
                    message: "boom".into(),
                })
            }
            async fn delete_transcript(&self, _session_id: &str) -> crate::error::Result<()> {
                Err(crate::error::Error::Storage {
                    message: "boom".into(),
                })
            }
            async fn load_memory(&self, _key: &str) -> crate::error::Result<Option<String>> {
                Err(crate::error::Error::Storage {
                    message: "boom".into(),
                })
            }
            async fn save_memory(&self, _key: &str, _value: &str) -> crate::error::Result<()> {
                Err(crate::error::Error::Storage {
                    message: "boom".into(),
                })
            }
            async fn delete_memory(&self, _key: &str) -> crate::error::Result<()> {
                Err(crate::error::Error::Storage {
                    message: "boom".into(),
                })
            }
        }

        let mem = SharedMemory::new().with_backend(Arc::new(FailingBackend));
        mem.set("k".into(), "v".into(), "a".into()).await;
        assert_eq!(mem.get("k").await.unwrap().value, "v");
        assert!(mem.remove("k").await);
        assert!(mem.get("k").await.is_none());
    }

    #[tokio::test]
    async fn shared_memory_remove_persists_tombstone() {
        let dir = tempfile::tempdir().unwrap();
        let backend: Arc<dyn StorageBackend> = Arc::new(crate::storage::LocalStorageBackend::new(
            dir.path().to_path_buf(),
        ));

        let mem = SharedMemory::new().with_backend(backend.clone());
        mem.set("gone".into(), "soon".into(), "a".into()).await;
        assert!(mem.remove("gone").await);

        let revived = SharedMemory::new().with_backend(backend);
        revived.restore().await;
        assert!(
            revived.get("gone").await.is_none(),
            "removal must survive a restart (snapshot rewritten without the key)"
        );
    }

    #[tokio::test]
    async fn message_bus_history_persists_and_restores_across_instances() {
        let dir = tempfile::tempdir().unwrap();
        let backend: Arc<dyn StorageBackend> = Arc::new(crate::storage::LocalStorageBackend::new(
            dir.path().to_path_buf(),
        ));

        let bus = MessageBus::new().with_backend(backend.clone());
        bus.send(make_msg("planner", "coder", "task one", MessageType::Task))
            .await;
        bus.send(make_msg("coder", "planner", "done", MessageType::Result))
            .await;

        let revived = MessageBus::new().with_backend(backend);
        assert!(
            revived.history().await.is_empty(),
            "fresh instance starts empty until restore() runs"
        );
        revived.restore().await;
        let history = revived.history().await;
        let contents: Vec<_> = history.iter().map(|m| m.content.clone()).collect();
        assert_eq!(contents, vec!["task one", "done"]);
        // The restored bus is a fully functional one: routing still works.
        let inbox = revived.inbox("coder").await;
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].content, "task one");
    }

    #[tokio::test]
    async fn message_bus_restore_respects_capacity_and_live_writes() {
        let dir = tempfile::tempdir().unwrap();
        let backend: Arc<dyn StorageBackend> = Arc::new(crate::storage::LocalStorageBackend::new(
            dir.path().to_path_buf(),
        ));

        let bus = MessageBus::with_capacity(3).with_backend(backend.clone());
        for i in 0..5 {
            bus.send(make_msg(
                "a",
                "broadcast",
                &format!("m{i}"),
                MessageType::Feedback,
            ))
            .await;
        }

        // Live instance: a newer message beats restore().
        let live = MessageBus::with_capacity(3).with_backend(backend);
        live.send(make_msg("z", "broadcast", "fresh", MessageType::Broadcast))
            .await;
        live.restore().await;
        let contents: Vec<_> = live
            .history()
            .await
            .iter()
            .map(|m| m.content.clone())
            .collect();
        assert_eq!(
            contents,
            vec!["m3", "m4", "fresh"],
            "restore must not clobber a live buffer or exceed capacity"
        );
    }

    #[tokio::test]
    async fn message_bus_clear_persists_empty_history() {
        let dir = tempfile::tempdir().unwrap();
        let backend: Arc<dyn StorageBackend> = Arc::new(crate::storage::LocalStorageBackend::new(
            dir.path().to_path_buf(),
        ));

        let bus = MessageBus::new().with_backend(backend.clone());
        bus.send(make_msg("a", "b", "hello", MessageType::Task))
            .await;
        bus.clear().await;
        let revived = MessageBus::new().with_backend(backend);
        revived.restore().await;
        assert!(
            revived.history().await.is_empty(),
            "clear() must persist so a restart does not resurrect old messages"
        );
    }

    #[tokio::test]
    async fn agent_pool_restore_rehydrates_memory_and_bus() {
        let dir = tempfile::tempdir().unwrap();
        let backend: Arc<dyn StorageBackend> = Arc::new(crate::storage::LocalStorageBackend::new(
            dir.path().to_path_buf(),
        ));

        let provider: Arc<dyn ChatProvider> = Arc::new(MockProvider::new(vec![]));
        let pool = AgentPool::new(provider.clone(), test_config()).with_backend(backend.clone());
        pool.memory()
            .set("plan".into(), "step 1".into(), "planner".into())
            .await;
        pool.bus()
            .send(make_msg(
                "coordinator",
                "worker",
                "kick off",
                MessageType::Task,
            ))
            .await;

        // A second pool over the same backend restores the collaboration state.
        let pool2 = AgentPool::new(provider, test_config()).with_backend(backend);
        pool2.restore().await;
        assert_eq!(
            pool2.memory().get("plan").await.unwrap().value,
            "step 1",
            "pool restore must rehydrate shared memory"
        );
        assert_eq!(
            pool2.bus().inbox("worker").await[0].content,
            "kick off",
            "pool restore must rehydrate bus history"
        );
    }

    #[tokio::test]
    async fn agent_pool_ensure_restored_is_single_shot_and_keeps_state() {
        let dir = tempfile::tempdir().unwrap();
        let backend: Arc<dyn StorageBackend> = Arc::new(crate::storage::LocalStorageBackend::new(
            dir.path().to_path_buf(),
        ));

        let provider: Arc<dyn ChatProvider> = Arc::new(MockProvider::new(vec![]));
        let pool = AgentPool::new(provider.clone(), test_config()).with_backend(backend.clone());
        pool.memory().set("k".into(), "v".into(), "a".into()).await;

        let revived = AgentPool::new(provider, test_config()).with_backend(backend);
        revived.ensure_restored().await;
        assert_eq!(revived.memory().get("k").await.unwrap().value, "v");
        // A repeated ensure_restored must not wipe or double-apply the state.
        revived.ensure_restored().await;
        assert_eq!(revived.memory().get("k").await.unwrap().value, "v");
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

    #[test]
    fn coordinator_system_prompt_teaches_worker_briefing() {
        // P1 (design doc 5.4/5.5): the coordinator must not just say *how* to
        // dispatch — it must teach *how to write the brief*. These phrases are
        // the load-bearing ideas; pin them so a future trim doesn't silently
        // drop the methodology.
        let prompt = coordinator_system_prompt();
        assert!(
            prompt.contains("Writing worker prompts"),
            "coordinator prompt must have a 'Writing worker prompts' section"
        );
        assert!(
            prompt.contains("Never delegate understanding"),
            "coordinator prompt must forbid delegating understanding to workers"
        );
        // Self-containment: workers can't see the conversation.
        assert!(
            prompt.contains("cannot see this conversation"),
            "coordinator prompt must state workers cannot see the conversation"
        );
        // Continue vs spawn decision table — both mechanisms must be named.
        assert!(
            prompt.contains("send_message") && prompt.contains("agent"),
            "coordinator prompt must cover both continue (send_message) and spawn (agent)"
        );
        // Verification bar: prove it works, don't confirm it exists.
        assert!(
            prompt.contains("prove"),
            "coordinator prompt must raise the verification bar beyond 'it exists'"
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
        let result = register_subagent_if_enabled(tools, &config, provider, None, None);
        assert_eq!(
            result.names(),
            initial_names,
            "disabled subagent must not register any additional tools"
        );
    }

    #[test]
    fn register_subagent_if_enabled_registers_artifact_tools() {
        // Hermetic: pin RECURSIVE_HOME so the collab backend resolves inside
        // an isolated tempdir, not the developer's real data dir.
        let ws = crate::test_util::IsolatedWorkspace::new();
        let provider = Arc::new(MockProvider::new(vec![]));
        let mut config = test_config();
        config.subagent_enabled = true;
        config.workspace = ws.path().to_path_buf();
        let tools = crate::tools::ToolRegistry::local();
        let result = register_subagent_if_enabled(tools, &config, provider, None, None);
        let names = result.names();
        for expected in ["agent", "artifact_read", "artifact_list"] {
            assert!(
                names.iter().any(|n| n == expected),
                "coordinator registry must expose '{expected}', got: {names:?}"
            );
        }
    }
}

// ── Goal 399: sub-agents inherit the parent session's wall-clock budget ────

#[cfg(test)]
mod wall_budget_tests {
    use super::*;
    use crate::llm::{Completion, MockProvider, ToolCall};

    /// Provider that stalls before its first N responses, then delegates to
    /// a scripted `MockProvider`. Deterministically drives the wall-clock
    /// deadline past a 1s budget without long real sleeps.
    struct SlowFirstCallProvider {
        inner: MockProvider,
        delay: std::time::Duration,
        remaining_slow_calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ChatProvider for SlowFirstCallProvider {
        async fn complete(
            &self,
            messages: &[Message],
            tools: &[crate::llm::ToolSpec],
        ) -> crate::error::Result<Completion> {
            use std::sync::atomic::Ordering;
            if self.remaining_slow_calls.fetch_sub(1, Ordering::SeqCst) > 0 {
                tokio::time::sleep(self.delay).await;
            }
            self.inner.complete(messages, tools).await
        }
    }

    fn slow_provider(script: Vec<Completion>, slow_calls: usize) -> Arc<dyn ChatProvider> {
        Arc::new(SlowFirstCallProvider {
            inner: MockProvider::new(script),
            delay: std::time::Duration::from_secs(2),
            remaining_slow_calls: std::sync::atomic::AtomicUsize::new(slow_calls),
        })
    }

    fn tool_call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            // Deliberately unregistered: the dispatch error must NOT end the
            // turn — the loop continues to the next step boundary, where the
            // wall check fires.
            name: "definitely_not_registered".into(),
            arguments: serde_json::json!({}),
        }
    }

    fn pool_with_budget(budget: u64, script: Vec<Completion>, slow_calls: usize) -> AgentPool {
        // Reuse the shared fixture from the sibling `tests` module.
        let mut config = super::tests::test_config();
        config.wall_timeout_secs = budget;
        let pool = AgentPool::new(slow_provider(script, slow_calls), config);
        assert_eq!(
            pool.wall_timeout_secs, budget,
            "pool must inherit the parent budget"
        );
        pool
    }

    fn add_harness_role(pool: &mut AgentPool) {
        pool.add_role(AgentRole {
            name: "harness".into(),
            system_prompt: "test".into(),
            max_steps: 10,
            allowed_tools: vec![],
        });
    }

    /// Goal 399: a sub-agent under a 1s parent budget that stalls 2s
    /// mid-turn finishes with `WallClockExceeded { secs: 1 }` — sub-agents
    /// are never unbounded while the parent is bounded.
    #[tokio::test]
    async fn run_with_role_inherits_parent_wall_timeout() {
        let script = vec![
            Completion {
                content: "stalling".into(),
                tool_calls: vec![tool_call("c1")],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "again".into(),
                tool_calls: vec![tool_call("c2")],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
        ];
        let mut pool = pool_with_budget(1, script, 1);
        add_harness_role(&mut pool);

        let outcome = pool
            .run_with_role("harness", "go")
            .await
            .expect("wall finish is Ok data, not an error");
        assert!(
            matches!(
                outcome.finish_reason,
                crate::agent::FinishReason::WallClockExceeded { secs: 1 }
            ),
            "sub-agent must inherit the 1s parent budget; got {:?}",
            outcome.finish_reason
        );
    }

    /// Goal 399: budget 0 (parent unlimited) keeps the legacy behaviour —
    /// the same stalled script runs to normal completion.
    #[tokio::test]
    async fn run_with_role_zero_budget_stays_unlimited() {
        let script = vec![
            Completion {
                content: "stalling".into(),
                tool_calls: vec![tool_call("c1")],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "done".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ];
        let mut pool = pool_with_budget(0, script, 1);
        add_harness_role(&mut pool);

        let outcome = pool
            .run_with_role("harness", "go")
            .await
            .expect("unlimited budget must complete normally");
        assert_eq!(
            outcome.finish_reason,
            crate::agent::FinishReason::NoMoreToolCalls
        );
    }
}
