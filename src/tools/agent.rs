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
use crate::event::{EventSink, WorkerEventSink};
use crate::llm::{ChatProvider, TokenUsage, ToolSpec};
use crate::multi::{AgentManifest, AgentMode, AgentPool, WorkerManifestEntry};
use crate::runtime::{AgentRuntime, AgentRuntimeBuilder};
use crate::tasks::{TaskId, TaskRegistry, TaskState};
use crate::tools::agent_defs::AgentDefinitions;
use crate::tools::artifacts::{
    artifact_reference_text, ArtifactListTool, ArtifactReadTool, ArtifactStore,
};
use crate::tools::edit::EditTool;
use crate::tools::fs::{ReadFile, ReadFileState, WriteFile};
use crate::tools::send_message::{ListWorkersTool, SendMessageTool, WorkerMailbox, WorkerRegistry};
use crate::tools::{PermissionHook, Tool, ToolRegistry, ToolSideEffect};

/// Fallback aggregate deadline for agent dispatches with no configured
/// wall-clock budget (issue #47③). Far above the provider reqwest timeout
/// (180s) times a reasonable `max_steps`, so legitimate long workers are
/// never truncated in production — it only bounds the pathological
/// "no worker ever finishes and nothing is configured" case.
const DEFAULT_FALLBACK_DEADLINE_SECS: u64 = 3600;

fn cancelled_result(worker_id: &str) -> Error {
    Error::BadToolArgs {
        name: "agent".into(),
        message: format!(
            "[worker '{worker_id}' finished: Cancelled]\n(aggregate cancellation; worker did not finish)"
        ),
    }
}

fn timeout_result(worker_id: &str) -> Error {
    Error::BadToolArgs {
        name: "agent".into(),
        message: format!(
            "[worker '{worker_id}' finished: WallClockExceeded]\n(aggregate deadline; worker did not finish)"
        ),
    }
}

/// A finished synchronous worker dispatch: the rendered text plus the reason
/// the worker's runtime stopped.
///
/// `execute_single` needs the reason, not just the text: the dispatch's
/// aggregate deadline and the worker runtime's own wall-clock budget are
/// derived from the same configured value, so which timer fires first is a
/// scheduling race under load. Whenever the worker's own runtime reports a
/// cut-off, single mode surfaces its report as the `Err` — same `Err` outcome
/// as the aggregate branch, so the cut-off does not depend on the race.
struct WorkerReport {
    text: String,
    finish_reason: FinishReason,
}

/// Return value of a single-worker dispatch: a worker-runtime cut-off becomes
/// the same `Err` the aggregate branch produces (whichever timer fired first
/// must not change the shape), and its message is the worker's own report —
/// which names the reason and carries the worker's text/artifact reference,
/// unlike the aggregate placeholder.
fn single_worker_result(report: WorkerReport) -> Result<String> {
    match report.finish_reason {
        FinishReason::WallClockExceeded { .. } | FinishReason::Cancelled => {
            Err(Error::BadToolArgs {
                name: "agent".into(),
                message: report.text,
            })
        }
        _ => Ok(report.text),
    }
}

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
///
/// The lock is a synchronous `std::sync::Mutex` (not tokio's `RwLock`) so a
/// `Drop` guard can remove an entry without awaiting — the `task_stop` abort
/// path unwinds the worker task and must deregister synchronously (goal-379).
/// Critical sections are microsecond-scale (HashMap insert/get/remove).
pub type WorkerTable = Arc<Mutex<HashMap<String, Arc<WorkerHandle>>>>;

/// RAII guard that removes a background worker's `WorkerHandle` from the
/// `WorkerTable` when the worker task exits.
///
/// The worker task normally removes its own entry at the end of the loop
/// (channel close) and on the turn-failure path; but `task_stop` aborts the
/// task's `JoinHandle`, unwinding the task at its next await point and
/// skipping those trailing statements. Because this guard is a local of the
/// spawned async block, its `Drop` runs on every exit path — normal return,
/// panic unwind, AND abort unwind — and the synchronous table lock makes the
/// removal safe from `Drop` (goal-379).
struct WorkerDeregisterGuard {
    workers: WorkerTable,
    worker_key: String,
}

impl Drop for WorkerDeregisterGuard {
    fn drop(&mut self) {
        self.workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.worker_key);
    }
}

/// RAII guard that flags a worker's `WorkerRegistry` mailbox as done when the
/// worker's `run_worker` future ends — on normal return, on an early `?`
/// return (e.g. the runtime failed to build), on panic unwind, and on abort
/// unwind.
///
/// The flag is what lets `WorkerRegistry::register` sweep a finished worker's
/// entry: `register` is the only place the sweep runs (lazy, no background
/// task), so an entry is only reclaimable if its mailbox says the worker is
/// gone. `deregister_sync` normally removes the entry outright; the flag
/// covers the path where that synchronous `try_write` loses its race
/// (goal-408). The abort path additionally marks the mailbox from
/// `execute_parallel`, because an aborted task is not guaranteed to run any of
/// its own code.
struct WorkerMailboxDoneGuard {
    mailbox: Option<WorkerMailbox>,
}

impl Drop for WorkerMailboxDoneGuard {
    fn drop(&mut self) {
        if let Some(mailbox) = &self.mailbox {
            mailbox.mark_done();
        }
    }
}

/// Issue #119: token usage and latency burned by delegated worker runs
/// since the last drain.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct WorkerUsage {
    /// Token usage summed across every worker turn since the last drain.
    pub usage: TokenUsage,
    /// LLM latency summed across every worker turn since the last drain.
    pub llm_latency_ms: u64,
}

/// Issue #119: the bridge between a parent runtime and the tools that
/// dispatch delegated workers.
///
/// Without it a worker's runtime runs against [`crate::event::NullSink`] and
/// its `RuntimeOutcome` usage is dropped on the floor, so worker activity is
/// invisible to the parent's event consumers and worker spend never reaches
/// the parent's cost accounting.
///
/// The parent runtime publishes its event sink here (workers then emit
/// through [`WorkerEventSink`], attributed to the worker) and drains the
/// accumulated [`WorkerUsage`] into the turn that dispatched the workers.
///
/// The sink is held **weakly** for the same reason [`WorkerEventSink`] is: a
/// `background: true` worker outlives the run that spawned it, and a strong
/// reference here would keep the run-scoped sink (and so the CLI printer's
/// channel) alive forever.
#[derive(Default)]
pub struct WorkerTelemetry {
    event_sink: Option<std::sync::Weak<dyn EventSink>>,
    usage: WorkerUsage,
}

/// A shared [`WorkerTelemetry`] handle, installed on both an [`AgentTool`]
/// and the runtime that owns it.
pub type WorkerTelemetrySlot = Arc<Mutex<WorkerTelemetry>>;

impl WorkerTelemetry {
    /// Create an empty bridge (no sink published, zero usage).
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish the parent session's event sink for worker runtimes to emit
    /// through. Overwrites any previously published sink.
    pub fn set_event_sink(&mut self, sink: Arc<dyn EventSink>) {
        self.event_sink = Some(Arc::downgrade(&sink));
    }

    /// The currently published parent sink, if the parent still holds it.
    ///
    /// `None` once the run that published it has ended and dropped its sink.
    pub fn event_sink(&self) -> Option<Arc<dyn EventSink>> {
        self.event_sink.as_ref()?.upgrade()
    }

    /// Add one worker turn's usage and LLM latency.
    pub fn record(&mut self, usage: TokenUsage, llm_latency_ms: u64) {
        self.usage.usage = self.usage.usage.accumulate(usage);
        self.usage.llm_latency_ms = self.usage.llm_latency_ms.saturating_add(llm_latency_ms);
    }

