//! Unified multi-agent delegation tool (`agent`) plus shared-memory tools.
//!
//! # Design
//!
//! A single `agent` tool replaces the previous fragmented delegation surface
//! (`SubAgent` / `spawn_worker` / `spawn_workers_parallel` / `team_add_role` /
//! `team_remove_role` / `team_list_roles`).  The caller provides a `manifest`
//! that maps worker IDs to `{ system_prompt, allowed_tools }` entries and an
//! execution `mode`:
//!
//! - `"single"`   — one worker, exactly as if `SubAgent` + explicit role had
//!   been combined.
//! - `"parallel"` — all workers run concurrently (join_all).  Read-only
//!   workers benefit most.
//! - `"sequential"` — workers run one after another, in manifest key order.
//!
//! Shared-memory read/write are kept as independent tools so workers can
//! coordinate through a shared key-value store.
//!
//! # Recursive safety
//!
//! A depth limit (`RECURSIVE_SUBAGENT_MAX_DEPTH` env, default 2) prevents
//! unbounded nesting.  Each child `agent` increments the depth counter; when
//! the limit is reached the tool returns an error string instead of spawning.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, RwLock};

use crate::agent::FinishReason;
use crate::error::{Error, Result};
use crate::llm::{ChatProvider, ToolSpec};
use crate::multi::{AgentManifest, AgentMode, AgentPool, WorkerManifestEntry};
use crate::runtime::{AgentRuntime, AgentRuntimeBuilder};
use crate::tasks::{TaskId, TaskRegistry, TaskState};
use crate::tools::agent_defs::AgentDefinitions;
use crate::tools::edit::EditTool;
use crate::tools::fs::{ReadFile, ReadFileState, WriteFile};
use crate::tools::send_message::{ListWorkersTool, SendMessageTool, WorkerRegistry};
use crate::tools::{PermissionHook, Tool, ToolRegistry, ToolSideEffect};

// ---------------------------------------------------------------------------
// SharedMemoryRead
// ---------------------------------------------------------------------------

/// The `shared_memory_read` tool — read a value from the shared memory store.
pub struct SharedMemoryRead {
    pool: Arc<RwLock<AgentPool>>,
}

impl SharedMemoryRead {
    pub fn new(pool: Arc<RwLock<AgentPool>>) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl Tool for SharedMemoryRead {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "shared_memory_read".into(),
            description: "Read a value from the shared memory store. Use this to retrieve context published by other workers.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "key": {
                        "type": "string",
                        "description": "The key to read from shared memory."
                    }
                },
                "required": ["key"]
            }),
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        ToolSideEffect::ReadOnly
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let key = arguments["key"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "shared_memory_read".into(),
                message: "missing required parameter: key".to_string(),
            })?;

        let pool = self.pool.read().await;
        match pool.memory().get(key).await {
            Some(entry) => Ok(entry.value),
            None => Ok(format!("Key '{key}' not found in shared memory.")),
        }
    }
}

// ---------------------------------------------------------------------------
// SharedMemoryWrite
// ---------------------------------------------------------------------------

/// The `shared_memory_write` tool — write a value into the shared memory store.
pub struct SharedMemoryWrite {
    pool: Arc<RwLock<AgentPool>>,
    author: String,
}

impl SharedMemoryWrite {
    pub fn new(pool: Arc<RwLock<AgentPool>>, author: String) -> Self {
        Self { pool, author }
    }
}

#[async_trait]
impl Tool for SharedMemoryWrite {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "shared_memory_write".into(),
            description: "Write a value to the shared memory store. Other workers can read this via shared_memory_read. Use this to publish findings, decisions, or intermediate results.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "key": {
                        "type": "string",
                        "description": "The key under which to store the value."
                    },
                    "value": {
                        "type": "string",
                        "description": "The value to store."
                    }
                },
                "required": ["key", "value"]
            }),
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        ToolSideEffect::External
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let key = arguments["key"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "shared_memory_write".into(),
                message: "missing required parameter: key".to_string(),
            })?
            .to_string();
        let value = arguments["value"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "shared_memory_write".into(),
                message: "missing required parameter: value".to_string(),
            })?
            .to_string();

        self.pool
            .read()
            .await
            .memory()
            .set(key.clone(), value, self.author.clone())
            .await;
        Ok(format!("Stored '{key}' in shared memory."))
    }
}

// ---------------------------------------------------------------------------
// AgentTool — unified delegation
// ---------------------------------------------------------------------------

/// A long-lived handle to a background worker, enabling cross-turn
/// continuation via `send_message`.
///
/// When a worker is spawned in the background, its `AgentRuntime` lives in a
/// dedicated tokio task that drains an mpsc channel of incoming prompts. Each
/// `send_message` to this worker pushes a prompt onto `tx`; the worker task
/// runs it as a new turn on the same runtime (preserving transcript/context).
/// `task_id` links this handle to the `TaskRegistry` entry so `task_get` /
/// `task_output` / `task_stop` work uniformly.
pub struct WorkerHandle {
    /// Push a new turn prompt to the background worker. Returns Err if the
    /// worker task has exited (channel closed).
    pub tx: mpsc::UnboundedSender<String>,
    /// The TaskRegistry id under which this worker is registered.
    pub task_id: TaskId,
}

/// A process-wide table of live background workers keyed by worker_id.
/// Shared between the `agent` tool (which inserts) and the `send_message`
/// tool ( which looks up to continue a worker).
pub type WorkerTable = Arc<RwLock<HashMap<String, Arc<WorkerHandle>>>>;