    /// Take (and reset to zero) the usage recorded since the last drain.
    pub fn take_usage(&mut self) -> WorkerUsage {
        std::mem::take(&mut self.usage)
    }
}

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
    /// Goal #106: durable artifact handoff. When set, every worker's final
    /// text is persisted as an artifact; results embed an id + preview
    /// reference instead of relying on the raw inline text surviving
    /// transcript trim/compaction.
    artifacts: Option<Arc<ArtifactStore>>,
    task_registry: Arc<TaskRegistry>,
    definitions: Option<AgentDefinitions>,
    /// Background worker continuation table (worker_id → handle). Populated
    /// when a worker is spawned in the background; `send_message` reads here
    /// to continue a worker across turns.
    workers: WorkerTable,
    /// Wall-clock budget propagated to worker runtimes (Goal 399 semantics:
    /// 0 = unbounded; children must not outlive the parent's budget).
    wall_timeout_secs: u64,
    /// Fallback aggregate deadline (secs) when `wall_timeout_secs == 0`.
    /// Issue #47③: even an unconfigured dispatch must be bounded so a
    /// stalled worker cannot park the parent turn forever. Default is far
    /// above the provider's 180s request timeout × reasonable max_steps.
    fallback_deadline_secs: u64,
    /// Cancellation token propagated to worker runtimes. Workers receive a
    /// CHILD token so a parent cancel stops all workers, while one worker's
    /// own cancellation cannot affect its siblings (issue #40).
    shutdown_token: Option<tokio_util::sync::CancellationToken>,
    /// Per-turn token slot (TUI): the host refreshes the token at each turn
    /// start. Only consulted when `shutdown_token` is unset (static wins).
    shutdown_token_slot: Option<crate::multi::SharedTokenSlot>,
    /// Issue #119: shared bridge to the parent runtime. When set, worker
    /// runtimes emit through the parent's event sink (attributed) and their
    /// usage is accumulated for the parent turn to drain.
    telemetry: Option<WorkerTelemetrySlot>,
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
            artifacts: None,
            task_registry: Arc::new(TaskRegistry::new()),
            definitions: None,
            workers: Arc::new(Mutex::new(HashMap::new())),
            wall_timeout_secs: 0,
            fallback_deadline_secs: DEFAULT_FALLBACK_DEADLINE_SECS,
            shutdown_token: None,
            shutdown_token_slot: None,
            telemetry: None,
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

    /// Attach the durable artifact store (goal #106). When set, worker
    /// results are persisted as artifacts and referenced by id + preview.
    pub fn with_artifact_store(mut self, store: Arc<ArtifactStore>) -> Self {
        self.artifacts = Some(store);
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

    /// Propagate the parent's wall-clock budget to worker runtimes
    /// (issue #40; 0 = unbounded, matching Goal 399 semantics).
    pub fn with_wall_timeout_secs(mut self, secs: u64) -> Self {
        self.wall_timeout_secs = secs;
        self
    }

    /// Override the fallback aggregate deadline used when no wall-clock
    /// budget is configured (issue #47③). Mainly for tests.
    pub fn with_fallback_deadline_secs(mut self, secs: u64) -> Self {
        self.fallback_deadline_secs = secs;
        self
    }

    /// Resolve the effective aggregate deadline for a dispatch
    /// (issue #47②③): configured wall timeout if set, else the fallback
    /// bound so an unconfigured dispatch is never unbounded.
    fn effective_deadline(&self) -> std::time::Instant {
        let secs = if self.wall_timeout_secs > 0 {
            self.wall_timeout_secs
        } else {
            self.fallback_deadline_secs
        };
        std::time::Instant::now() + std::time::Duration::from_secs(secs)
    }

    /// Propagate a cancellation token so Ctrl-C / host interrupt can stop
    /// hanging workers (issue #40).
    pub fn with_shutdown_token(mut self, token: tokio_util::sync::CancellationToken) -> Self {
        self.shutdown_token = Some(token);
        self
    }

    /// Attach a per-turn token slot (TUI). The host stores the current turn's
    /// CancellationToken into the slot; each worker invocation clones it out
    /// at dispatch time, so a Ctrl-C interrupt reaches in-flight parallel
    /// workers via the same child-token tree as a static token.
    pub fn with_shutdown_token_slot(mut self, slot: crate::multi::SharedTokenSlot) -> Self {
        self.shutdown_token_slot = Some(slot);
        self
    }

    /// Attach the parent runtime's telemetry bridge (issue #119). When set,
    /// worker runtimes emit their events through the parent's event sink
    /// (wrapped in [`crate::event::AgentEvent::WorkerEvent`]) and accumulate
    /// their token usage for the parent turn to drain.
    pub fn with_worker_telemetry(mut self, slot: WorkerTelemetrySlot) -> Self {
        self.telemetry = Some(slot);
        self
    }

    /// The parent sink a worker should emit through, if the parent runtime
    /// published one. `None` when no telemetry bridge is attached or the
    /// parent has not set a sink yet.
    fn worker_event_sink(
        &self,
        worker_id: &str,
        task_id: Option<&str>,
    ) -> Option<Arc<dyn EventSink>> {
        let sink = self
            .telemetry
            .as_ref()?
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .event_sink()?;
        let mut wrapped = WorkerEventSink::new(sink, worker_id);
        if let Some(task_id) = task_id {
            wrapped = wrapped.with_task_id(task_id);
        }
        Some(Arc::new(wrapped))
    }

    /// Record one worker turn's usage into the telemetry bridge (issue #119).
    fn record_worker_usage(&self, outcome: &crate::runtime::RuntimeOutcome) {
        if let Some(slot) = &self.telemetry {
            slot.lock()
                .unwrap_or_else(|e| e.into_inner())
                .record(outcome.total_usage, outcome.llm_latency_ms);
        }
    }

    /// Resolve the effective parent token: a static `shutdown_token` (if set)
    /// wins; otherwise the current value of the per-turn slot (if attached).
    /// The slot lock is only held for the clone (microsecond scale, no await).
    fn effective_shutdown_token(&self) -> Option<tokio_util::sync::CancellationToken> {
        if let Some(token) = &self.shutdown_token {
            return Some(token.clone());
        }
        let slot = self.shutdown_token_slot.as_ref()?;
        slot.lock().unwrap_or_else(|e| e.into_inner()).clone()
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
    /// The worker is its own session (Goal 394): its tools come from a
    /// session fork of the parent, so every session-scoped slot a tool holds
    /// is private to this worker — above all the deliverables ledger, else
    /// parallel workers would share one baseline and one `presented` list and
    /// clobber each other's turn ledger.
    ///
    /// `retain_tools` marks the surface as an explicit allow-list decision,
    /// so `AgentRuntimeBuilder::build` does not re-inject tools (`TodoWrite`,
    /// `Present`) this list left out — a read-only `explore`/`plan` worker
    /// must not advertise a mutating tool.
    ///
    /// Sub-agents receive a **fresh** `ReadFileState` so their read history
    /// is independent from the parent's.
    fn build_sub_registry(&self, tool_names: &[String]) -> ToolRegistry {
        let sub_read_state = Arc::new(Mutex::new(ReadFileState::new()));
        // Fork once: `source` owns the worker's private session state and the
        // tools bound to it.
        let source = self.all_tools.fork_session();
        // Start from parent's transport/permissions/policy (and its
        // background-job manager) but override read_file_state with a fresh
        // instance for isolation, and adopt the fork's own ledger.
        let mut reg = self
            .all_tools
            .with_same_transport()
            .with_read_file_state(sub_read_state.clone())
            .with_deliverables(source.deliverables());
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
                    if let Some(t) = source.get(name) {
                        t
                    } else {
                        continue;
                    }
                }
            };
            reg = reg.register(tool);
        }
        reg.retain_tools(tool_names);
        // Issue #134: `RunCode` carries the registry a program may call, and
        // the instance inherited above holds the PARENT's registry — a program
        // would reach tools outside this worker's `allowed_tools`. Rebuild it
        // over the restricted registry (no-op when the worker was not given
        // `RunCode`; `binding_names` excludes `RunCode` itself, so a program
        // can never recurse).
        if reg
            .find_by_name(crate::tools::run_code::RUN_CODE_TOOL_NAME)
            .is_some()
        {
            let restricted = reg.clone();
            reg = reg.register(Arc::new(crate::tools::run_code::RunCode::new(restricted)));
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
    /// `task_id` is the background task id when the worker runs as one, used
    /// only to attribute its forwarded events (issue #119).
    async fn build_worker_runtime(
        &self,
        worker_id: &str,
        task_id: Option<&str>,
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
            // Goal 394: real session isolation — the child must not inherit
            // (or be observed through) the parent's read-before-edit guard,
            // touched-files, background jobs, or sandbox-root expansions.
            self.all_tools.fork_session(),
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
        // Goal #106: descendants share the same durable artifact store.
        if let Some(store) = &self.artifacts {
            child_agent = child_agent.with_artifact_store(store.clone());
        }
        // Always propagate the task registry and worker table so descendants
        // share coordination state with the coordinator. Issue #40: propagate
        // the wall budget and a CHILD cancellation token (parent cancel → all
        // workers cancel; workers stay independent of each other).
        child_agent = child_agent
            .with_task_registry(self.task_registry.clone())
            .with_workers(self.workers.clone())
            .with_wall_timeout_secs(self.wall_timeout_secs);
        if let Some(token) = self.effective_shutdown_token() {
            child_agent = child_agent.with_shutdown_token(token.child_token());
        }
        // Issue #119: descendants report their workers' telemetry up the same
        // bridge, so a nested dispatch's usage still lands on the root turn.
        if let Some(telemetry) = &self.telemetry {
            child_agent = child_agent.with_worker_telemetry(telemetry.clone());
        }
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

        // Goal #106: artifact tools so workers can hand off / consume large
        // outputs through the durable store instead of inline transcripts.
        if let Some(store) = &self.artifacts {
            sub_registry = sub_registry.register(Arc::new(ArtifactReadTool::new(store.clone())));
            sub_registry = sub_registry.register(Arc::new(ArtifactListTool::new(store.clone())));
        }

        // Build the system prompt with shared-memory context
        let mut system_prompt = entry.system_prompt.clone();
        if let Some(pool) = &self.pool {
            let memory_ctx = pool.read().await.memory().to_context_string().await;
            if !memory_ctx.is_empty() {
                system_prompt = format!("{}\n\n{}", system_prompt, memory_ctx);
            }
        }

        let mut builder = AgentRuntimeBuilder::new()
            .llm(self.provider.clone())
            .tools(sub_registry)
            .max_steps(max_steps)
            .system_prompt(system_prompt)
            .wall_timeout_secs(self.wall_timeout_secs);
        if let Some(token) = self.effective_shutdown_token() {
            builder = builder.shutdown_token(token.child_token());
        }
        // Issue #119: emit the worker's events through the parent's sink,
        // attributed to this worker, instead of dropping them into the
        // default `NullSink`.
        if let Some(sink) = self.worker_event_sink(worker_id, task_id) {
            builder = builder.event_sink(sink);
        }
        builder.build().map_err(|e| Error::Tool {
            name: "agent".into(),
            call_id: None,
            message: format!("failed to build worker '{}' runtime: {e}", worker_id),
        })
    }

    /// Run a single worker synchronously and return its report (rendered text
    /// plus why the runtime stopped).
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
    ) -> Result<WorkerReport> {
        // Snapshot the mailbox this worker was pre-registered with (parallel
        // mode) and flag it done however this call ends — every return path,
        // including the `?` on a runtime-build failure and the cut-off
        // finish reasons below. Single/sequential mode (or no registry) has
        // nothing registered, so the guard is a no-op.
        let mailbox = match &self.registry {
            Some(registry) => registry.get(worker_id).await,
            None => None,
        };
        let _done_guard = WorkerMailboxDoneGuard { mailbox };

        let mut runtime = self
            .build_worker_runtime(worker_id, None, entry, max_steps, child_depth)
            .await?;
        let outcome = runtime.run(prompt).await.map_err(|e| Error::Tool {
            name: "agent".into(),
            call_id: None,
            message: format!("worker '{}' failed: {e}", worker_id),
        })?;

        // Issue #119: bill the worker's spend to the parent turn.
        self.record_worker_usage(&outcome);

        let finish_label = match &outcome.finish_reason {
            FinishReason::NoMoreToolCalls => "NoMoreToolCalls".to_string(),
            FinishReason::BudgetExceeded => "BudgetExceeded".to_string(),
            FinishReason::ProviderStop(r) => r.clone(),
            FinishReason::Stuck { .. } => "Stuck".to_string(),
            FinishReason::TranscriptLimit { .. } => "TranscriptLimit".to_string(),
            FinishReason::Cancelled => "Cancelled".to_string(),
            FinishReason::PermissionDenialLimit => "PermissionDenialLimit".to_string(),
            FinishReason::WallClockExceeded { .. } => "WallClockExceeded".to_string(),
        };

        let final_text = outcome
            .final_text
            .unwrap_or_else(|| "(no final message)".to_string());

        // Goal #106: persist the full result as an artifact and return an
        // id + preview reference. Best-effort: when no store is attached or
        // the save fails, fall back to the legacy fully-inline result so the
        // worker's report is never lost.
        if let Some(store) = &self.artifacts {
            match store
                .put(&final_text, format!("{worker_id}-result"), worker_id)
                .await
            {
                Ok(meta) => {
                    return Ok(WorkerReport {
                        text: format!(
                            "[worker '{worker_id}' finished: {finish_label}]\n{}",
                            artifact_reference_text(&meta, &final_text)
                        ),
                        finish_reason: outcome.finish_reason,
                    });
                }
                Err(e) => {
                    tracing::warn!(
                        "agent: artifact persistence failed for worker '{worker_id}', \
                         falling back to inline result: {e}"
                    );
                }
            }
        }

        Ok(WorkerReport {
            text: format!("[worker '{worker_id}' finished: {finish_label}]\n{final_text}"),
            finish_reason: outcome.finish_reason,
        })
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
        // Issue #119: mint the task id before building the runtime (the
        // worker's forwarded events carry it), but register the `TaskState`
        // only after a successful build — a failed build must not leave a
        // registered task behind (the deregister guard lives in the spawned
        // task, which never runs on that path).
        let (state, task_id) =
            TaskState::new(format!("worker '{worker_id}'"), String::new(), worker_id);

        let runtime = self
            .build_worker_runtime(
                worker_id,
                Some(task_id.0.as_str()),
                entry,
                max_steps,
                child_depth,
            )
            .await?;
        let runtime = Arc::new(tokio::sync::Mutex::new(runtime));
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
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(worker_id.to_string(), handle.clone());

        // Spawn the long-lived worker task.
        let state_for_task = state.clone();
        // Issue #119: the spawned task records each worker turn's usage into
        // the parent's telemetry bridge, so background-worker spend lands on
        // the parent turn too.
        let telemetry = self.telemetry.clone();
        // Clone the table + key so the task can deregister itself on EVERY
        // exit path. A `WorkerDeregisterGuard` is a local of the spawned
        // async block, so its `Drop` runs on normal exit (failure return,
        // channel-close completion), on panic unwind, AND on the `task_stop`
        // abort path where `JoinHandle::abort()` unwinds the task past any
        // trailing statements. Without this the WorkerHandle — with its mpsc
        // sender — lingers in the process-wide WorkerTable for the rest of
        // the process: a slow memory + channel-buffer leak for long-running
        // coordinator/HTTP/TUI processes (goal-370 normal exits, goal-379
        // abort path).
        let workers = self.workers.clone();
        let worker_key = worker_id.to_string();
        let join_handle = tokio::spawn(async move {
            // Deregister from the WorkerTable when the task ends, no matter
            // how it ends (see `WorkerDeregisterGuard` docs).
            let _dereg_guard = WorkerDeregisterGuard {
                workers: workers.clone(),
                worker_key: worker_key.clone(),
            };
            // Drain the channel: each message is a new turn on the same runtime.
            while let Some(msg) = rx.recv().await {
                let mut rt = runtime.lock().await;
                match rt.run(&msg).await {
                    Ok(outcome) => {
                        if let Some(slot) = &telemetry {
                            slot.lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .record(outcome.total_usage, outcome.llm_latency_ms);
                        }
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
            state_for_task
                .mark_completed("worker finished".to_string())
                .await;
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
        // Issue #47②: single mode gets the same cancel/deadline bounds as
        // parallel — a hanging provider must not park the parent turn.
        let token = self.effective_shutdown_token().map(|t| t.child_token());
        let deadline = self.effective_deadline();
        let worker = self.run_worker(worker_id, entry, prompt, max_steps, child_depth);
        let mut worker = std::pin::pin!(worker);
        let mut timed_out = false;
        let result = match token.as_ref() {
            Some(t) => {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                tokio::select! {
                    r = worker.as_mut() => r,
                    _ = t.cancelled() => Err(cancelled_result(worker_id)),
                    _ = tokio::time::sleep(remaining) => { timed_out = true; Err(timeout_result(worker_id)) }
                }
            }
            None => {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                match tokio::time::timeout(remaining, worker.as_mut()).await {
                    Ok(r) => r,
                    Err(_) => {
                        timed_out = true;
                        Err(timeout_result(worker_id))
                    }
                }
            }
        };
        if timed_out {
            if let Some(t) = &token {
                t.cancel();
            }
        }
        match result {
            Ok(report) => single_worker_result(report),
            Err(e) => Err(e),
        }
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

        // Pre-register all workers in the registry so they can message each
        // other. The mailboxes are kept here too: the aggregate
        // cancel/deadline branch below must flag the workers it aborts as
        // done itself, because an aborted task is not guaranteed to run any
        // of its own code (goal-408).
        let mut registered: HashMap<String, WorkerMailbox> = HashMap::new();
        if let Some(reg) = &self.registry {
            for worker_id in manifest.keys() {
                registered.insert(worker_id.clone(), reg.register(worker_id).await);
            }
        }

        // Build a self-like AgentTool instance that can be moved into each task.
        // The AgentTool struct is intentionally designed so that each parallel
        // worker gets its own clone of the relevant fields.
        let workspace = self.workspace.clone();
        let provider = self.provider.clone();
        let all_tools = self.all_tools.fork_session();
        let max_depth = self.max_depth;
        let permission_hook = self.permission_hook.clone();
        let registry = self.registry.clone();
        let pool = self.pool.clone();
        let artifacts = self.artifacts.clone();
        let definitions = self.definitions.clone();
        let workers = self.workers.clone();
        let wall_timeout_secs = self.wall_timeout_secs;
        let fallback_deadline_secs = self.fallback_deadline_secs;
        // Issue #119: every parallel worker shares the parent's telemetry
        // bridge so their events and usage all report up.
        let telemetry = self.telemetry.clone();
        // Child token: cancelling the parent cancels all workers at once;
        // a single worker's runtime never cancels its siblings.
        let child_token = self.effective_shutdown_token().map(|t| t.child_token());
        let token_slot = self.shutdown_token_slot.clone();
        let deadline = self.effective_deadline();

        // 结果登记表：worker 任务返回前先把结果写入这里。聚合超时/取消分支据此
        // 抢救已完成 worker 的真实结果——此时 JoinHandle 可能已被 join_all 的
        // 部分轮询消费，再 poll 已完成的句柄会 panic，登记表是唯一可靠出口。
        let rescued: Arc<
            std::sync::Mutex<std::collections::BTreeMap<String, Result<String, String>>>,
        > = Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new()));

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
            let worker_token = child_token.as_ref().map(|t| t.child_token());
            let token_slot = token_slot.clone();
            let rescued = rescued.clone();
            let artifacts = artifacts.clone();
            let telemetry = telemetry.clone();

            handles.push(tokio::spawn(async move {
                // Deregister on every exit path, including the abort path of
                // issue #40's aggregate-cancel branch (the trailing
                // `deregister().await` used to be skipped on abort).
                struct ParallelDeregister {
                    registry: Option<WorkerRegistry>,
                    worker_id: String,
                }
                impl Drop for ParallelDeregister {
                    fn drop(&mut self) {
                        if let Some(reg) = &self.registry {
                            reg.deregister_sync(&self.worker_id);
                        }
                    }
                }
                let _dereg = ParallelDeregister {
                    registry: registry.clone(),
                    worker_id: worker_id.clone(),
                };
                let agent = AgentTool {
                    workspace,
                    provider,
                    all_tools,
                    max_depth,
                    current_depth: child_depth,
                    permission_hook,
                    registry: registry.clone(),
                    pool: pool.clone(),
                    artifacts: artifacts.clone(),
                    task_registry: Arc::new(crate::tasks::TaskRegistry::new()),
                    definitions,
                    workers,
                    wall_timeout_secs,
                    fallback_deadline_secs,
                    shutdown_token: worker_token,
                    shutdown_token_slot: token_slot,
                    telemetry,
                };
                let result = agent
                    .run_worker(&worker_id, &entry, &prompt, max_steps, child_depth)
                    .await
                    .map(|report| report.text);
                let entry = match &result {
                    Ok(text) => Ok(text.clone()),
                    Err(e) => Err(e.to_string()),
                };
                rescued
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(worker_id.clone(), entry);

                (worker_id, result)
            }));
        }

        // Await all handles, bounded by the aggregate cancellation/deadline
        // paths (issue #40): a stalled worker must not park the parent turn
        // forever. When the cancel/timeout branch wins, we cancel the child
        // token, abort unfinished handles, and emit a placeholder result per
        // unfinished worker — invariant #7 (finish is data, not Err) and #8
        // (every dispatched worker has a paired result in the aggregate).
        // `join_all` borrows the handles (`&mut JoinHandle` is a Future), so
        // they remain available for the abort path after the select.
        let mut handles = handles;
        let mut aggregated = false;
        // Label must be decided by WHICH branch fired, BEFORE `cancel()` is
        // called below (cancel() synchronously flips `is_cancelled()`, so
        // post-hoc `is_cancelled()` probing would always say "Cancelled").
        let mut timed_out = false;
        // `deadline` is always `Some` (issue #47③: effective_deadline falls
        // back to a large bound), so the unbounded `(None, None)` shape no
        // longer exists.
        let outcomes: Vec<Result<(String, Result<String>), tokio::task::JoinError>> =
            match &child_token {
                Some(token) => {
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    let mut join = futures_util::future::join_all(handles.iter_mut());
                    tokio::select! {
                        o = &mut join => { aggregated = true; o }
                        _ = token.cancelled() => Vec::new(),
                        _ = tokio::time::sleep(remaining) => { timed_out = true; Vec::new() }
                    }
                }
                None => {
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    let mut join = futures_util::future::join_all(handles.iter_mut());
                    match tokio::time::timeout(remaining, &mut join).await {
                        Ok(o) => {
                            aggregated = true;
                            o
                        }
                        Err(_elapsed) => {
                            timed_out = true;
                            Vec::new()
                        }
                    }
                }
            };

        // If the cancel/timeout branch fired, `outcomes` is empty but handles
        // may still be running: rescue the already-finished workers' real
        // results, cancel the child token (graceful), abort the stragglers, and
        // produce placeholder results so the parent LLM sees one result line
        // per dispatched worker.
        let mut results: Vec<(String, String)> = Vec::new();
        if !aggregated {
            let label = if timed_out {
                "WallClockExceeded"
            } else {
                "Cancelled"
            };
            // 超时/取消分支按登记表抢救已完成 worker 的真实结果；只对登记表里
            // 没有的（未完成）abort + 占位——deadline 一到，先完成的报告不能被
            // "did not finish" 占位整体顶掉。
            //
            // The snapshot MUST be taken before `token.cancel()`: a worker
            // parked in `complete_with_budget` races its provider call against
            // `token.cancelled()`, so cancelling first lets our own cancel
            // surface as `FinishReason::Cancelled`, land in the ledger, and be
            // rescued as "Cancelled" — mislabeling a wall-clock timeout (the
            // race that made
            // `…wall_deadline_labels_wall_clock_exceeded` flake under load).
            let ids: Vec<String> = manifest.keys().cloned().collect();
            debug_assert_eq!(ids.len(), handles.len());
            // Snapshot the whole ledger (id-keyed, not index-keyed) so the
            // rescue lookup below stays coupled to the worker *id* rather than
            // to `handles[idx]`/`manifest.keys()[idx]` positional alignment.
            let rescued_before_cancel = {
                let ledger = rescued.lock().unwrap_or_else(|e| e.into_inner());
                ledger.clone()
            };
            if let Some(token) = &child_token {
                token.cancel();
            }
            for (idx, handle) in handles.into_iter().enumerate() {
                let id = ids.get(idx).cloned().unwrap_or_else(|| "(unknown)".into());
                match rescued_before_cancel.get(&id) {
                    Some(Ok(text)) => results.push((id, text.clone())),
                    Some(Err(e)) => results.push((id, format!("ERROR: {e}"))),
                    None => {
                        // Flag the unfinished worker's mailbox as done *before*
                        // aborting it: the aborted task may never be polled
                        // again, so its own exit guard cannot be relied on.
                        // The entry (if `deregister_sync` also lost its race)
                        // is then reclaimed by the next `register`.
                        if let Some(mailbox) = registered.get(&id) {
                            mailbox.mark_done();
                        }
                        handle.abort();
                        results.push((
                            id.clone(),
                            format!(
                                "[worker '{id}' finished: {label}]\n(aggregate deadline/cancellation; worker did not finish)"
                            ),
                        ));
                    }
                }
            }
        } else {
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
        // Issue #47②: sequential mode gets the same cancel/deadline bounds as
        // parallel, computed once for the whole sequence; a hanging worker
        // must not park the parent turn. Finished parts are still returned
        // (labelled) alongside the cut-off tail — finish is data (#7).
        let token = self.effective_shutdown_token().map(|t| t.child_token());
        let deadline = self.effective_deadline();
        for worker_id in &keys {
            let entry = &manifest[*worker_id];
            let worker = self.run_worker(worker_id, entry, prompt, max_steps, child_depth);
            let mut worker = std::pin::pin!(worker);
            let (result, timed_out) = match &token {
                Some(t) => {
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    tokio::select! {
                        r = worker.as_mut() => (r, false),
                        _ = t.cancelled() => (Err(cancelled_result(worker_id)), false),
                        _ = tokio::time::sleep(remaining) => (Err(timeout_result(worker_id)), true),
                    }
                }
                None => {
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    match tokio::time::timeout(remaining, worker.as_mut()).await {
                        Ok(r) => (r, false),
                        Err(_) => (Err(timeout_result(worker_id)), true),
                    }
                }
            };
            if timed_out {
                // Finish is data (#7): label the cut-off tail and return the
                // parts already completed alongside it, like parallel mode.
                result_parts.push(timeout_result(worker_id).to_string());
                break;
            }
            result_parts.push(result?.text);
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
            ),
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
        // Goal #106: rehydrate the persisted collaboration snapshot exactly
        // once, on the first dispatch. The registration site is synchronous,
        // so it cannot await; this is the first async choke point that holds
        // both the pool and the artifact store.
        if let Some(pool) = &self.pool {
            pool.read().await.ensure_restored().await;
        }
        if let Some(store) = &self.artifacts {
            store.ensure_restored().await;
        }

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
    use crate::deliverables::{Budgets, Deliverables};
    use crate::event::{AgentEvent, NullSink};
    use crate::llm::{Completion, MockProvider};
    use crate::tools::{
        run_code::RunCode, ChangeLedgerTool, GlobTool, LocalTransport, PresentTool, ReadFile,
        SearchFiles, ToolTransport, WebFetch, WriteFile,
    };

    fn mock_provider(script: Vec<Completion>) -> Arc<dyn ChatProvider> {
        Arc::new(MockProvider::new(script))
    }

    fn full_tool_registry(workspace: &std::path::Path) -> ToolRegistry {
        let transport: Arc<dyn ToolTransport> = Arc::new(LocalTransport);
        ToolRegistry::new(transport.clone())
            .register(Arc::new(ReadFile::new(workspace)))
            .register(Arc::new(
                SearchFiles::new(workspace).with_transport(transport.clone()),
            ))
            .register(Arc::new(WriteFile::new(workspace)))
            .register(Arc::new(GlobTool::new(workspace).with_transport(transport)))
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
        let worker_table: WorkerTable = Arc::new(Mutex::new(HashMap::new()));
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

    /// Issue #119 regression (review blocker): a `background: true` worker
    /// parks on its prompt channel and outlives the run that spawned it. It
    /// must not pin the run's sink — with a strong reference the CLI's channel
    /// printer never sees the channel close and `recursive run` hangs at the
    /// end of any run that dispatched a background worker.
    #[tokio::test]
    async fn background_worker_does_not_pin_the_parent_sink() {
        let provider = mock_provider(vec![Completion {
            content: "done".to_string(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]);
        let tmp = tempfile::tempdir().unwrap();
        let all_tools = full_tool_registry(tmp.path());
        let telemetry: WorkerTelemetrySlot = Arc::new(Mutex::new(WorkerTelemetry::new()));
        let (sink, mut rx) = crate::event::ChannelSink::new();
        let parent: Arc<dyn crate::event::EventSink> = Arc::new(sink);
        telemetry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_event_sink(parent.clone());

        let agent = AgentTool::new(tmp.path(), provider, all_tools, 2, 0, None)
            .with_worker_telemetry(telemetry.clone());
        let result = agent
            .execute(json!({
                "mode": "single",
                "background": true,
                "manifest": { "w1": { "system_prompt": "You are a helper." } },
                "prompt": "do the first thing"
            }))
            .await
            .unwrap();
        assert!(result.contains("spawned as task"), "{result}");

        // The spawning run is over. Draining the channel must terminate even
        // though the worker task is still alive and parked on its prompt
        // channel — this is the CLI printer's loop.
        drop(parent);
        let drained = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while rx.recv().await.is_some() {}
        })
        .await;
        assert!(
            drained.is_ok(),
            "a live background worker is still pinning the run's sink"
        );
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
        let worker_table: WorkerTable = Arc::new(Mutex::new(HashMap::new()));
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

    #[tokio::test]
    async fn background_worker_deregisters_from_worker_table_on_exit() {
        // goal-370: a background worker whose turn fails must remove its
        // WorkerHandle (and its mpsc sender) from the process-wide
        // WorkerTable when the task exits. Previously the mark_failed
        // early-return path left the entry in the table forever — a slow
        // memory + channel-buffer leak for long-running coordinator / HTTP /
        // TUI processes. (This test exercises the failure exit path; the
        // channel-close mark_completed path shares the same removal line.
        // A *panicked* task is not covered — its cleanup is out of scope.)
        let provider: Arc<dyn ChatProvider> =
            Arc::new(MockProvider::new(vec![]).with_errors(vec![Error::Llm {
                provider: "mock".into(),
                model: None,
                request_id: None,
                message: "injected failure for deregistration test".into(),
            }]));
        let tmp = tempfile::tempdir().unwrap();
        let all_tools = full_tool_registry(tmp.path());
        let task_registry = Arc::new(TaskRegistry::new());
        let worker_table: WorkerTable = Arc::new(Mutex::new(HashMap::new()));
        let agent = AgentTool::new(tmp.path(), provider, all_tools, 2, 0, None)
            .with_task_registry(task_registry.clone())
            .with_workers(worker_table.clone());

        // Spawn the worker in the background.
        let spawn_result = agent
            .execute(json!({
                "mode": "single",
                "background": true,
                "manifest": {
                    "w1": { "system_prompt": "You are a helper." }
                },
                "prompt": "do the thing"
            }))
            .await
            .unwrap();
        let task_id = spawn_result
            .split("task '")
            .nth(1)
            .and_then(|s| s.split('\'').next())
            .expect("task id in spawn result")
            .to_string();

        // While the worker is alive its entry must be present.
        assert!(
            worker_table
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key("w1"),
            "entry should exist while the worker is alive"
        );

        // The injected provider error makes the first turn fail, so the
        // worker task takes the mark_failed early-return path and exits.
        // Wait (bounded) for the entry to disappear — the property under
        // test. On the old code this loop would time out because the entry
        // was never removed.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if !worker_table
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key("w1")
            {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("worker table entry for 'w1' was not removed after the worker exited");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // Confirm the task actually failed (mark_failed path, not some other
        // exit), so the removal above is attributable to the fix.
        let task = task_registry
            .get(&TaskId(task_id))
            .await
            .expect("task still registered");
        let status = task.status().await;
        assert!(
            matches!(status, crate::tasks::TaskStatus::Failed),
            "worker task should have failed, was {status:?}"
        );
    }

    #[tokio::test]
    async fn task_stop_deregisters_aborted_worker_from_worker_table() {
        // goal-379: `task_stop` aborts the worker's JoinHandle, which unwinds
        // the task at its next await point — skipping trailing cleanup. The
        // `WorkerDeregisterGuard` (a local of the spawned task) must remove
        // the WorkerHandle from the WorkerTable on that abort path too, so a
        // later `send_message` to the worker_id reports "not found" instead
        // of finding a stale handle (memory + channel-buffer leak, goal-370
        // class).
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
        let worker_registry = WorkerRegistry::new();
        let worker_table: WorkerTable = Arc::new(Mutex::new(HashMap::new()));
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
                    "w1": { "system_prompt": "You are a helper." }
                },
                "prompt": "do the first thing"
            }))
            .await
            .unwrap();
        let task_id = spawn_result
            .split("task '")
            .nth(1)
            .and_then(|s| s.split('\'').next())
            .expect("task id in spawn result")
            .to_string();

        // Let the first turn finish so the worker is alive and parked on
        // rx.recv(), then confirm its table entry is present.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(
            worker_table
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key("w1"),
            "entry should exist while the worker is alive"
        );

        // Stop the task: `TaskRegistry::stop` → `TaskState::stop` aborts the
        // JoinHandle, unwinding the worker task (same path TaskStopTool uses,
        // without needing the coordinator-mode feature gate).
        let stopped = task_registry.stop(&TaskId(task_id.clone())).await;
        assert!(stopped, "task_stop must find a live handle to abort");

        // The abort unwind must run the WorkerDeregisterGuard: wait (bounded)
        // for the entry to disappear. On the old code this loop would time
        // out because abort skipped the trailing remove statements.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if !worker_table
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key("w1")
            {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("worker table entry for 'w1' was not removed after task_stop aborted the worker");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // The task itself stays registered (so task_get can report Stopped),
        // but its worker handle is gone.
        let task = task_registry
            .get(&TaskId(task_id.clone()))
            .await
            .expect("task still registered");
        assert_eq!(task.status().await, crate::tasks::TaskStatus::Stopped);

        // A follow-up send_message by worker_id must report "not found"
        // rather than silently buffering into a dead continuation channel.
        let send_tool = SendMessageTool::new(
            worker_registry.clone(),
            task_registry.clone(),
            worker_table.clone(),
        );
        let send_result = send_tool
            .execute(json!({
                "worker_id": "w1",
                "message": "are you there?"
            }))
            .await
            .unwrap();
        assert!(
            send_result.contains("not found"),
            "send_message to a stopped worker must report not found, got: {send_result}"
        );
        assert!(
            !send_result.contains("new turn"),
            "send_message must not claim it will drive a new turn on a stopped worker: {send_result}"
        );

        // The task_id addressing mode still resolves the (stopped) task, but
        // must NOT find a live continuation handle.
        let send_result = send_tool
            .execute(json!({
                "task_id": task_id,
                "message": "anyone home?"
            }))
            .await
            .unwrap();
        assert!(
            !send_result.contains("will run it as a new turn"),
            "stopped worker must not claim a new turn, got: {send_result}"
        );
    }

    /// Issue #40 — parallel mode with a stalled LLM and a wall budget must
    /// return Ok with a paired placeholder result per worker (invariants #7
    /// and #8), not hang forever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_parallel_timeout_produces_paired_results() {
        struct HangingProvider;
        #[async_trait::async_trait]
        impl crate::llm::ChatProvider for HangingProvider {
            async fn complete(
                &self,
                _m: &[crate::message::Message],
                _t: &[crate::llm::ToolSpec],
            ) -> crate::error::Result<Completion> {
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                unreachable!("should have been aborted")
            }
            async fn stream(
                &self,
                _m: &[crate::message::Message],
                _t: &[crate::llm::ToolSpec],
                _tx: Option<crate::llm::StreamSender>,
                _c: Option<tokio_util::sync::CancellationToken>,
            ) -> crate::error::Result<Completion> {
                unreachable!("stream not used")
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let provider: Arc<dyn ChatProvider> = Arc::new(HangingProvider);
        let all_tools = ToolRegistry::new(Arc::new(LocalTransport));
        let agent =
            AgentTool::new(tmp.path(), provider, all_tools, 2, 0, None).with_wall_timeout_secs(1);

        let fut = agent.execute(serde_json::json!({
            "mode": "parallel",
            "manifest": {
                "w0": { "system_prompt": "a", "allowed_tools": [] },
                "w1": { "system_prompt": "b", "allowed_tools": [] }
            },
            "prompt": "go",
            "max_steps": 3
        }));
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), fut)
            .await
            .expect("must terminate via wall budget")
            .expect("execute must return Ok (finish is data, not Err)");
        assert!(result.contains("WallClockExceeded"), "got: {result}");
        assert!(result.contains("=== w0 ==="), "got: {result}");
        assert!(result.contains("=== w1 ==="), "got: {result}");
    }

    /// Issue #40 — cancelling the parent token mid-parallel-run must yield
    /// Ok with Cancelled placeholders for every worker.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_parallel_cancel_produces_paired_results() {
        struct HangingProvider;
        #[async_trait::async_trait]
        impl crate::llm::ChatProvider for HangingProvider {
            async fn complete(
                &self,
                _m: &[crate::message::Message],
                _t: &[crate::llm::ToolSpec],
            ) -> crate::error::Result<Completion> {
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                unreachable!("should have been cancelled")
            }
            async fn stream(
                &self,
                _m: &[crate::message::Message],
                _t: &[crate::llm::ToolSpec],
                _tx: Option<crate::llm::StreamSender>,
                _c: Option<tokio_util::sync::CancellationToken>,
            ) -> crate::error::Result<Completion> {
                unreachable!("stream not used")
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let provider: Arc<dyn ChatProvider> = Arc::new(HangingProvider);
        let all_tools = ToolRegistry::new(Arc::new(LocalTransport));
        let token = tokio_util::sync::CancellationToken::new();
        let agent = AgentTool::new(tmp.path(), provider, all_tools, 2, 0, None)
            .with_shutdown_token(token.clone());

        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            token.cancel();
        });

        let fut = agent.execute(serde_json::json!({
            "mode": "parallel",
            "manifest": {
                "w0": { "system_prompt": "a", "allowed_tools": [] },
                "w1": { "system_prompt": "b", "allowed_tools": [] }
            },
            "prompt": "go",
            "max_steps": 3
        }));
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), fut)
            .await
            .expect("must terminate via cancellation")
            .expect("execute must return Ok");
        assert!(result.contains("Cancelled"), "got: {result}");
        assert!(result.contains("=== w0 ==="), "got: {result}");
        assert!(result.contains("=== w1 ==="), "got: {result}");
    }

    /// Issue #40 — the per-turn token slot resolves to a cancellable token
    /// when no static token is attached, and a static token takes precedence
    /// over the slot when both are present.
    #[test]
    fn token_slot_resolves_and_prefers_static() {
        let tmp = tempfile::tempdir().unwrap();
        let all_tools = ToolRegistry::new(Arc::new(LocalTransport));
        let agent = AgentTool::new(
            tmp.path(),
            mock_provider(vec![]),
            all_tools.clone(),
            2,
            0,
            None,
        );
        // No slot, no static token → None.
        assert!(agent.effective_shutdown_token().is_none());

        // Slot only → resolves the slot's token.
        let slot: crate::multi::SharedTokenSlot = Arc::new(std::sync::Mutex::new(Some(
            tokio_util::sync::CancellationToken::new(),
        )));
        let agent = agent.with_shutdown_token_slot(slot.clone());
        let resolved = agent
            .effective_shutdown_token()
            .expect("slot token must resolve");
        assert!(!resolved.is_cancelled());
        resolved.cancel();
        assert!(
            agent
                .effective_shutdown_token()
                .map(|t| t.is_cancelled())
                .unwrap_or(false),
            "slot resolution must be live (same token)"
        );

        // Empty slot → None.
        *slot.lock().unwrap() = None;
        assert!(agent.effective_shutdown_token().is_none());

        // Static + slot → static wins.
        *slot.lock().unwrap() = Some(tokio_util::sync::CancellationToken::new());
        let static_token = tokio_util::sync::CancellationToken::new();
        let agent = agent.with_shutdown_token(static_token.clone());
        let resolved = agent
            .effective_shutdown_token()
            .expect("static token must resolve");
        static_token.cancel();
        assert!(resolved.is_cancelled(), "static token must take precedence");
    }

    /// Goal 408 — a worker that *does* return flags its own registry mailbox
    /// as done on the way out, so a stale presence entry stays reclaimable
    /// even when `deregister_sync` loses its `try_write` race. Exercised in
    /// single mode: that is the dispatch path where the registry entry is
    /// created by the caller (the parallel path removes its own entry via
    /// `ParallelDeregister`, hiding the flag).
    #[tokio::test]
    async fn run_worker_flags_its_mailbox_done_on_exit() {
        let provider = mock_provider(vec![Completion {
            content: "all done".to_string(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]);
        let tmp = tempfile::tempdir().unwrap();
        let all_tools = full_tool_registry(tmp.path());
        let worker_registry = WorkerRegistry::new();
        let mailbox = worker_registry.register("helper").await;
        let agent = AgentTool::new(tmp.path(), provider, all_tools, 2, 0, None)
            .with_registry(worker_registry.clone());

        let out = agent
            .execute(json!({
                "mode": "single",
                "manifest": { "helper": { "system_prompt": "You are a helper.", "allowed_tools": [] } },
                "prompt": "say hi"
            }))
            .await
            .unwrap();
        assert!(out.contains("NoMoreToolCalls"), "got: {out}");
        assert!(
            mailbox.is_done(),
            "a returned worker must flag its own mailbox as done"
        );

        // ...and the entry it left behind is swept by the next register.
        worker_registry.register("other").await;
        assert!(
            worker_registry.get("helper").await.is_none(),
            "the finished worker's entry must be swept by the next register"
        );
    }

    /// Goal 408 — when the aggregate deadline fires, the unfinished workers
    /// are aborted; an aborted task is not guaranteed to run any of its own
    /// code, so `execute_parallel` must flag their registry mailboxes as done
    /// itself (rather than relying on the worker's exit guard or on
    /// `deregister_sync` winning its `try_write` race). The next `register`
    /// then sweeps the stale entries.
    ///
    /// Deliberately a `current_thread` runtime: the abort is asynchronous,
    /// and the assertions below must observe the registry before the aborted
    /// tasks get a chance to be dropped (nothing between the dispatch
    /// returning and the assertions yields to the scheduler, since the
    /// uncontended `RwLock` reads complete inline).
    #[tokio::test]
    async fn execute_parallel_timeout_flags_unfinished_workers_done() {
        let (agent, _tmp) = hanging_agent();
        let worker_registry = WorkerRegistry::new();
        let agent = agent
            .with_registry(worker_registry.clone())
            .with_wall_timeout_secs(1);

        let fut = agent.execute(serde_json::json!({
            "mode": "parallel",
            "manifest": {
                "w0": { "system_prompt": "a", "allowed_tools": [] },
                "w1": { "system_prompt": "b", "allowed_tools": [] }
            },
            "prompt": "go",
            "max_steps": 3
        }));
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), fut)
            .await
            .expect("must terminate via wall budget")
            .expect("execute must return Ok (finish is data, not Err)");
        // Placeholder pairing is untouched by this goal (invariants #7/#8).
        assert!(result.contains("WallClockExceeded"), "got: {result}");
        assert!(result.contains("=== w0 ==="), "got: {result}");
        assert!(result.contains("=== w1 ==="), "got: {result}");

        // Both workers were pre-registered and neither ever returned, so their
        // entries are still in the table at this instant — and the aggregator
        // must have flagged them done even though their tasks were aborted
        // before they could mark themselves.
        for id in ["w0", "w1"] {
            let mailbox = worker_registry
                .get(id)
                .await
                .expect("unfinished worker must still be registered right after the dispatch");
            assert!(
                mailbox.is_done(),
                "unfinished worker '{id}' must be flagged done by the aggregator"
            );
        }

        // The next register sweeps both stale entries, keeping the new one.
        worker_registry.register("probe").await;
        assert!(
            worker_registry.get("probe").await.is_some(),
            "the newly registered worker must survive the sweep"
        );
        for id in ["w0", "w1"] {
            assert!(
                worker_registry.get(id).await.is_none(),
                "stale entry '{id}' must be swept by the next register"
            );
        }
    }

    /// Issue #47③ — parallel mode with no wall budget configured must still
    /// be bounded by the fallback deadline (overridden short here).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_parallel_unconfigured_wall_timeout_still_bounded() {
        let agent = hanging_agent().0.with_fallback_deadline_secs(2);
        let fut = agent.execute(serde_json::json!({
            "mode": "parallel",
            "manifest": { "w0": { "system_prompt": "a", "allowed_tools": [] } },
            "prompt": "go",
            "max_steps": 3
        }));
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), fut)
            .await
            .expect("fallback deadline must bound the unconfigured dispatch")
            .expect("execute must return Ok");
        assert!(result.contains("WallClockExceeded"), "got: {result}");
    }

    /// Issue #47② — single mode with a cancelled parent token must return
    /// (cancelled semantics), not park forever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_single_cancel_returns_within_budget() {
        let token = tokio_util::sync::CancellationToken::new();
        let agent = hanging_agent().0.with_shutdown_token(token.clone());
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            token.cancel();
        });
        let fut = agent.execute(serde_json::json!({
            "mode": "single",
            "manifest": { "w0": { "system_prompt": "a", "allowed_tools": [] } },
            "prompt": "go",
            "max_steps": 3
        }));
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), fut)
            .await
            .expect("single mode must terminate via cancellation");
        let err = result.expect_err("cancelled single dispatch surfaces the cut-off");
        assert!(err.to_string().contains("Cancelled"), "got: {err}");
    }

    /// Issue #47② — single mode with a short wall budget must return
    /// WallClockExceeded semantics rather than park forever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_single_wall_timeout_returns() {
        let agent = hanging_agent().0.with_wall_timeout_secs(1);
        let fut = agent.execute(serde_json::json!({
            "mode": "single",
            "manifest": { "w0": { "system_prompt": "a", "allowed_tools": [] } },
            "prompt": "go",
            "max_steps": 3
        }));
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), fut)
            .await
            .expect("single mode must terminate via wall budget");
        let err = result.expect_err("timed-out single dispatch surfaces the cut-off");
        assert!(err.to_string().contains("WallClockExceeded"), "got: {err}");
    }

    /// Issue #47② — sequential mode: a hanging first worker must not park
    /// the parent turn; the cut-off surfaces via the fallback deadline.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_sequential_wall_timeout_returns() {
        let agent = hanging_agent().0.with_fallback_deadline_secs(2);
        let fut = agent.execute(serde_json::json!({
            "mode": "sequential",
            "manifest": {
                "w0": { "system_prompt": "a", "allowed_tools": [] },
                "w1": { "system_prompt": "b", "allowed_tools": [] }
            },
            "prompt": "go",
            "max_steps": 3
        }));
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), fut)
            .await
            .expect("sequential mode must terminate via fallback deadline");
        // The cut-off tail is returned as labelled data (finish is data #7),
        // not an Err — matching parallel-mode semantics.
        let out = result.expect("timed-out sequential dispatch returns labelled parts");
        assert!(out.contains("WallClockExceeded"), "got: {out}");
    }

    /// Single mode cut-offs (issue #47②) must not leak *which* of the two
    /// same-budget timers fired first into the result shape, and the message
    /// must be the worker's own report — the aggregate placeholder
    /// ("aggregate deadline; worker did not finish") used to be reused here
    /// while the worker's text/artifact reference was dropped.
    #[test]
    fn single_worker_cutoffs_are_errors_carrying_the_worker_report() {
        let cut_off = WorkerReport {
            text: "[worker 'w0' finished: WallClockExceeded]\n(no final message)".to_string(),
            finish_reason: FinishReason::WallClockExceeded { secs: 1 },
        };
        let err = single_worker_result(cut_off).expect_err("wall-clock cut-off must be an Err");
        assert!(err.to_string().contains("WallClockExceeded"), "got: {err}");
        assert!(
            !err.to_string().contains("aggregate deadline"),
            "the worker's own report is the message, not the aggregate placeholder: {err}"
        );

        let cancelled = WorkerReport {
            text: "[worker 'w0' finished: Cancelled]\n(no final message)".to_string(),
            finish_reason: FinishReason::Cancelled,
        };
        let err = single_worker_result(cancelled).expect_err("cancellation must be an Err");
        assert!(err.to_string().contains("Cancelled"), "got: {err}");

        let finished = WorkerReport {
            text: "[worker 'w0' finished: NoMoreToolCalls]\ndone".to_string(),
            finish_reason: FinishReason::NoMoreToolCalls,
        };
        assert_eq!(
            single_worker_result(finished).expect("a normal finish is the worker's text"),
            "[worker 'w0' finished: NoMoreToolCalls]\ndone"
        );
    }

    /// Test helper (issue #47): an AgentTool whose provider never returns —
    /// kept alive by the returned tempdir.
    fn hanging_agent() -> (AgentTool, tempfile::TempDir) {
        struct HangingProvider;
        #[async_trait::async_trait]
        impl crate::llm::ChatProvider for HangingProvider {
            async fn complete(
                &self,
                _m: &[crate::message::Message],
                _t: &[crate::llm::ToolSpec],
            ) -> crate::error::Result<Completion> {
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                unreachable!("should have been cut off")
            }
            async fn stream(
                &self,
                _m: &[crate::message::Message],
                _t: &[crate::llm::ToolSpec],
                _tx: Option<crate::llm::StreamSender>,
                _c: Option<tokio_util::sync::CancellationToken>,
            ) -> crate::error::Result<Completion> {
                unreachable!("stream not used")
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let provider: Arc<dyn ChatProvider> = Arc::new(HangingProvider);
        let all_tools = ToolRegistry::new(Arc::new(LocalTransport));
        (
            AgentTool::new(tmp.path(), provider, all_tools, 2, 0, None),
            tmp,
        )
    }

    /// Goal #133 / Goal 394: a worker is its own session, so every worker
    /// registry brings its own deliverables ledger. Without this, workers of
    /// one `agent` call share the coordinator's ledger (`begin_turn` clears
    /// the baseline of whoever ran last) and `ChangeLedger` renders the
    /// coordinator's turn.
    #[tokio::test]
    async fn sub_agents_get_their_own_deliverables_ledger() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let coordinator_ledger = Arc::new(
            Deliverables::new(&ws, tmp.path().join("private"), Budgets::default()).unwrap(),
        );
        let tools = full_tool_registry(&ws)
            .with_deliverables(Some(coordinator_ledger.clone()))
            .register(Arc::new(PresentTool::new(
                coordinator_ledger.clone(),
                Arc::new(NullSink),
            )))
            .register(Arc::new(ChangeLedgerTool::new(coordinator_ledger.clone())));
        let agent = AgentTool::new(&ws, mock_provider(vec![]), tools, 2, 0, None);

        // The coordinator's turn is armed and already has a change…
        coordinator_ledger.begin_turn(1);
        coordinator_ledger.ensure_baseline().unwrap();
        std::fs::write(ws.join("coordinator.txt"), "c\n").unwrap();

        let deliverables_tools = ["ChangeLedger".to_string(), "Present".to_string()];
        let a = agent.build_sub_registry(&deliverables_tools);
        let b = agent.build_sub_registry(&deliverables_tools);
        let la = a.deliverables().expect("worker A ledger");
        let lb = b.deliverables().expect("worker B ledger");
        assert!(
            !Arc::ptr_eq(&coordinator_ledger, &la) && !Arc::ptr_eq(&coordinator_ledger, &lb),
            "a worker must not share the coordinator's ledger"
        );
        assert!(
            !Arc::ptr_eq(&la, &lb),
            "two workers must not share one ledger (parallel workers would clobber it)"
        );

        // …which worker A's ChangeLedger must not render.
        let out = a
            .invoke("ChangeLedger", serde_json::json!({}))
            .await
            .unwrap();
        assert!(out.contains("no change ledger"), "{out}");

        // A worker's declaration lands in the worker's ledger only.
        std::fs::write(ws.join("worker.txt"), "w\n").unwrap();
        la.begin_turn(1);
        a.invoke("Present", serde_json::json!({"files": ["worker.txt"]}))
            .await
            .unwrap();
        assert_eq!(la.presented().len(), 1);
        assert!(
            coordinator_ledger.presented().is_empty(),
            "a worker's declaration must not leak into the coordinator ledger"
        );
    }

    /// A worker registry is built from an explicit allow-list, so the runtime
    /// builder must not re-inject tools the list left out — a read-only
    /// worker must not advertise `Present` (or `TodoWrite`).
    #[tokio::test]
    async fn read_only_workers_do_not_advertise_the_deliverables_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let ledger = Arc::new(
            Deliverables::new(&ws, tmp.path().join("private"), Budgets::default()).unwrap(),
        );
        let tools = full_tool_registry(&ws)
            .with_deliverables(Some(ledger.clone()))
            .register(Arc::new(PresentTool::new(
                ledger.clone(),
                Arc::new(NullSink),
            )))
            .register(Arc::new(ChangeLedgerTool::new(ledger.clone())));
        let agent = AgentTool::new(&ws, mock_provider(vec![]), tools, 2, 0, None);

        let ro = agent.build_sub_registry(&["Read".to_string()]);
        assert!(ro.find_by_name("Read").is_some(), "allow-listed tool kept");
        assert!(
            ro.find_by_name("Present").is_none() && ro.find_by_name("ChangeLedger").is_none(),
            "an allow-list that dropped the deliverables tools stays strict"
        );

        let entry = WorkerManifestEntry {
            system_prompt: "read-only".into(),
            allowed_tools: vec![],
        };
        let runtime = agent
            .build_worker_runtime("ro", None, &entry, 3, 0)
            .await
            .expect("worker runtime");
        assert!(
            runtime.kernel().tools().find_by_name("Present").is_none(),
            "the builder must not re-inject Present into a read-only worker"
        );
        let worker_ledger = runtime.deliverables().expect("worker ledger");
        assert!(
            !Arc::ptr_eq(&ledger, &worker_ledger),
            "the worker runtime must run on its own ledger"
        );
    }

    // ── Issue #119: worker telemetry bridge ───────────────────────────────

    #[test]
    fn worker_telemetry_records_and_takes_usage() {
        let mut telemetry = WorkerTelemetry::new();
        assert_eq!(telemetry.take_usage(), WorkerUsage::default());

        telemetry.record(
            TokenUsage {
                prompt_tokens: 10,
                completion_tokens: 4,
                total_tokens: 14,
                ..Default::default()
            },
            100,
        );
        telemetry.record(
            TokenUsage {
                prompt_tokens: 5,
                completion_tokens: 1,
                total_tokens: 6,
                ..Default::default()
            },
            30,
        );
        let taken = telemetry.take_usage();
        assert_eq!(taken.usage.prompt_tokens, 15);
        assert_eq!(taken.usage.completion_tokens, 5);
        assert_eq!(taken.llm_latency_ms, 130);
        // take_usage resets the accumulator.
        assert_eq!(telemetry.take_usage(), WorkerUsage::default());
    }

    #[test]
    fn worker_telemetry_event_sink_round_trips() {
        let mut telemetry = WorkerTelemetry::new();
        assert!(telemetry.event_sink().is_none());
        // The bridge holds the sink weakly, so the owner (here the test, in
        // production the runtime) must outlive it for the lookup to succeed.
        let parent: Arc<dyn EventSink> = Arc::new(NullSink);
        telemetry.set_event_sink(parent.clone());
        assert!(telemetry.event_sink().is_some());
        // …and a released sink is reported as gone rather than resurrected.
        drop(parent);
        assert!(telemetry.event_sink().is_none());
    }

    #[tokio::test]
    async fn worker_telemetry_records_usage_and_forwards_events() {
        let provider = mock_provider(vec![Completion {
            content: "done".to_string(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: Some(TokenUsage {
                prompt_tokens: 11,
                completion_tokens: 7,
                total_tokens: 18,
                ..Default::default()
            }),
            reasoning_content: None,
        }]);

        let tmp = tempfile::tempdir().unwrap();
        let all_tools = full_tool_registry(tmp.path());
        let telemetry: WorkerTelemetrySlot = Arc::new(Mutex::new(WorkerTelemetry::new()));
        let (sink, mut rx) = crate::event::ChannelSink::new();
        // The bridge holds the sink weakly; in production the runtime owns it.
        let parent: Arc<dyn EventSink> = Arc::new(sink);
        telemetry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_event_sink(parent.clone());
        let agent = AgentTool::new(tmp.path(), provider, all_tools, 2, 0, None)
            .with_worker_telemetry(telemetry.clone());

        agent
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

        // Usage reached the bridge…
        let recorded = telemetry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take_usage();
        assert_eq!(recorded.usage.prompt_tokens, 11);
        assert_eq!(recorded.usage.completion_tokens, 7);

        // …and the worker's events were forwarded, attributed (issue #119).
        let mut forwarded = 0usize;
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentEvent::WorkerEvent {
                    worker_id, task_id, ..
                } => {
                    assert_eq!(worker_id, "helper");
                    assert_eq!(task_id, None, "foreground worker has no task id");
                    forwarded += 1;
                }
                other => panic!("worker sink must only forward WorkerEvent, got {other:?}"),
            }
        }
        assert!(forwarded > 0, "worker events must reach the parent sink");
    }

    #[tokio::test]
    async fn worker_without_telemetry_uses_the_null_sink() {
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
        assert!(result.contains("done"));
    }

    /// Issue #134 review: `RunCode` carries the registry a program may call,
    /// so a manifest that *explicitly* lists it must not hand the worker the
    /// PARENT's instance — a program would then reach tools outside the
    /// worker's `allowed_tools`. The worker gets a rebuild over its own
    /// registry; without the entry it gets no `RunCode` at all.
    #[tokio::test]
    async fn a_worker_given_run_code_gets_its_own_binding_set() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let parent =
            full_tool_registry(&ws).register(Arc::new(RunCode::new(full_tool_registry(&ws))));
        let parent_view = parent.clone();
        let agent = AgentTool::new(&ws, mock_provider(vec![]), parent, 2, 0, None);

        let sub = agent.build_sub_registry(&["Read".to_string(), "RunCode".to_string()]);
        let run_code = sub
            .find_by_name("RunCode")
            .expect("an explicit allow-list entry keeps RunCode");
        assert!(
            !Arc::ptr_eq(
                &run_code,
                &parent_view
                    .find_by_name("RunCode")
                    .expect("the parent has a RunCode")
            ),
            "the worker must not share the parent's RunCode instance"
        );
        let description = run_code.spec().description;
        let available = description
            .split("Available bindings: ")
            .nth(1)
            .unwrap_or_default();
        assert!(available.starts_with("Read"), "{description}");
        assert!(
            !available.contains("Write") && !available.contains("WebFetch"),
            "a program must only reach the worker's allow-list: {description}"
        );

        let plain = agent.build_sub_registry(&["Read".to_string()]);
        assert!(plain.find_by_name("RunCode").is_none());
    }
}