/// The unified `agent` delegation tool.
///
/// Spawns one or more specialist sub-agents (workers) according to a
/// caller-supplied `manifest` and execution `mode`.
pub struct AgentTool {
    workspace: std::path::PathBuf,
    provider: Arc<dyn ChatProvider>,
    all_tools: ToolRegistry,
    max_depth: usize,
    current_depth: usize,
    permission_hook: Option<Arc<dyn PermissionHook>>,
    registry: Option<WorkerRegistry>,
    pool: Option<Arc<RwLock<AgentPool>>>,
    task_registry: Arc<TaskRegistry>,
    definitions: Option<AgentDefinitions>,
    /// Background worker continuation table (worker_id → handle). Populated
    /// when a worker is spawned in the background; `send_message` reads here
    /// to continue a worker across turns.
    workers: WorkerTable,
}

impl AgentTool {
    pub fn new(
        workspace: impl Into<std::path::PathBuf>,
        provider: Arc<dyn ChatProvider>,
        all_tools: ToolRegistry,
        max_depth: usize,
        current_depth: usize,
        permission_hook: Option<Arc<dyn PermissionHook>>,
    ) -> Self {
        Self {
            workspace: workspace.into(),
            provider,
            all_tools,
            max_depth,
            current_depth,
            permission_hook,
            registry: None,
            pool: None,
            task_registry: Arc::new(TaskRegistry::new()),
            definitions: None,
            workers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Attach a `WorkerRegistry` so workers can send messages to each other.
    pub fn with_registry(mut self, registry: WorkerRegistry) -> Self {
        self.registry = Some(registry);
        self
    }

    /// Attach an `AgentPool` for shared-memory coordination between workers.
    pub fn with_pool(mut self, pool: Arc<RwLock<AgentPool>>) -> Self {
        self.pool = Some(pool);
        self
    }

    /// Attach a `TaskRegistry` so this agent and its descendants can
    /// share background tasks (Phase D). If never called, a private
    /// in-memory registry is used.
    pub fn with_task_registry(mut self, reg: Arc<TaskRegistry>) -> Self {
        self.task_registry = reg;
        self
    }

    /// Attach an `AgentDefinitions` registry so manifest entries can
    /// reference definitions by name via the `definition` field.
    pub fn with_definitions(mut self, defs: AgentDefinitions) -> Self {
        self.definitions = Some(defs);
        self
    }

    /// Attach a shared background-worker continuation table. When set,
    /// background workers register their continuation handle here so that
    /// `send_message` can drive follow-up turns on the same runtime.
    pub fn with_workers(mut self, workers: WorkerTable) -> Self {
        self.workers = workers;
        self
    }

    // ------------------------------------------------------------------
    // Tool-registry construction
    // ------------------------------------------------------------------

    /// Build a restricted tool registry containing only the named tools.
    ///
    /// Uses `with_same_transport()` to start from an empty registry with the
    /// same transport/permissions/policy as the parent, so only explicitly
    /// listed tools are available — no accidental tool leakage.
    ///
    /// Sub-agents receive a **fresh** `ReadFileState` so their read history
    /// is independent from the parent's.
    fn build_sub_registry(&self, tool_names: &[String]) -> ToolRegistry {
        let sub_read_state = Arc::new(Mutex::new(ReadFileState::new()));
        // Start from parent's transport/permissions/policy but override
        // read_file_state with a fresh instance for isolation.
        let mut reg = self
            .all_tools
            .with_same_transport()
            .with_read_file_state(sub_read_state.clone());
        for name in tool_names {
            // ReadFile and EditTool carry internal read_state references;
            // create new instances bound to the sub-agent's fresh state rather
            // than inheriting the parent's Arc.
            let tool: Arc<dyn Tool> = match name.as_str() {
                "Read" => {
                    Arc::new(ReadFile::new(&self.workspace).with_read_state(sub_read_state.clone()))
                }
                "Edit" => {
                    Arc::new(EditTool::new(&self.workspace).with_read_state(sub_read_state.clone()))
                }
                "Write" => Arc::new(
                    WriteFile::new(&self.workspace).with_read_state(sub_read_state.clone()),
                ),
                _ => {
                    if let Some(t) = self.all_tools.get(name) {
                        t
                    } else {
                        continue;
                    }
                }
            };
            reg = reg.register(tool);
        }
        reg
    }

    /// Default tool set when no `allowed_tools` is specified: read-only + basic.
    fn default_tool_names() -> Vec<String> {
        vec![
            "Read".to_string(),
            "Grep".to_string(),
            "Glob".to_string(),
            "WebFetch".to_string(),
            "SearchFiles".to_string(),
        ]
    }

    // ------------------------------------------------------------------
    // Worker execution
    // ------------------------------------------------------------------

    /// Build a fresh `AgentRuntime` for a worker with the manifest entry's
    /// tool set and system prompt. The runtime retains transcript across
    /// turns, which is what enables `send_message` to continue a background
    /// worker: each follow-up prompt is a new turn on the same runtime.
    ///
    /// `worker_id` is used only for shared-memory tool namespacing.
    async fn build_worker_runtime(
        &self,
        worker_id: &str,
        entry: &WorkerManifestEntry,
        max_steps: usize,
        child_depth: usize,
    ) -> Result<AgentRuntime> {
        // Resolve allowed tools
        let tool_names: Vec<String> = if entry.allowed_tools.is_empty() {
            Self::default_tool_names()
        } else {
            entry.allowed_tools.clone()
        };

        // Build the worker's tool registry
        let mut sub_registry = self.build_sub_registry(&tool_names);

        // Register a child AgentTool for recursive delegation
        let mut child_agent = AgentTool::new(
            &self.workspace,
            self.provider.clone(),
            self.all_tools.fork(),
            self.max_depth,
            child_depth,
            self.permission_hook.clone(),
        );
        if let Some(reg) = &self.registry {
            child_agent = child_agent.with_registry(reg.clone());
        }
        if let Some(pool) = &self.pool {
            child_agent = child_agent.with_pool(pool.clone());
        }
        // Always propagate the task registry and worker table so descendants
        // share coordination state with the coordinator.
        child_agent = child_agent
            .with_task_registry(self.task_registry.clone())
            .with_workers(self.workers.clone());
        sub_registry = sub_registry.register(Arc::new(child_agent));

        // Inject shared-memory tools if pool is available
        if let Some(pool) = &self.pool {
            sub_registry = sub_registry.register(Arc::new(SharedMemoryRead::new(pool.clone())));
            sub_registry = sub_registry.register(Arc::new(SharedMemoryWrite::new(
                pool.clone(),
                worker_id.to_string(),
            )));
        }

        // Inject inter-worker messaging tools if registry is available
        if let Some(reg) = &self.registry {
            sub_registry = sub_registry.register(Arc::new(SendMessageTool::new(
                reg.clone(),
                self.task_registry.clone(),
                self.workers.clone(),
            )));
            sub_registry = sub_registry.register(Arc::new(ListWorkersTool::new(
                reg.clone(),
                self.task_registry.clone(),
            )));
        }

        // Build the system prompt with shared-memory context
        let mut system_prompt = entry.system_prompt.clone();
        if let Some(pool) = &self.pool {
            let memory_ctx = pool.read().await.memory().to_context_string().await;
            if !memory_ctx.is_empty() {
                system_prompt = format!("{}\n\n{}", system_prompt, memory_ctx);
            }
        }

        AgentRuntimeBuilder::new()
            .llm(self.provider.clone())
            .tools(sub_registry)
            .max_steps(max_steps)
            .system_prompt(system_prompt)
            .build()
            .map_err(|e| Error::Tool {
                name: "agent".into(),
                call_id: None,
                message: format!("failed to build worker '{}' runtime: {e}", worker_id),
            })
    }

    /// Run a single worker synchronously and return its final text.
    ///
    /// The worker runs exactly one turn (the initial prompt) on a fresh
    /// runtime; it is NOT registered for continuation. Use
    /// [`spawn_background_worker`] for a long-lived, continuable worker.
    async fn run_worker(
        &self,
        worker_id: &str,
        entry: &WorkerManifestEntry,
        prompt: &str,
        max_steps: usize,
        child_depth: usize,
    ) -> Result<String> {
        let mut runtime = self
            .build_worker_runtime(worker_id, entry, max_steps, child_depth)
            .await?;
        let outcome = runtime.run(prompt).await.map_err(|e| Error::Tool {
            name: "agent".into(),
            call_id: None,
            message: format!("worker '{}' failed: {e}", worker_id),
        })?;

        let finish_label = match outcome.finish_reason {
            FinishReason::NoMoreToolCalls => "NoMoreToolCalls".to_string(),
            FinishReason::BudgetExceeded => "BudgetExceeded".to_string(),
            FinishReason::ProviderStop(r) => r,
            FinishReason::Stuck { .. } => "Stuck".to_string(),
            FinishReason::TranscriptLimit { .. } => "TranscriptLimit".to_string(),
            FinishReason::Cancelled => "Cancelled".to_string(),
            FinishReason::PermissionDenialLimit => "PermissionDenialLimit".to_string(),
            FinishReason::WallClockExceeded { .. } => "WallClockExceeded".to_string(),
        };

        let final_text = outcome
            .final_text
            .unwrap_or_else(|| "(no final message)".to_string());

        Ok(format!(
            "[worker '{worker_id}' finished: {finish_label}]\n{final_text}"
        ))
    }

    /// Spawn a worker into a background tokio task that drains an mpsc channel
    /// of turn prompts against a single long-lived `AgentRuntime`. Returns the
    /// task id (registered in the shared `TaskRegistry`) immediately; the
    /// coordinator can continue the worker via `send_message(task_id=...)` and
    /// inspect/cancel it via `task_get` / `task_output` / `task_stop`.
    ///
    /// The runtime is retained across turns, so follow-up prompts continue
    /// the same conversation (transcript + todo + goals preserved).
    async fn spawn_background_worker(
        &self,
        worker_id: &str,
        entry: &WorkerManifestEntry,
        prompt: &str,
        max_steps: usize,
        child_depth: usize,
    ) -> Result<TaskId> {
        let runtime = self
            .build_worker_runtime(worker_id, entry, max_steps, child_depth)
            .await?;
        let runtime = Arc::new(tokio::sync::Mutex::new(runtime));

        // Register a TaskState so task_* tools can observe/cancel this worker.
        let (state, task_id) =
            TaskState::new(format!("worker '{worker_id}'"), String::new(), worker_id);
        let state = self.task_registry.register(state).await;

        // Continuation channel: the first prompt is enqueued by the spawner;
        // each `send_message` push adds another turn.
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let _ = tx.send(prompt.to_string());

        // Record the continuation handle so `send_message` can reach this
        // worker by worker_id.
        let handle = Arc::new(WorkerHandle {
            tx,
            task_id: task_id.clone(),
        });
        self.workers
            .write()
            .await
            .insert(worker_id.to_string(), handle.clone());

        // Spawn the long-lived worker task.
        let state_for_task = state.clone();
        let join_handle = tokio::spawn(async move {
            // Drain the channel: each message is a new turn on the same runtime.
            while let Some(msg) = rx.recv().await {
                let mut rt = runtime.lock().await;
                match rt.run(&msg).await {
                    Ok(outcome) => {
                        let text = outcome
                            .final_text
                            .unwrap_or_else(|| "(no final message)".to_string());
                        let _ = state_for_task
                            .append_output(format!("--- turn ---\n{text}"))
                            .await;
                    }
                    Err(e) => {
                        state_for_task
                            .mark_failed(format!("worker turn failed: {e}"))
                            .await;
                        return;
                    }
                }
            }
            // Channel closed (no more send_message will arrive): mark complete.
            state_for_task.mark_completed("worker finished".to_string()).await;
        });

        // Attach the JoinHandle so `task_stop` can truly abort this task.
        state.set_handle(join_handle).await;

        Ok(task_id)
    }


    // ------------------------------------------------------------------
    // Mode dispatchers
    // ------------------------------------------------------------------

    /// Single mode: one worker.
    async fn execute_single(
        &self,
        manifest: &AgentManifest,
        prompt: &str,
        max_steps: usize,
        child_depth: usize,
    ) -> Result<String> {
        if manifest.len() != 1 {
            return Err(Error::BadToolArgs {
                name: "agent".into(),
                message: format!(
                    "mode 'single' requires exactly one manifest entry, got {}",
                    manifest.len()
                ),
            });
        }
        // Safe: the `manifest.len() != 1` check above guarantees exactly one
        // entry, so the iterator yields exactly one element.  Use
        // `ok_or_else` (not `unwrap()`) to satisfy AGENTS.md invariant #5
        // (no `unwrap()` in non-test code) while preserving the same error
        // type.
        let (worker_id, entry) = manifest.iter().next().ok_or_else(|| Error::BadToolArgs {
            name: "agent".into(),
            message: "mode 'single' requires exactly one manifest entry".to_string(),
        })?;
        self.run_worker(worker_id, entry, prompt, max_steps, child_depth)
            .await
    }

    /// Parallel mode: all workers run concurrently via `futures_util::future::join_all`.
    async fn execute_parallel(
        &self,
        manifest: &AgentManifest,
        prompt: &str,
        max_steps: usize,
        child_depth: usize,
    ) -> Result<String> {
        if manifest.is_empty() {
            return Err(Error::BadToolArgs {
                name: "agent".into(),
                message: "mode 'parallel' requires at least one manifest entry".to_string(),
            });
        }

        // Pre-register all workers in the registry so they can message each other.
        if let Some(reg) = &self.registry {
            for worker_id in manifest.keys() {
                reg.register(worker_id).await;
            }
        }

        // Build a self-like AgentTool instance that can be moved into each task.
        // The AgentTool struct is intentionally designed so that each parallel
        // worker gets its own clone of the relevant fields.
        let workspace = self.workspace.clone();
        let provider = self.provider.clone();
        let all_tools = self.all_tools.fork();
        let max_depth = self.max_depth;
        let permission_hook = self.permission_hook.clone();
        let registry = self.registry.clone();
        let pool = self.pool.clone();
        let definitions = self.definitions.clone();
        let workers = self.workers.clone();

        // Spawn each worker into a tokio task, collecting JoinHandles.
        let mut handles: Vec<tokio::task::JoinHandle<(String, Result<String>)>> = Vec::new();
        for (worker_id, entry) in manifest.iter() {
            let worker_id = worker_id.clone();
            let entry = entry.clone();
            let prompt = prompt.to_string();
            let workspace = workspace.clone();
            let provider = provider.clone();
            let all_tools = all_tools.clone();
            let permission_hook = permission_hook.clone();
            let registry = registry.clone();
            let pool = pool.clone();
            let definitions = definitions.clone();
            let workers = workers.clone();

            handles.push(tokio::spawn(async move {
                let agent = AgentTool {
                    workspace,
                    provider,
                    all_tools,
                    max_depth,
                    current_depth: child_depth,
                    permission_hook,
                    registry: registry.clone(),
                    pool: pool.clone(),
                    task_registry: Arc::new(crate::tasks::TaskRegistry::new()),
                    definitions,
                    workers,
                };
                let result = agent
                    .run_worker(&worker_id, &entry, &prompt, max_steps, child_depth)
                    .await;

                // Deregister this worker
                if let Some(reg) = &registry {
                    reg.deregister(&worker_id).await;
                }

                (worker_id, result)
            }));
        }

        // Await all handles
        let outcomes = futures_util::future::join_all(handles).await;

        // Collect results, preserving order by worker ID
        let mut results: Vec<(String, String)> = Vec::new();
        for outcome in outcomes {
            match outcome {
                Ok((id, Ok(text))) => results.push((id, text)),
                Ok((id, Err(e))) => {
                    results.push((id, format!("ERROR: {e}")));
                }
                Err(join_err) => {
                    results.push(("(unknown)".into(), format!("join error: {join_err}")));
                }
            }
        }

        // Sort by worker ID for deterministic output
        results.sort_by(|a, b| a.0.cmp(&b.0));

        Ok(results
            .into_iter()
            .map(|(id, text)| format!("=== {id} ===\n{text}"))
            .collect::<Vec<_>>()
            .join("\n\n"))
    }

    /// Sequential mode: workers run one after another.
    async fn execute_sequential(
        &self,
        manifest: &AgentManifest,
        prompt: &str,
        max_steps: usize,
        child_depth: usize,
    ) -> Result<String> {
        if manifest.is_empty() {
            return Err(Error::BadToolArgs {
                name: "agent".into(),
                message: "mode 'sequential' requires at least one manifest entry".to_string(),
            });
        }

        // Collect keys in stable order
        let mut keys: Vec<&String> = manifest.keys().collect();
        keys.sort();

        let mut result_parts = Vec::new();
        for worker_id in &keys {
            let entry = &manifest[*worker_id];
            let result = self
                .run_worker(worker_id, entry, prompt, max_steps, child_depth)
                .await?;
            result_parts.push(result);
        }

        Ok(result_parts.join("\n\n"))
    }

    // ------------------------------------------------------------------
    // Manifest validation
    // ------------------------------------------------------------------

    /// Parse a JSON Value into an AgentManifest, with helpful error messages.
    ///
    /// Supports named-definition resolution: if a manifest entry includes a
    /// `definition` field, the entry is resolved from the loaded
    /// `AgentDefinitions` registry.  Inline `system_prompt` and
    /// `allowed_tools` override the definition's values when both are
    /// provided.
    fn parse_manifest(&self, value: &Value) -> Result<AgentManifest, Error> {
        let obj = value.as_object().ok_or_else(|| Error::BadToolArgs {
            name: "agent".into(),
            message:
                "`manifest` must be a JSON object mapping worker_id → {system_prompt | definition, allowed_tools?}"
                    .to_string(),
        })?;

        if obj.is_empty() {
            return Err(Error::BadToolArgs {
                name: "agent".into(),
                message: "`manifest` must have at least one entry".to_string(),
            });
        }

        let mut manifest = AgentManifest::new();
        for (worker_id, entry_val) in obj {
            let entry_obj = entry_val.as_object().ok_or_else(|| Error::BadToolArgs {
                name: "agent".into(),
                message: format!(
                    "manifest entry '{}' must be an object with `system_prompt` or `definition` (and optional `allowed_tools`)",
                    worker_id
                ),
            })?;

            // --- Resolve definition (if any) ---
            let def_name = entry_obj.get("definition").and_then(|v| v.as_str());

            let (base_system_prompt, base_allowed_tools) = if let Some(name) = def_name {
                let defs = self.definitions.as_ref().ok_or_else(|| {
                    Error::BadToolArgs {
                        name: "agent".into(),
                        message: format!(
                            "manifest entry '{}' references definition '{}', but no agent definitions are loaded (missing .recursive/agents/)",
                            worker_id, name
                        ),
                    }
                })?;
                let def = defs.get(name).ok_or_else(|| Error::BadToolArgs {
                    name: "agent".into(),
                    message: format!(
                        "manifest entry '{}' references unknown definition '{}'. Available: {}",
                        worker_id,
                        name,
                        defs.iter()
                            .map(|(n, _)| n.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                })?;
                (def.system_prompt.clone(), def.allowed_tools.clone())
            } else {
                (String::new(), Vec::new())
            };

            // --- Resolve system_prompt: inline wins over definition ---
            let system_prompt = entry_obj
                .get("system_prompt")
                .and_then(|v| v.as_str())
                .map(String::from)
                .or({
                    if !base_system_prompt.is_empty() {
                        Some(base_system_prompt)
                    } else {
                        None
                    }
                })
                .ok_or_else(|| Error::BadToolArgs {
                    name: "agent".into(),
                    message: format!(
                        "manifest entry '{}' requires a `system_prompt` string or a `definition` reference",
                        worker_id
                    ),
                })?;

            // --- Resolve allowed_tools: inline wins over definition ---
            let allowed_tools: Vec<String> = entry_obj
                .get("allowed_tools")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .or({
                    if !base_allowed_tools.is_empty() {
                        Some(base_allowed_tools)
                    } else {
                        None
                    }
                })
                .unwrap_or_default();

            manifest.insert(
                worker_id.clone(),
                WorkerManifestEntry {
                    system_prompt,
                    allowed_tools,
                },
            );
        }
        Ok(manifest)
    }
}

#[async_trait]
impl Tool for AgentTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "agent".into(),
            // The coordinator guidance (how/when to delegate, writing worker
            // prompts, never delegating understanding, continue vs spawn,
            // verification) is intentionally embedded in the tool description
            // rather than the system prompt. This mirrors the fake-cc pattern:
            // the model sees it only when the `agent` tool is registered (i.e.
            // sub-agent is enabled), so disabling sub-agent removes both the
            // tool and its token cost, and the base system prompt stays stable
            // for users who never delegate.
            description: format!(
                "{}\n\n{}",
                concat!(
                    "Spawn one or more specialist sub-agents (workers) defined by a `manifest`. ",
                    "Use `mode: \"single\"` for one worker, `mode: \"parallel\"` for concurrent ",
                    "execution, or `mode: \"sequential\"` when each worker depends on the previous. ",
                    "Set `background: true` (with `single`) to spawn a long-lived worker that ",
                    "returns a `task_id` immediately and can be continued across turns. ",
                    "Workers have restricted tool sets and isolated transcripts."
                ),
                crate::multi::coordinator_system_prompt()
            )
            .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "mode": {
                        "type": "string",
                        "enum": ["single", "parallel", "sequential"],
                        "description": "Execution mode. 'single' spawns exactly one worker (manifest must have one entry). 'parallel' runs all workers concurrently. 'sequential' runs workers one after another.",
                        "default": "single"
                    },
                    "manifest": {
                        "type": "object",
                        "description": "Map of worker_id → { system_prompt, allowed_tools? }. Each worker gets its own system prompt and restricted tool set.",
                        "additionalProperties": {
                            "type": "object",
                            "properties": {
                                "system_prompt": {
                                    "type": "string",
                                    "description": "System prompt defining the worker's role, behavior, and output format."
                                },
                                "allowed_tools": {
                                    "type": "array",
                                    "items": { "type": "string" },
                                    "description": "Optional tool allowlist. Empty/absent defaults to read-only tools: Read, Grep, Glob, WebFetch, SearchFiles."
                                }
                            },
                            "required": ["system_prompt"]
                        }
                    },
                    "prompt": {
                        "type": "string",
                        "description": "The task description / goal for the worker(s). Every worker receives the same prompt."
                    },
                    "max_steps": {
                        "type": "integer",
                        "description": "Maximum steps per worker (default 30, max 100).",
                        "default": 30
                    },
                    "background": {
                        "type": "boolean",
                        "description": "If true (and mode is 'single'), spawn the worker in the background and return its task_id immediately instead of waiting for it to finish. The worker runs on a long-lived runtime; use send_message(task_id=...) to run follow-up turns, and task_get/task_output/task_stop to inspect or cancel.",
                        "default": false
                    }
                },
                "required": ["manifest", "prompt"]
            }),
        }
    }

    fn side_effect_class(&self) -> ToolSideEffect {
        // The agent tool may spawn workers that write files, so it's External
        // by default.  Individual workers within a manifest can be constrained
        // to read-only via their `allowed_tools`.
        ToolSideEffect::External
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        // --- Resolve mode ---
        let mode_str = arguments
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("single");
        let mode = AgentMode::parse(mode_str).ok_or_else(|| Error::BadToolArgs {
            name: "agent".into(),
            message: format!(
                "unknown mode '{mode_str}'. Valid modes: single, parallel, sequential"
            ),
        })?;

        // --- Resolve prompt ---
        let prompt = arguments["prompt"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "agent".into(),
                message: "missing required parameter: prompt".to_string(),
            })?;

        // --- Resolve max_steps ---
        let max_steps = arguments["max_steps"].as_i64().unwrap_or(30).clamp(1, 100) as usize;

        // --- Resolve background flag ---
        let background = arguments
            .get("background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // --- Parse manifest ---
        let manifest = self.parse_manifest(&arguments["manifest"])?;

        // --- Depth limit check ---
        if self.current_depth >= self.max_depth {
            return Ok(format!(
                "ERROR: agent depth limit reached (max_depth={}). Cannot spawn deeper agents.",
                self.max_depth
            ));
        }

        let child_depth = self.current_depth + 1;

        // --- Background mode (single only): spawn and return task_id ---
        if background && matches!(mode, AgentMode::Single) {
            if manifest.len() != 1 {
                return Err(Error::BadToolArgs {
                    name: "agent".into(),
                    message: format!(
                        "background=true with mode 'single' requires exactly one manifest entry, got {}",
                        manifest.len()
                    ),
                });
            }
            let (worker_id, entry) = manifest.iter().next().ok_or_else(|| Error::BadToolArgs {
                name: "agent".into(),
                message: "background=true requires one manifest entry".to_string(),
            })?;
            let task_id = self
                .spawn_background_worker(worker_id, entry, prompt, max_steps, child_depth)
                .await?;
            return Ok(format!(
                "Background worker '{worker_id}' spawned as task '{task_id}'. \
                 Use send_message(task_id=\"{task_id}\", ...) to run follow-up turns, \
                 task_get/task_output to inspect, or task_stop to cancel."
            ));
        }

        // --- Dispatch (foreground) ---
        match mode {
            AgentMode::Single => {
                self.execute_single(&manifest, prompt, max_steps, child_depth)
                    .await
            }
            AgentMode::Parallel => {
                self.execute_parallel(&manifest, prompt, max_steps, child_depth)
                    .await
            }
            AgentMode::Sequential => {
                self.execute_sequential(&manifest, prompt, max_steps, child_depth)
                    .await
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Completion, MockProvider};
    use crate::tools::{
        GlobTool, LocalTransport, ReadFile, SearchFiles, ToolTransport, WebFetch, WriteFile,
    };

    fn mock_provider(script: Vec<Completion>) -> Arc<dyn ChatProvider> {
        Arc::new(MockProvider::new(script))
    }

    fn full_tool_registry(workspace: &std::path::Path) -> ToolRegistry {
        let transport: Arc<dyn ToolTransport> = Arc::new(LocalTransport);
        ToolRegistry::new(transport)
            .register(Arc::new(ReadFile::new(workspace)))
            .register(Arc::new(SearchFiles::new(workspace)))
            .register(Arc::new(WriteFile::new(workspace)))
            .register(Arc::new(GlobTool::new(workspace)))
            .register(Arc::new(WebFetch::new()))
    }

    #[tokio::test]
    async fn agent_single_mode_basic() {
        let provider = mock_provider(vec![Completion {
            content: "done".to_string(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]);

        let tmp = tempfile::tempdir().unwrap();
        let all_tools = full_tool_registry(tmp.path());
        let agent = AgentTool::new(tmp.path(), provider, all_tools, 2, 0, None);

        let result = agent
            .execute(json!({
                "mode": "single",
                "manifest": {
                    "helper": {
                        "system_prompt": "You are a helper.",
                        "allowed_tools": ["Read"]
                    }
                },
                "prompt": "say hi"
            }))
            .await
            .unwrap();

        assert!(result.contains("helper"));
        assert!(result.contains("NoMoreToolCalls"));
        assert!(result.contains("done"));
    }

    #[tokio::test]
    async fn agent_depth_limit() {
        let provider = mock_provider(vec![]);
        let tmp = tempfile::tempdir().unwrap();
        let all_tools = full_tool_registry(tmp.path());
        // current_depth == max_depth → should refuse
        let agent = AgentTool::new(tmp.path(), provider, all_tools, 2, 2, None);

        let result = agent
            .execute(json!({
                "manifest": {
                    "w": { "system_prompt": "hi" }
                },
                "prompt": "test"
            }))
            .await
            .unwrap();

        assert!(result.contains("depth limit reached"));
    }

    #[tokio::test]
    async fn agent_sequential() {
        let provider = mock_provider(vec![
            Completion {
                content: "first".to_string(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "second".to_string(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]);

        let tmp = tempfile::tempdir().unwrap();
        let all_tools = full_tool_registry(tmp.path());
        let agent = AgentTool::new(tmp.path(), provider, all_tools, 2, 0, None);

        let result = agent
            .execute(json!({
                "mode": "sequential",
                "manifest": {
                    "a": { "system_prompt": "A", "allowed_tools": ["Read"] },
                    "b": { "system_prompt": "B", "allowed_tools": ["Read"] }
                },
                "prompt": "process"
            }))
            .await
            .unwrap();

        assert!(result.contains("first"));
        assert!(result.contains("second"));
    }

    #[test]
    fn test_agent_mode_parse() {
        assert_eq!(AgentMode::parse("single"), Some(AgentMode::Single));
        assert_eq!(AgentMode::parse("parallel"), Some(AgentMode::Parallel));
        assert_eq!(AgentMode::parse("sequential"), Some(AgentMode::Sequential));
        assert_eq!(AgentMode::parse("unknown"), None);
    }

    #[test]
    fn parse_manifest_empty_object_is_error() {
        // kills `if obj.is_empty() { return Err(...) }` guard removal mutation
        let tmp = tempfile::tempdir().unwrap();
        let all_tools = full_tool_registry(tmp.path());
        let agent = AgentTool::new(tmp.path(), mock_provider(vec![]), all_tools, 2, 0, None);
        let err = agent.parse_manifest(&json!({})).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("at least one entry"),
            "empty manifest must error; got: {msg}"
        );
    }

    #[test]
    fn parse_manifest_non_object_is_error() {
        // kills `value.as_object().ok_or_else(...)` mutation
        let tmp = tempfile::tempdir().unwrap();
        let all_tools = full_tool_registry(tmp.path());
        let agent = AgentTool::new(tmp.path(), mock_provider(vec![]), all_tools, 2, 0, None);
        let err = agent.parse_manifest(&json!("not an object")).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("must be a JSON object"),
            "non-object manifest must error; got: {msg}"
        );
    }

    // ------------------------------------------------------------------
    // Definition resolution tests
    // ------------------------------------------------------------------

    #[test]
    fn parse_manifest_resolves_definition() {
        let tmp = tempfile::tempdir().unwrap();
        let all_tools = full_tool_registry(tmp.path());

        // Populate agent definitions
        let agents_dir = tmp.path().join(".recursive").join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("reviewer.md"),
            "---
name: reviewer
system_prompt: 'You review code.'
allowed_tools:
  - Read
  - Glob
---
",
        )
        .unwrap();

        let defs = AgentDefinitions::load(tmp.path()).unwrap();
        let agent = AgentTool::new(tmp.path(), mock_provider(vec![]), all_tools, 2, 0, None)
            .with_definitions(defs);

        let manifest = agent
            .parse_manifest(&json!({
                "rev": { "definition": "reviewer" }
            }))
            .unwrap();

        let entry = manifest.get("rev").unwrap();
        assert_eq!(entry.system_prompt, "You review code.");
        assert_eq!(entry.allowed_tools, vec!["Read", "Glob"]);
    }

    #[test]
    fn parse_manifest_definition_with_inline_override() {
        let tmp = tempfile::tempdir().unwrap();
        let all_tools = full_tool_registry(tmp.path());

        let agents_dir = tmp.path().join(".recursive").join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("helper.md"),
            "---
name: helper
system_prompt: 'Base prompt.'
allowed_tools:
  - Read
---
",
        )
        .unwrap();

        let defs = AgentDefinitions::load(tmp.path()).unwrap();
        let agent = AgentTool::new(tmp.path(), mock_provider(vec![]), all_tools, 2, 0, None)
            .with_definitions(defs);

        // Inline system_prompt overrides the definition's
        let manifest = agent
            .parse_manifest(&json!({
                "h": {
                    "definition": "helper",
                    "system_prompt": "Overridden prompt.",
                    "allowed_tools": ["Write"]
                }
            }))
            .unwrap();

        let entry = manifest.get("h").unwrap();
        assert_eq!(entry.system_prompt, "Overridden prompt.");
        assert_eq!(entry.allowed_tools, vec!["Write"]);
    }

    #[test]
    fn parse_manifest_unknown_definition_is_error() {
        let tmp = tempfile::tempdir().unwrap();
        let all_tools = full_tool_registry(tmp.path());

        // Empty registry
        let defs = AgentDefinitions::load(tmp.path()).unwrap();
        let agent = AgentTool::new(tmp.path(), mock_provider(vec![]), all_tools, 2, 0, None)
            .with_definitions(defs);

        let err = agent
            .parse_manifest(&json!({
                "w": { "definition": "nonexistent" }
            }))
            .unwrap_err();

        let msg = format!("{err}");
        assert!(msg.contains("unknown definition"), "got: {msg}");
        assert!(msg.contains("nonexistent"), "got: {msg}");
    }

    #[test]
    fn parse_manifest_definition_without_registry_is_error() {
        let tmp = tempfile::tempdir().unwrap();
        let all_tools = full_tool_registry(tmp.path());

        // No with_definitions() call — definitions is None
        let agent = AgentTool::new(tmp.path(), mock_provider(vec![]), all_tools, 2, 0, None);

        let err = agent
            .parse_manifest(&json!({
                "w": { "definition": "some-agent" }
            }))
            .unwrap_err();

        let msg = format!("{err}");
        assert!(
            msg.contains("no agent definitions are loaded"),
            "got: {msg}"
        );
    }

    #[test]
    fn parse_manifest_neither_definition_nor_system_prompt_is_error() {
        let tmp = tempfile::tempdir().unwrap();
        let all_tools = full_tool_registry(tmp.path());
        let agent = AgentTool::new(tmp.path(), mock_provider(vec![]), all_tools, 2, 0, None);

        let err = agent
            .parse_manifest(&json!({
                "w": { "allowed_tools": ["Read"] }
            }))
            .unwrap_err();

        let msg = format!("{err}");
        assert!(
            msg.contains("requires a `system_prompt` string or a `definition` reference"),
            "got: {msg}"
        );
    }

    #[tokio::test]
    async fn agent_single_mode_with_definition() {
        let provider = mock_provider(vec![Completion {
            content: "review done".to_string(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]);

        let tmp = tempfile::tempdir().unwrap();
        let all_tools = full_tool_registry(tmp.path());

        // Set up agent definitions
        let agents_dir = tmp.path().join(".recursive").join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("inspector.md"),
            "---
name: inspector
system_prompt: 'Inspect thoroughly.'
allowed_tools:
  - Read
---
",
        )
        .unwrap();

        let defs = AgentDefinitions::load(tmp.path()).unwrap();
        let agent =
            AgentTool::new(tmp.path(), provider, all_tools, 2, 0, None).with_definitions(defs);

        let result = agent
            .execute(json!({
                "mode": "single",
                "manifest": {
                    "inspector": { "definition": "inspector" }
                },
                "prompt": "inspect this"
            }))
            .await
            .unwrap();

        assert!(result.contains("inspector"));
        assert!(result.contains("review done"));
    }

    #[tokio::test]
    async fn background_worker_returns_task_id_and_runs() {
        // background=true in single mode returns a task_id immediately and the
        // worker runs in the background. The first turn's output should appear
        // in the task's output buffer.
        let provider = mock_provider(vec![Completion {
            content: "first turn done".to_string(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]);
        let tmp = tempfile::tempdir().unwrap();
        let all_tools = full_tool_registry(tmp.path());
        let task_registry = Arc::new(TaskRegistry::new());
        let worker_table: WorkerTable = Arc::new(RwLock::new(HashMap::new()));
        let agent = AgentTool::new(tmp.path(), provider, all_tools, 2, 0, None)
            .with_task_registry(task_registry.clone())
            .with_workers(worker_table);

        let result = agent
            .execute(json!({
                "mode": "single",
                "background": true,
                "manifest": {
                    "w1": { "system_prompt": "You are a helper." }
                },
                "prompt": "do the first thing"
            }))
            .await
            .unwrap();

        // Should return a task_id, not a finished worker transcript.
        assert!(result.contains("spawned as task"), "{result}");
        assert!(result.contains("send_message"), "{result}");

        // The task should be registered. Give the background task a moment to
        // run its first turn, then drain output.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let tasks = task_registry.list().await;
        assert_eq!(tasks.len(), 1, "exactly one task should be registered");
        let _ = task_registry.drain_output(&tasks[0].id).await;
        let output = tasks[0].output_snapshot().await;
        let joined = output.join("\n");
        assert!(joined.contains("first turn done"), "output was: {joined}");
    }

    #[tokio::test]
    async fn send_message_continues_background_worker() {
        // A background worker can be continued via send_message(task_id=...):
        // the follow-up runs as a new turn on the same runtime, preserving
        // transcript. We verify both turns' outputs appear.
        let provider = mock_provider(vec![
            Completion {
                content: "turn one".to_string(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "turn two".to_string(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]);
        let tmp = tempfile::tempdir().unwrap();
        let all_tools = full_tool_registry(tmp.path());
        let task_registry = Arc::new(TaskRegistry::new());
        let worker_registry = WorkerRegistry::new();
        let worker_table: WorkerTable = Arc::new(RwLock::new(HashMap::new()));
        let agent = AgentTool::new(tmp.path(), provider, all_tools, 2, 0, None)
            .with_task_registry(task_registry.clone())
            .with_registry(worker_registry.clone())
            .with_workers(worker_table.clone());

        // Spawn the worker in the background.
        let spawn_result = agent
            .execute(json!({
                "mode": "single",
                "background": true,
                "manifest": {
                    "cw": { "system_prompt": "You are a helper." }
                },
                "prompt": "turn one please"
            }))
            .await
            .unwrap();
        let task_id = spawn_result
            .split("task '")
            .nth(1)
            .and_then(|s| s.split('\'').next())
            .expect("task id in spawn result")
            .to_string();

        // Let turn one finish.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        // Send a follow-up via send_message — this drives a new turn.
        let send_tool = SendMessageTool::new(
            worker_registry.clone(),
            task_registry.clone(),
            worker_table.clone(),
        );
        let send_result = send_tool
            .execute(json!({
                "task_id": task_id,
                "message": "now turn two"
            }))
            .await
            .unwrap();
        assert!(
            send_result.contains("new turn"),
            "send_message should report a new turn: {send_result}"
        );

        // Let turn two finish, then drain the full output buffer.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let task = task_registry
            .get(&TaskId(task_id.clone()))
            .await
            .expect("task still registered");
        let _ = task_registry.drain_output(&task.id).await;
        let joined = task.output_snapshot().await.join("\n");
        assert!(joined.contains("turn one"), "missing turn one: {joined}");
        assert!(joined.contains("turn two"), "missing turn two: {joined}");
    }
}
