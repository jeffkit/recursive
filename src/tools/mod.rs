//! Tool abstraction: any side effect the model can request.
//!
//! Tools are orthogonal to the agent and to each other. To add a capability
//! you implement `Tool` and register it; no other file changes.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};
use tracing::Instrument;

use crate::agent::PermissionDecision;
use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::permissions::auto_classifier::AutoClassifier;
use crate::permissions::SharedPermissions;
use crate::permissions::{DecisionReason, PermissionMode, PermissionsConfig};
use crate::tools::fs::ReadFileState;
use tokio::sync::RwLock;

// ── Goal-153: Tool side-effect classification + audit types ─────────────────

/// Classification of a tool's observable side-effects on state outside
/// the agent process. Used by orphan detection and safe-replay (g154) to
/// decide how aggressively to retry or skip an unfinished tool call.
///
/// Distinct from `crate::kernel::SideEffect`, which tracks background-job
/// scheduling; the two live in different modules and never collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSideEffect {
    /// No mutation of any state outside the agent process. Safe to
    /// replay at any time. Examples: `Read`, `Grep`,
    /// `recall`, `checkpoint_list`.
    ReadOnly,
    /// Modifies local state (filesystem, scratchpad) in an idempotent-
    /// friendly way. Examples: `Write`, `Edit`, `remember`.
    Mutating,
    /// Reaches out to the external world or triggers opaque side-effects.
    /// Cannot determine safe re-execution from local state alone.
    /// Examples: `Bash`, `Agent`, `schedule_wakeup`.
    /// **Default** for any tool that does not override `side_effect_class`.
    External,
}

/// Maximum length of the persisted error message in [`ExitStatus::Err`].
/// Anything longer is UTF-8 char-boundary clipped and `truncated` is set.
pub const AUDIT_ERR_MAX_BYTES: usize = 512;

#[inline]
fn is_false(b: &bool) -> bool {
    !b
}

/// Outcome of a single tool invocation, as recorded in [`AuditMeta`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExitStatus {
    Ok,
    Err {
        /// Error message, truncated to [`AUDIT_ERR_MAX_BYTES`] bytes.
        message: String,
        /// `true` when the original message was longer and was clipped.
        #[serde(default, skip_serializing_if = "is_false")]
        truncated: bool,
    },
}

/// Per-call audit record returned by [`ToolRegistry::invoke_with_audit`]
/// and stored in [`crate::session::TranscriptEntry::audit`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditMeta {
    /// UUIDv7 step identifier (time-ordered).
    pub step_id: String,
    /// Unix epoch millis at registry dispatch start.
    pub started_at: i64,
    /// Unix epoch millis when the tool returned.
    pub finished_at: i64,
    /// BLAKE3 of the canonical JSON of `arguments` (hex-encoded).
    /// Detects argument drift across resumes.
    pub args_hash: String,
    /// Side-effect class as reported by the tool at call time.
    pub side_effect: ToolSideEffect,
    /// Whether the tool returned `Ok` or `Err`.
    pub exit_status: ExitStatus,
}

impl AuditMeta {
    /// Synthetic `AuditMeta` for an unknown-tool dispatch (tool not in
    /// registry). Called when `invoke_with_audit` cannot find the tool.
    pub fn synthetic_unknown_tool(name: &str) -> Self {
        let now = unix_millis();
        Self {
            step_id: uuid::Uuid::now_v7().hyphenated().to_string(),
            started_at: now,
            finished_at: now,
            args_hash: String::new(),
            side_effect: ToolSideEffect::External,
            exit_status: ExitStatus::Err {
                message: format!("unknown tool: {name}"),
                truncated: false,
            },
        }
    }
}

/// Return value of [`ToolRegistry::invoke_with_audit`]: the tool result
/// and its accompanying audit record.
pub struct ToolDispatch {
    pub result: Result<String>,
    pub audit: AuditMeta,
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Clip `s` to at most `AUDIT_ERR_MAX_BYTES` bytes on a UTF-8 char boundary.
/// Returns `(clipped, was_truncated)`.
fn truncate_for_audit(s: &str) -> (String, bool) {
    if s.len() <= AUDIT_ERR_MAX_BYTES {
        return (s.to_string(), false);
    }
    let mut end = AUDIT_ERR_MAX_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

/// BLAKE3 hash of the canonical JSON encoding of `v`.
fn blake3_canonical_json(v: &Value) -> String {
    let canonical = v.to_string();
    let hash = blake3::hash(canonical.as_bytes());
    hash.to_hex().to_string()
}

pub mod a2a;
pub mod agent;
pub mod agent_defs;
pub mod checkpoint;
#[cfg(feature = "cloud-runtime")]
pub mod docker_provider;
#[cfg(feature = "cloud-runtime")]
pub mod docker_sandbox;
#[cfg(feature = "e2b-sandbox")]
pub mod e2b_provider;
pub mod edit;
pub mod episodic_recall;
pub mod estimate_tokens;
pub mod facts;
#[cfg(feature = "skill-hub")]
pub mod find_skills;
pub mod fs;
pub mod glob;
#[cfg(feature = "skill-hub")]
pub mod install_skill;
pub mod load_skill;
pub mod memory;
pub mod permission_pipeline;
pub mod plan_mode;
pub mod policy_sandbox;
pub mod run_background;
pub mod run_skill_script;
pub mod schedule_wakeup;
pub mod search;
pub mod send_message;
pub mod shell;

#[cfg(feature = "coordinator-mode")]
pub mod task_create;
#[cfg(feature = "coordinator-mode")]
pub mod task_get;
#[cfg(feature = "coordinator-mode")]
pub mod task_list;
#[cfg(feature = "coordinator-mode")]
pub mod task_output;
#[cfg(feature = "coordinator-mode")]
pub mod task_stop;
#[cfg(feature = "coordinator-mode")]
pub mod task_update;
#[cfg(feature = "coordinator-mode")]
pub mod team_create;
#[cfg(feature = "coordinator-mode")]
pub mod team_delete;
pub mod todo;
pub mod tool_search;
pub mod transport;
#[cfg(feature = "web_fetch")]
pub mod web_fetch;
#[cfg(feature = "web_search")]
pub mod web_search;

pub use a2a::{A2aCallTool, A2aCardTool, A2aTaskCheckTool};
pub use agent::{AgentTool, SharedMemoryRead, SharedMemoryWrite};
pub use agent_defs::{AgentDefinition, AgentDefinitions};
pub use checkpoint::{build_checkpoint_tools, CheckpointDiff, CheckpointList, CheckpointToolCtx};
pub use edit::EditTool;
pub use episodic_recall::{episodic_recall_summary, EpisodicRecall};
pub use estimate_tokens::EstimateTokens;
pub use facts::{
    facts_path, facts_summary, load_facts, search_facts, Fact, FactStore, ForgetFact, RecallFact,
    RememberFact, ScoredFact, UpdateFact,
};
#[cfg(feature = "skill-hub")]
pub use find_skills::FindSkills;
pub use fs::{ReadFile, WriteFile};
pub use glob::GlobTool;
#[cfg(feature = "skill-hub")]
pub use install_skill::InstallSkill;
pub use load_skill::LoadSkill;
pub use memory::{
    load_scratchpad, scratchpad_path, scratchpad_summary, Scratchpad, ScratchpadDelete,
    ScratchpadGet, ScratchpadList, WorkingMemoryTool,
};
pub use memory::{Forget, Recall, Remember};
pub use permission_pipeline::{CheckOutcome, PermissionPipeline};
pub use plan_mode::{
    EnterPlanModeTool, ExitPlanModeTool, PlanApprovalGate, PlanApprovalResult, PlanModeRequestGate,
    PlanModeRequestResult, RequestPlanModeTool,
};
pub use policy_sandbox::{FsPolicy, PolicyConfig, ShellPolicy};
pub use run_background::{BackgroundJobManager, CheckBackground, Job, JobState, RunBackground};
pub use run_skill_script::RunSkillScript;
pub use schedule_wakeup::{ScheduleWakeup, WakeupRequest, WakeupSlot};
pub use search::SearchFiles;
pub use send_message::{ListWorkersTool, SendMessageTool, WorkerMailbox, WorkerRegistry};
pub use shell::RunShell;

#[cfg(feature = "coordinator-mode")]
pub use task_create::TaskCreateTool;
#[cfg(feature = "coordinator-mode")]
pub use task_get::TaskGetTool;
#[cfg(feature = "coordinator-mode")]
pub use task_list::TaskListTool;
#[cfg(feature = "coordinator-mode")]
pub use task_output::TaskOutputTool;
#[cfg(feature = "coordinator-mode")]
pub use task_stop::TaskStopTool;
#[cfg(feature = "coordinator-mode")]
pub use task_update::TaskUpdateTool;
#[cfg(feature = "coordinator-mode")]
pub use team_create::TeamCreateTool;
#[cfg(feature = "coordinator-mode")]
pub use team_delete::TeamDeleteTool;
pub use todo::{TodoItem, TodoStatus, TodoWriteTool};
pub use tool_search::{DeferredCatalog, ToolSearchTool, TOOL_SEARCH_TOOL_NAME};
pub use transport::{DirEntry, ExecResult, LocalTransport, ReadResult, ToolTransport};
#[cfg(feature = "web_fetch")]
pub use web_fetch::WebFetch;
#[cfg(feature = "web_search")]
pub use web_search::WebSearch;

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::local()
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn execute(&self, arguments: Value) -> Result<String>;

    /// Classify this tool's observable side-effects. Default is the most
    /// conservative value (`External`) so any unannotated tool is treated
    /// as risky on resume. Override to `ReadOnly` or `Mutating` for
    /// built-in tools; MCP tools derive this from their annotations.
    fn side_effect_class(&self) -> ToolSideEffect {
        ToolSideEffect::External
    }

    /// Return `true` to send this tool as deferred (name-only) in the initial
    /// prompt; the model must call `ToolSearch` to load its full schema.
    /// Default is `false` (eager). Override in low-frequency tools.
    fn is_deferred(&self) -> bool {
        false
    }

    /// Convenience: a tool is read-only iff it classifies as `ReadOnly`.
    /// Used by the parallel-dispatch path in `agent.rs`. Override only if
    /// you have an unusual reason (you almost never should — override
    /// `side_effect_class` instead and let this default through).
    fn is_readonly(&self) -> bool {
        matches!(self.side_effect_class(), ToolSideEffect::ReadOnly)
    }

    /// Like `is_readonly` but can inspect the call-time arguments.
    ///
    /// Override this when read-only-ness depends on parameters (e.g. `sub_agent`
    /// with `subagent_type: "explore"` behaves as read-only while `"general_purpose"`
    /// is not). The default delegates to `is_readonly()`.
    fn is_readonly_for_args(&self, _arguments: &Value) -> bool {
        self.is_readonly()
    }
}

/// Goal-161: runtime permission hook. Implement this trait to intercept
/// every tool invocation before it runs.
///
/// - [`PermissionDecision::Allow`] — let the call proceed unchanged.
/// - [`PermissionDecision::Deny(reason)`] — block and return the reason as a tool error.
/// - [`PermissionDecision::Transform(args)`] — replace the arguments before execution.
///
/// When no hook is registered all tools are allowed.
#[async_trait]
pub trait PermissionHook: Send + Sync {
    /// Called before every tool dispatch.
    async fn check(&self, tool_name: &str, args: &serde_json::Value) -> PermissionDecision;
}

/// NOTE: Clone shares Arc state with all tools. Use fork() for isolation.
#[derive(Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    /// Alias → primary name mapping for `find_by_name`.
    /// Populated by `register`; never mutated by `invoke`.
    aliases: BTreeMap<String, String>,
    transport: Arc<dyn ToolTransport>,
    /// Goal-197: thread-safe shared permissions for runtime rule updates.
    /// When `Some`, `invoke_with_audit` reads through the lock at call time,
    /// so `add_session_rule` / `remove_session_rule` changes are immediately
    /// visible. When `None`, all tools are allowed (backward-compatible).
    permissions: Option<SharedPermissions>,
    /// Default permission mode for tools not covered by the config lists.
    /// Mirrors `PermissionsConfig.mode` for quick access without config lookup.
    permission_mode: PermissionMode,
    touched: Option<Arc<Mutex<TouchedFiles>>>,
    /// Partial-read guard: shared state written by `ReadFile` and checked by
    /// `EditTool`. `None` disables the guard (backward-compatible).
    read_file_state: Option<Arc<Mutex<ReadFileState>>>,
    /// Goal-161: optional runtime permission hook. When `Some`, called
    /// before every tool invocation. `None` means allow all (backward-
    /// compatible default).
    permission_hook: Option<Arc<dyn PermissionHook>>,
    /// Goal-184: optional L1 policy config. Stored here so individual tools
    /// can query it at call time. Does not enforce anything by itself;
    /// tools must call `registry.policy()` and check before executing.
    policy: Option<policy_sandbox::PolicyConfig>,
    /// Goal-199: headless mode — interactive tools go through external hooks
    /// instead of waiting for terminal input.
    pub headless: bool,
    /// Goal-199: external hook runner for headless permission checks.
    pub hook_runner: crate::hooks::ExternalHookRunner,

    /// Goal-200: optional auto classifier for `PermissionMode::Auto`.
    /// When `Some`, each tool call in Auto mode is classified by the
    /// LLM before execution. Wrapped in a `Mutex` (tokio) because `classify()`
    /// takes `&mut self` (it updates the denial tracker).
    pub auto_classifier: Option<Arc<tokio::sync::Mutex<AutoClassifier>>>,
}

/// Observer that records files touched by structured filesystem tools
/// during a single agent turn. Owned by `AgentRuntime` and reset at
/// every turn boundary; passed by `Arc<Mutex<...>>` to the
/// `ToolRegistry` so tool dispatch can record `path` arguments.
#[derive(Debug, Default, Clone)]
pub struct TouchedFiles {
    /// Workspace-relative file paths recorded from `Write`, `Edit`, etc.
    pub paths: HashSet<String>,
    /// True if the agent invoked `Bash` this turn — runtime will
    /// use a pre/post snapshot diff to attribute file changes.
    pub saw_shell: bool,
}

impl TouchedFiles {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && !self.saw_shell
    }
    pub fn paths_sorted(&self) -> Vec<String> {
        let mut v: Vec<_> = self.paths.iter().cloned().collect();
        v.sort();
        v
    }
}

/// Inspect tool arguments for known fs tools and record their paths
/// on the shared `TouchedFiles` collector.
fn record_touched(name: &str, args: &Value, slot: &Mutex<TouchedFiles>) {
    let Ok(mut t) = slot.lock() else {
        return;
    };
    match name {
        "Write" => {
            if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
                t.paths.insert(p.to_string());
            }
        }
        "Edit" => {
            // Edit (str_replace) stores the path in file_path.
            if let Some(p) = args.get("file_path").and_then(|v| v.as_str()) {
                t.paths.insert(p.to_string());
            }
        }
        "Bash" => {
            t.saw_shell = true;
        }
        _ => {}
    }
}

/// A `(ToolSpec, optional_search_hint)` pair returned by
/// [`ToolRegistry::split_eager_deferred`].
pub type SpecWithHint = (ToolSpec, Option<String>);

impl ToolRegistry {
    pub fn new(transport: Arc<dyn ToolTransport>) -> Self {
        Self {
            tools: BTreeMap::new(),
            aliases: BTreeMap::new(),
            transport,
            permissions: None,
            auto_classifier: None,
            permission_mode: PermissionMode::Default,
            touched: None,
            read_file_state: None,
            permission_hook: None,
            policy: None,
            headless: false,
            hook_runner: crate::hooks::ExternalHookRunner::discover(&[]),
        }
    }

    /// Create a registry with the default local transport.
    pub fn local() -> Self {
        Self::new(Arc::new(LocalTransport))
    }

    /// Returns a reference to the transport layer.
    pub fn transport(&self) -> &Arc<dyn ToolTransport> {
        &self.transport
    }

    /// Create a new empty registry that shares the same transport.
    pub fn with_same_transport(&self) -> Self {
        Self {
            tools: BTreeMap::new(),
            aliases: BTreeMap::new(),
            transport: self.transport.clone(),
            permissions: self.permissions.clone(),
            auto_classifier: self.auto_classifier.clone(),
            permission_mode: self.permission_mode.clone(),
            touched: self.touched.clone(),
            read_file_state: self.read_file_state.clone(),
            permission_hook: self.permission_hook.clone(),
            policy: self.policy.clone(),
            headless: self.headless,
            hook_runner: self.hook_runner.clone(),
        }
    }

    /// Create an isolated copy of this registry.
    ///
    /// Unlike `clone()`, `fork()` calls `tool.fork()` on each registered
    /// tool so that tools with internal state (e.g. scratchpad, memory)
    /// get independent copies rather than shared `Arc` references.
    ///
    /// Tools that do not implement `fork()` (stateless tools) are simply
    /// cloned as usual.
    ///
    /// For now, this is equivalent to `clone()` — a full fork requires
    /// per-tool fork support. This method exists as a named extension
    /// point so call sites can opt in to isolation semantics explicitly.
    pub fn fork(&self) -> Self {
        self.clone()
    }

    /// Attach a [`PermissionHook`] (Goal 161). When set, `ask_permission`
    /// is called before every tool invocation; returning `false` causes
    /// `invoke` to return `Error::PermissionDenied` without running the tool.
    pub fn with_permission_hook(mut self, hook: Arc<dyn PermissionHook>) -> Self {
        self.permission_hook = Some(hook);
        self
    }

    /// Attach a permission hook via mutable reference.
    /// Equivalent to [`with_permission_hook`] but usable on existing registries.
    pub fn set_permission_hook(&mut self, hook: Arc<dyn PermissionHook>) {
        self.permission_hook = Some(hook);
    }

    /// Remove any previously attached permission hook.
    pub fn clear_permission_hook(&mut self) {
        self.permission_hook = None;
    }

    /// Attach an L1 policy config. The registry stores the policy so that
    /// individual tools (e.g. `run_shell`) can query it via
    /// `registry.policy()` at call time.
    pub fn with_policy(mut self, policy: policy_sandbox::PolicyConfig) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Set the L1 policy config via mutable reference.
    pub fn set_policy(&mut self, policy: policy_sandbox::PolicyConfig) {
        self.policy = Some(policy);
    }

    /// Return the attached policy config, if any.
    pub fn policy(&self) -> Option<&policy_sandbox::PolicyConfig> {
        self.policy.as_ref()
    }

    /// Enable headless mode (Goal 199): interactive tools go through external
    /// hooks instead of waiting for terminal input.
    pub fn with_headless(mut self, headless: bool) -> Self {
        self.headless = headless;
        self
    }

    /// Set headless mode via mutable reference.
    pub fn set_headless(&mut self, headless: bool) {
        self.headless = headless;
    }

    /// Attach an [`ExternalHookRunner`] for headless permission checks.
    pub fn with_hook_runner(mut self, hook_runner: crate::hooks::ExternalHookRunner) -> Self {
        self.hook_runner = hook_runner;
        self
    }

    /// Set the external hook runner via mutable reference.
    pub fn set_hook_runner(&mut self, hook_runner: crate::hooks::ExternalHookRunner) {
        self.hook_runner = hook_runner;
    }

    /// Set the permissions configuration for this registry.
    pub fn with_permissions(mut self, permissions: PermissionsConfig) -> Self {
        self.permission_mode = permissions.mode.clone();
        self.permissions = Some(Arc::new(RwLock::new(permissions)));
        self
    }

    /// Attach a [`SharedPermissions`] reference for runtime rule updates.
    ///
    /// Unlike [`with_permissions`], this accepts an already-constructed
    /// `Arc<RwLock<LayeredPermissionsConfig>>` so that multiple components
    /// can share the same mutable config. Changes made via
    /// `add_session_rule` / `remove_session_rule` on the shared config
    /// are immediately visible through this registry.
    pub fn with_shared_permissions(mut self, sp: SharedPermissions) -> Self {
        // Snapshot the current mode for quick access.
        if let Ok(guard) = sp.try_read() {
            self.permission_mode = guard.mode.clone();
        }
        self.permissions = Some(sp);
        self
    }

    /// Attach an [`AutoClassifier`] for `PermissionMode::Auto`.
    ///
    /// When the registry's permission mode is [`Auto`](PermissionMode::Auto),
    /// each tool call is sent to the classifier before execution. The
    /// classifier is wrapped in `Arc<Mutex<...>>` so it can be shared
    /// across clones of the registry.
    pub fn with_auto_classifier(mut self, classifier: AutoClassifier) -> Self {
        self.auto_classifier = Some(Arc::new(tokio::sync::Mutex::new(classifier)));
        self
    }

    /// Return the current permission mode.
    pub fn permission_mode(&self) -> PermissionMode {
        self.permission_mode.clone()
    }

    /// Return a reference to the current permissions config, if any.
    /// Return a cloned snapshot of the current permissions config.
    ///
    /// Uses `try_read()` — returns `None` if the lock is held for writing
    /// (which is rare and brief). Callers that need a guaranteed read
    /// should use [`invoke_with_audit`] which does an async `.read().await`.
    pub fn permissions_config(&self) -> Option<PermissionsConfig> {
        self.permissions
            .as_ref()
            .and_then(|sp| sp.try_read().ok())
            .map(|guard| guard.clone())
    }

    /// Check whether a tool requires plan mode according to the current
    /// permissions configuration.
    pub fn is_plan_mode(&self, tool_name: &str) -> bool {
        self.permissions
            .as_ref()
            .and_then(|sp| sp.try_read().ok())
            .map(|guard| guard.is_plan_mode(tool_name))
            .unwrap_or(false)
    }

    /// Attach a [`TouchedFiles`] collector. Tool invocations on
    /// structured filesystem tools will record their path arguments
    /// onto the shared collector. Used by `AgentRuntime` to assemble
    /// per-turn checkpoint metadata.
    pub fn with_touched_files(mut self, slot: Arc<Mutex<TouchedFiles>>) -> Self {
        self.touched = Some(slot);
        self
    }

    /// Detach any previously attached collector.
    pub fn clear_touched_files(&mut self) {
        self.touched = None;
    }

    /// Return the currently attached touched-files collector, if any.
    pub fn touched_files(&self) -> Option<Arc<Mutex<TouchedFiles>>> {
        self.touched.clone()
    }

    /// Attach shared `ReadFileState` so `ReadFile` records reads and
    /// `EditTool` can enforce the partial-read guard.
    pub fn with_read_file_state(mut self, slot: Arc<Mutex<ReadFileState>>) -> Self {
        self.read_file_state = Some(slot);
        self
    }

    /// Return the currently attached read-file state, if any.
    pub fn read_file_state(&self) -> Option<Arc<Mutex<ReadFileState>>> {
        self.read_file_state.clone()
    }

    pub fn register(mut self, tool: Arc<dyn Tool>) -> Self {
        let name = tool.spec().name;
        self.tools.insert(name, tool);
        self
    }

    /// Register a tool and associate one or more aliases with it.
    ///
    /// Aliases are **not** sent to the LLM — they are only used by
    /// [`find_by_name`] so sandboxed replacements can be looked up under
    /// the original name the model knows.
    pub fn register_with_aliases(mut self, tool: Arc<dyn Tool>, aliases: &[&str]) -> Self {
        let name = tool.spec().name.clone();
        for &alias in aliases {
            self.aliases.insert(alias.to_string(), name.clone());
        }
        self.tools.insert(name, tool);
        self
    }

    /// Register a tool via mutable reference (for use with shared registries).
    pub fn register_mut(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.spec().name;
        self.tools.insert(name, tool);
    }

    /// Register a tool with aliases via mutable reference.
    pub fn register_mut_with_aliases(&mut self, tool: Arc<dyn Tool>, aliases: &[&str]) {
        let name = tool.spec().name.clone();
        for &alias in aliases {
            self.aliases.insert(alias.to_string(), name.clone());
        }
        self.tools.insert(name, tool);
    }

    /// Find a registered tool by its primary name or any alias.
    ///
    /// This is the preferred lookup path. `invoke` delegates to this so that
    /// sandboxed tool replacements can be reached under the original name.
    pub fn find_by_name(&self, name: &str) -> Option<Arc<dyn Tool>> {
        // Fast path: primary name.
        if let Some(tool) = self.tools.get(name) {
            return Some(tool.clone());
        }
        // Alias path.
        if let Some(primary) = self.aliases.get(name) {
            return self.tools.get(primary).cloned();
        }
        None
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec()).collect()
    }

    /// Return (eager_specs, deferred_specs).
    /// Eager tools are sent to the LLM with full schemas.
    /// Deferred tools are not — their names appear in
    /// `<available-deferred-tools>` so the model can call ToolSearchTool.
    pub fn specs_partitioned(&self) -> (Vec<ToolSpec>, Vec<ToolSpec>) {
        let mut eager = Vec::new();
        let mut deferred = Vec::new();
        for tool in self.tools.values() {
            if tool.is_deferred() {
                deferred.push(tool.spec());
            } else {
                eager.push(tool.spec());
            }
        }
        (eager, deferred)
    }

    /// Restrict the registry to only the named tools, removing all others.
    /// Tool names are matched case-insensitively. Aliases for removed tools
    /// are also dropped. Used by `--allow-tools` to give agents a limited
    /// tool set (e.g. read-only review agents).
    pub fn retain_tools(&mut self, allow: &[String]) {
        let allowed: std::collections::HashSet<String> =
            allow.iter().map(|n| n.to_lowercase()).collect();
        self.tools
            .retain(|name, _| allowed.contains(&name.to_lowercase()));
        self.aliases
            .retain(|_, primary| self.tools.contains_key(primary));
    }

    /// Split the registry's tools into eager and deferred partitions.
    ///
    /// Returns `(eager, deferred)` where each element is a
    /// `(ToolSpec, optional_search_hint)` pair. Eager tools carry their
    /// full schema; deferred tools carry only the name (the full schema is
    /// returned on demand when the model calls `ToolSearch`). The
    /// search hint is the first sentence of the tool's description,
    /// suitable for injection into the deferred tool list so the model
    /// knows what is available without the full schema.
    pub fn split_eager_deferred(&self) -> (Vec<SpecWithHint>, Vec<SpecWithHint>) {
        let mut eager = Vec::new();
        let mut deferred = Vec::new();
        for tool in self.tools.values() {
            let spec = tool.spec();
            let hint = spec
                .description
                .split('.')
                .next()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            if tool.is_deferred() {
                deferred.push((spec, hint));
            } else {
                eager.push((spec, hint));
            }
        }
        (eager, deferred)
    }

    /// Check whether a spec is deferred by looking up the tool in the registry.
    pub fn is_deferred_spec(&self, spec: &ToolSpec) -> bool {
        self.tools
            .get(&spec.name)
            .map(|t| t.is_deferred())
            .unwrap_or(false)
    }

    /// Finalize deferred tool support: collect all deferred tool specs into a
    /// shared catalog and register a `ToolSearchTool` backed by that catalog.
    ///
    /// Call this once after all other tools have been registered. If there are
    /// no deferred tools, this is a no-op (ToolSearchTool is not registered).
    /// Finalise deferred tool support: collect all deferred tool specs into a
    /// shared catalog and register a `ToolSearchTool` backed by that catalog.
    ///
    /// `native` controls the response format:
    /// - `true`  → name-array output (official Anthropic API, expands via
    ///   `tool_reference` blocks server-side).
    /// - `false` → full JSON schema output (Anthropic-compatible endpoints
    ///   like DeepSeek / MiniMax that don't support `tool_reference`).
    pub fn freeze_deferred_specs(&mut self, native: bool) {
        let deferred_specs: Vec<ToolSpec> = self
            .tools
            .values()
            .filter(|t| t.is_deferred())
            .map(|t| t.spec())
            .collect();

        if deferred_specs.is_empty() {
            return;
        }

        let catalog: DeferredCatalog = Arc::new(std::sync::RwLock::new(deferred_specs));
        let tool = Arc::new(ToolSearchTool::new(catalog).with_native(native));
        self.tools.insert(TOOL_SEARCH_TOOL_NAME.to_string(), tool);
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// Check if a tool is read-only (no side effects).
    pub fn is_readonly(&self, name: &str) -> bool {
        self.tools
            .get(name)
            .map(|t| t.is_readonly())
            .unwrap_or(false)
    }

    /// Like `is_readonly` but passes call-time arguments to the tool so it can
    /// make an argument-specific decision (e.g. `sub_agent` checking
    /// `subagent_type: "explore"`).
    pub fn is_readonly_for_call(&self, name: &str, args: &Value) -> bool {
        self.tools
            .get(name)
            .map(|t| t.is_readonly_for_args(args))
            .unwrap_or(false)
    }

    pub async fn invoke(&self, name: &str, arguments: Value) -> Result<String> {
        // Goal-161: runtime permission hook — checked first, before static
        // config, so the user gets the chance to allow/deny at call time.
        let effective_args = if let Some(hook) = &self.permission_hook {
            match hook.check(name, &arguments).await {
                PermissionDecision::Allow => arguments,
                PermissionDecision::Transform(new_args) => new_args,
                PermissionDecision::Deny(reason) => {
                    return Err(Error::PermissionDenied {
                        name: name.into(),
                        reason: DecisionReason::Hook { name: reason },
                    });
                }
            }
        } else {
            arguments
        };
        self.invoke_with_audit(name, effective_args).await.result
    }

    /// Invoke a tool and return both its result and a populated
    /// [`AuditMeta`]. Callers that need to persist audit data should
    /// use this method; callers that don't can call `invoke` which
    /// discards the audit half.
    pub async fn invoke_with_audit(&self, name: &str, arguments: Value) -> ToolDispatch {
        // Goal-261: pre-execution permission checks are delegated to
        // `PermissionPipeline`. This method keeps only `touched_files`
        // recording, tool lookup, L1 policy check, side-effect
        // classification, step_id/hash generation, and tool execution
        // + audit construction. The pipeline owns the 7 permission-
        // orchestration phases and exposes a public `recheck_policy`
        // method for callers that need to re-validate hook-mutated args.
        let pipeline = permission_pipeline::PermissionPipeline::new(self);
        match pipeline.check(name, arguments).await {
            permission_pipeline::CheckOutcome::Deny { error, audit } => ToolDispatch {
                result: Err(error),
                audit,
            },
            permission_pipeline::CheckOutcome::Allow { arguments } => {
                self.dispatch_after_permission_check(name, arguments).await
            }
        }
    }

    /// Goal-261: the execution half of `invoke_with_audit` — invoked
    /// after `PermissionPipeline::check` has returned `Allow`. Records
    /// touched files, looks up the tool, runs the L1 policy check, and
    /// constructs the `AuditMeta` around `tool.execute()`.
    async fn dispatch_after_permission_check(&self, name: &str, arguments: Value) -> ToolDispatch {
        // Record touched files for the active turn (if a collector is attached).
        if let Some(slot) = &self.touched {
            record_touched(name, &arguments, slot);
        }

        let Some(tool) = self.find_by_name(name) else {
            return ToolDispatch {
                result: Err(Error::UnknownTool(name.into())),
                audit: AuditMeta::synthetic_unknown_tool(name),
            };
        };

        let side_effect = tool.side_effect_class();
        let step_id = uuid::Uuid::now_v7().hyphenated().to_string();
        let args_hash = blake3_canonical_json(&arguments);
        let started_at = unix_millis();

        let args_size = arguments.to_string().len();
        let span = tracing::info_span!("tool.execute", name = %name, args_size);
        let raw_result = tool
            .execute(arguments)
            .instrument(span)
            .await
            .map_err(|e| match e {
                Error::Tool { .. } | Error::BadToolArgs { .. } | Error::UnknownTool(_) => e,
                other => Error::Tool {
                    name: name.into(),
                    message: other.to_string(),
                },
            });

        let finished_at = unix_millis();
        let exit_status = match &raw_result {
            Ok(_) => ExitStatus::Ok,
            Err(e) => {
                let (clipped, truncated) = truncate_for_audit(&e.to_string());
                ExitStatus::Err {
                    message: clipped,
                    truncated,
                }
            }
        };

        ToolDispatch {
            result: raw_result,
            audit: AuditMeta {
                step_id,
                started_at,
                finished_at,
                args_hash,
                side_effect,
                exit_status,
            },
        }
    }
}

/// Build a short human-readable preview of tool arguments for the
/// permission dialog. Extracts up to 80 characters.
pub fn args_preview_for_permission(arguments: &Value) -> String {
    let s = match arguments {
        Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .take(3)
                .map(|(k, v)| {
                    let v_str = match v {
                        Value::String(s) => {
                            let short: String = s.chars().take(30).collect();
                            format!("\"{}\"", short)
                        }
                        other => {
                            let s = other.to_string();
                            s.chars().take(30).collect()
                        }
                    };
                    format!("{k}={v_str}")
                })
                .collect();
            parts.join(", ")
        }
        other => other.to_string(),
    };
    if s.chars().count() > 80 {
        let head: String = s.chars().take(79).collect();
        format!("{head}…")
    } else {
        s
    }
}

/// Resolve a possibly-relative path against the workspace root.
///
/// Normalises both the root and candidate to absolute, dot-free form first,
/// then performs a second check via `canonicalize()` (which follows symlinks)
/// when the path already exists on disk.  This prevents symlink-based escapes
/// where a link inside the workspace points to a location outside it.
///
/// For paths that do not yet exist (e.g. a new file being written), only the
/// lexical normalisation check is performed — the caller is responsible for
/// ensuring no symlink is created that would bridge outside the root.
pub(crate) fn resolve_within(root: &std::path::Path, path: &str) -> Result<std::path::PathBuf> {
    let candidate = std::path::Path::new(path);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    let abs_root = absolutise(root);
    let abs_joined = absolutise(&joined);
    // Lexical check (works for paths that don't exist yet).
    if !abs_joined.starts_with(&abs_root) {
        return Err(Error::BadToolArgs {
            name: "<fs>".into(),
            message: format!(
                "path `{}` escapes workspace root `{}`",
                path,
                abs_root.display()
            ),
        });
    }
    // Symlink-aware check: if the path exists, canonicalize both sides and
    // re-check so that symlinks pointing outside the workspace are rejected.
    if abs_joined.exists() {
        let canonical_root = abs_root.canonicalize().map_err(|e| Error::BadToolArgs {
            name: "<fs>".into(),
            message: format!(
                "cannot canonicalize workspace root `{}`: {}",
                abs_root.display(),
                e
            ),
        })?;
        match abs_joined.canonicalize() {
            Ok(canonical_joined) => {
                if !canonical_joined.starts_with(&canonical_root) {
                    return Err(Error::BadToolArgs {
                        name: "<fs>".into(),
                        message: format!(
                            "path `{}` resolves via symlink to a location outside the workspace root `{}`",
                            path,
                            canonical_root.display()
                        ),
                    });
                }
            }
            Err(e) => {
                return Err(Error::BadToolArgs {
                    name: "<fs>".into(),
                    message: format!("cannot resolve path `{}`: {e}", path),
                });
            }
        }
    }
    Ok(abs_joined)
}

/// Turn a path into an absolute, normalised form. Does not touch the disk,
/// so it works for files that don't yet exist (needed by `write_file`).
fn absolutise(p: &std::path::Path) -> std::path::PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join(p)
    };
    normalise(&abs)
}

fn normalise(p: &std::path::Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for c in p.components() {
        use std::path::Component::*;
        match c {
            ParentDir => {
                out.pop();
            }
            CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Build the standard tool registry for an agent rooted at `workspace`.
///
/// This is the canonical tool set shared by all entry points (CLI, TUI, HTTP
/// server, etc.). Entry points may register additional tools on top of this
/// baseline (e.g. `ScheduleWakeup` for loop mode, `SubAgent` when enabled).
///
/// Skills are opt-in: pass a non-empty `skills` slice to register
/// `load_skill` and `run_skill_script`. Pass `&[]` to skip.
pub fn build_standard_tools(
    workspace: &std::path::Path,
    skills: &[crate::skills::Skill],
    shell_timeout_secs: u64,
) -> ToolRegistry {
    let bg_manager = Arc::new(tokio::sync::Mutex::new(BackgroundJobManager::new()));
    let todo_list = Arc::new(std::sync::RwLock::new(Vec::<TodoItem>::new()));
    let read_state = Arc::new(Mutex::new(ReadFileState::new()));
    let mut registry = ToolRegistry::local()
        .with_read_file_state(read_state.clone())
        .register(Arc::new(
            ReadFile::new(workspace).with_read_state(read_state.clone()),
        ))
        .register(Arc::new(WriteFile::new(workspace)))
        .register(Arc::new(
            EditTool::new(workspace).with_read_state(read_state.clone()),
        ))
        .register(Arc::new(
            RunShell::new(workspace)
                .with_timeout(std::time::Duration::from_secs(shell_timeout_secs)),
        ))
        .register(Arc::new(SearchFiles::new(workspace)))
        .register(Arc::new(GlobTool::new(workspace)))
        .register(Arc::new(RunBackground::new(workspace, bg_manager.clone())))
        .register(Arc::new(CheckBackground::new(bg_manager)))
        .register(Arc::new(EstimateTokens::new(workspace)))
        .register(Arc::new(Remember::new(workspace)))
        .register(Arc::new(Recall::new(workspace)))
        .register(Arc::new(Forget::new(workspace)))
        .register(Arc::new(RememberFact::new(workspace)))
        .register(Arc::new(RecallFact::new(workspace)))
        .register(Arc::new(ForgetFact::new(workspace)))
        .register(Arc::new(UpdateFact::new(workspace)))
        .register(Arc::new(EpisodicRecall::new(workspace)))
        .register(Arc::new(WorkingMemoryTool::new(workspace)))
        .register(Arc::new(ScratchpadGet::new(workspace)))
        .register(Arc::new(ScratchpadDelete::new(workspace)))
        .register(Arc::new(ScratchpadList::new(workspace)))
        .register(Arc::new(TodoWriteTool::new(
            todo_list,
            Arc::new(crate::event::NullSink),
        )))
        .register(Arc::new(A2aCallTool::new()))
        .register(Arc::new(A2aCardTool::new()))
        .register(Arc::new(A2aTaskCheckTool::new()));

    // Goal-201: plan mode tools are channel capabilities (TUI / HTTP only).
    // They are registered exclusively by AgentRuntimeBuilder::build() which
    // wires them to the real PlanApprovalGate and EventSink.  Headless /
    // CLI / self-improve runs that call build_standard_tools() directly
    // will not have these tools, preventing the LLM from blocking on an
    // interactive review that can never complete.

    #[cfg(feature = "web_fetch")]
    {
        registry = registry.register(Arc::new(WebFetch::new()));
    }

    #[cfg(feature = "web_search")]
    {
        registry = registry.register(Arc::new(WebSearch::new()));
    }

    if !skills.is_empty() {
        registry = registry
            .register(Arc::new(LoadSkill::new(skills.to_vec())))
            .register(Arc::new(RunSkillScript::new(
                skills.to_vec(),
                workspace.to_path_buf(),
                std::time::Duration::from_secs(shell_timeout_secs),
            )));
    }

    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::PermissionMode;
    use async_trait::async_trait;

    struct Echo;

    #[async_trait]
    impl Tool for Echo {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "echo".into(),
                description: "echo".into(),
                parameters: serde_json::json!({"type":"object","properties":{"msg":{"type":"string"}}}),
            }
        }
        async fn execute(&self, args: Value) -> Result<String> {
            Ok(args["msg"].as_str().unwrap_or("").into())
        }
    }

    #[tokio::test]
    async fn registry_dispatches_and_errors_on_unknown() {
        let reg = ToolRegistry::local().register(Arc::new(Echo));
        let out = reg
            .invoke("echo", serde_json::json!({"msg":"hi"}))
            .await
            .unwrap();
        assert_eq!(out, "hi");
        let err = reg.invoke("nope", serde_json::json!({})).await.unwrap_err();
        assert!(matches!(err, Error::UnknownTool(_)));
    }

    #[test]
    fn resolve_within_rejects_escape() {
        let root = std::path::Path::new("/work");
        assert!(resolve_within(root, "../etc/passwd").is_err());
        assert!(resolve_within(root, "/elsewhere").is_err());
        assert!(resolve_within(root, "src/lib.rs").is_ok());
    }

    #[test]
    fn resolve_within_handles_relative_root() {
        // Regression: `--workspace .` (relative) used to fail the prefix check.
        let cwd = std::env::current_dir().unwrap();
        let resolved = resolve_within(std::path::Path::new("."), "src/lib.rs").unwrap();
        assert!(resolved.starts_with(&cwd));
        assert!(resolved.ends_with("src/lib.rs"));
    }

    #[tokio::test]
    async fn test_permission_deny_blocks_invoke() {
        let config = crate::permissions::LayeredPermissionsConfig {
            mode: PermissionMode::Default,
            layers: vec![crate::permissions::PermissionLayer {
                source: crate::permissions::RuleSource::User,
                allow: vec!["echo".into()],
                deny: vec!["echo".into()],
                ..Default::default()
            }],
        };
        let reg = ToolRegistry::local()
            .with_permissions(config)
            .register(Arc::new(Echo));
        let err = reg
            .invoke("echo", serde_json::json!({"msg":"hi"}))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::PermissionDenied { .. }));
    }

    // ── Goal-161: PermissionHook tests ───────────────────────────────────

    struct AllowHook;
    struct DenyHook;

    #[async_trait]
    impl PermissionHook for AllowHook {
        async fn check(&self, _name: &str, _args: &serde_json::Value) -> PermissionDecision {
            PermissionDecision::Allow
        }
    }

    #[async_trait]
    impl PermissionHook for DenyHook {
        async fn check(&self, _name: &str, _args: &serde_json::Value) -> PermissionDecision {
            PermissionDecision::Deny("denied by test hook".to_string())
        }
    }

    #[tokio::test]
    async fn permission_hook_allow_lets_tool_run() {
        let reg = ToolRegistry::local()
            .with_permission_hook(Arc::new(AllowHook))
            .register(Arc::new(Echo));
        let result = reg
            .invoke("echo", serde_json::json!({"msg": "hello"}))
            .await
            .unwrap();
        assert_eq!(result, "hello");
    }

    #[tokio::test]
    async fn permission_hook_deny_blocks_invoke() {
        let reg = ToolRegistry::local()
            .with_permission_hook(Arc::new(DenyHook))
            .register(Arc::new(Echo));
        let err = reg
            .invoke("echo", serde_json::json!({"msg": "blocked"}))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::PermissionDenied { .. }));
    }

    #[test]
    fn args_preview_truncates_long_strings() {
        let big_val = "x".repeat(200);
        let args = serde_json::json!({"command": big_val});
        let preview = args_preview_for_permission(&args);
        assert!(preview.chars().count() <= 81); // 80 chars + ellipsis
    }

    // ── Goal-199: Headless mode tests ────────────────────────────────────

    /// headless=true, no hooks, interactive tool → PermissionDenied
    #[tokio::test]
    async fn headless_interactive_tool_denied_without_hooks() {
        let config = crate::permissions::LayeredPermissionsConfig {
            mode: PermissionMode::Default,
            layers: vec![crate::permissions::PermissionLayer {
                source: crate::permissions::RuleSource::User,
                interactive: vec!["echo".into()],
                ..Default::default()
            }],
        };
        let reg = ToolRegistry::local()
            .with_permissions(config)
            .with_headless(true)
            .register(Arc::new(Echo));
        let err = reg
            .invoke("echo", serde_json::json!({"msg": "hi"}))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::PermissionDenied { .. }));
    }

    /// headless=true, mock hook returns Continue → interactive tool allowed
    #[tokio::test]
    async fn headless_interactive_tool_allowed_by_hook() {
        use tempfile::tempdir;
        let tmp = tempdir().unwrap();
        let hook_path = tmp.path().join("allow.sh");
        let script = "#!/bin/sh\nread -r _\necho '{\"action\":\"continue\"}'\n";
        std::fs::write(&hook_path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&hook_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&hook_path, perms).unwrap();
        }
        let hook_runner = crate::hooks::ExternalHookRunner::discover(&[tmp.path().to_path_buf()]);

        let config = crate::permissions::LayeredPermissionsConfig {
            mode: PermissionMode::Default,
            layers: vec![crate::permissions::PermissionLayer {
                source: crate::permissions::RuleSource::User,
                interactive: vec!["echo".into()],
                ..Default::default()
            }],
        };
        let reg = ToolRegistry::local()
            .with_permissions(config)
            .with_headless(true)
            .with_hook_runner(hook_runner)
            .register(Arc::new(Echo));

        #[cfg(unix)]
        {
            let result = reg
                .invoke("echo", serde_json::json!({"msg": "allowed"}))
                .await
                .unwrap();
            assert_eq!(result, "allowed");
        }
        #[cfg(not(unix))]
        {
            let err = reg
                .invoke("echo", serde_json::json!({"msg": "blocked"}))
                .await
                .unwrap_err();
            assert!(matches!(err, Error::PermissionDenied { .. }));
        }
    }

    /// headless=false → interactive tools go through normal path (Passthrough)
    #[tokio::test]
    async fn non_headless_interactive_not_auto_denied() {
        let config = crate::permissions::LayeredPermissionsConfig {
            mode: PermissionMode::Default,
            layers: vec![crate::permissions::PermissionLayer {
                source: crate::permissions::RuleSource::User,
                interactive: vec!["echo".into()],
                ..Default::default()
            }],
        };
        let reg = ToolRegistry::local()
            .with_permissions(config)
            .with_headless(false)
            .register(Arc::new(Echo));
        let result = reg
            .invoke("echo", serde_json::json!({"msg": "hello"}))
            .await
            .unwrap();
        assert_eq!(result, "hello");
    }

    // ── Goal-212: Permission::Unknown semantics ──────────────────────────────

    /// Smoke test: Permission::Unknown variant compiles and can be constructed.
    #[test]
    fn permission_unknown_variant_exists() {
        use crate::permissions::Permission;
        let u = Permission::Unknown;
        assert!(!u.is_allowed());
        assert!(!u.is_denied());
    }

    /// Unknown + non-headless + interactive + DenyHook → PermissionDenied
    /// (invoke_with_audit called directly, bypassing invoke()).
    #[tokio::test]
    async fn unknown_interactive_tool_deny_hook_blocks_invoke_with_audit() {
        let config = crate::permissions::LayeredPermissionsConfig {
            mode: PermissionMode::Default,
            layers: vec![crate::permissions::PermissionLayer {
                source: crate::permissions::RuleSource::User,
                interactive: vec!["echo".into()],
                ..Default::default()
            }],
        };
        let reg = ToolRegistry::local()
            .with_permissions(config)
            .with_permission_hook(Arc::new(DenyHook))
            .with_headless(false)
            .register(Arc::new(Echo));
        let dispatch = reg
            .invoke_with_audit("echo", serde_json::json!({"msg": "hi"}))
            .await;
        assert!(
            matches!(dispatch.result, Err(Error::PermissionDenied { .. })),
            "hook-deny should block interactive Unknown tool via invoke_with_audit"
        );
    }

    /// Unknown + non-headless + interactive + no hook → allowed (library default).
    #[tokio::test]
    async fn unknown_interactive_tool_no_hook_is_allowed() {
        let config = crate::permissions::LayeredPermissionsConfig {
            mode: PermissionMode::Default,
            layers: vec![crate::permissions::PermissionLayer {
                source: crate::permissions::RuleSource::User,
                interactive: vec!["echo".into()],
                ..Default::default()
            }],
        };
        let reg = ToolRegistry::local()
            .with_permissions(config)
            .with_headless(false)
            .register(Arc::new(Echo));
        let dispatch = reg
            .invoke_with_audit("echo", serde_json::json!({"msg": "ok"}))
            .await;
        assert_eq!(
            dispatch.result.unwrap(),
            "ok",
            "no hook = allow for Unknown interactive tool"
        );
    }

    /// Unknown + non-headless + non-interactive + DenyHook → allowed (hook not consulted).
    #[tokio::test]
    async fn unknown_non_interactive_tool_hook_not_consulted() {
        // echo is NOT in the interactive list; DenyHook should not fire.
        let config = crate::permissions::LayeredPermissionsConfig {
            mode: PermissionMode::Default,
            layers: vec![],
        };
        let reg = ToolRegistry::local()
            .with_permissions(config)
            .with_permission_hook(Arc::new(DenyHook))
            .with_headless(false)
            .register(Arc::new(Echo));
        let dispatch = reg
            .invoke_with_audit("echo", serde_json::json!({"msg": "pass"}))
            .await;
        assert_eq!(
            dispatch.result.unwrap(),
            "pass",
            "DenyHook must not fire for non-interactive Unknown tools"
        );
    }

    /// Allowed (explicit allow rule) + interactive + DenyHook
    /// → allowed (no hook fired because perm_is_unknown=false).
    #[tokio::test]
    async fn allowed_interactive_tool_hook_not_consulted() {
        let config = crate::permissions::LayeredPermissionsConfig {
            mode: PermissionMode::Default,
            layers: vec![crate::permissions::PermissionLayer {
                source: crate::permissions::RuleSource::User,
                allow: vec!["echo".into()],
                interactive: vec!["echo".into()],
                ..Default::default()
            }],
        };
        let reg = ToolRegistry::local()
            .with_permissions(config)
            .with_permission_hook(Arc::new(DenyHook))
            .with_headless(false)
            .register(Arc::new(Echo));
        // invoke_with_audit directly: check_static returns Allowed, not Unknown,
        // so the Goal-212 hook block must NOT fire.
        let dispatch = reg
            .invoke_with_audit("echo", serde_json::json!({"msg": "explicit-allow"}))
            .await;
        assert_eq!(
            dispatch.result.unwrap(),
            "explicit-allow",
            "Explicitly Allowed tools must not be re-checked via hook"
        );
    }

    // ── Goal-201: plan mode tools are opt-in (not in default registry) ──────

    #[test]
    fn default_registry_has_no_plan_mode_tools() {
        // build_standard_tools() must NOT register enter_plan_mode / exit_plan_mode.
        // These are channel capabilities owned exclusively by AgentRuntimeBuilder.
        let workspace = std::path::PathBuf::from(".");
        let registry = build_standard_tools(&workspace, &[], 30);
        assert!(
            registry.get("enter_plan_mode").is_none(),
            "enter_plan_mode must not be in the default registry"
        );
        assert!(
            registry.get("exit_plan_mode").is_none(),
            "exit_plan_mode must not be in the default registry"
        );
    }

    // ── Goal-247: ToolRegistry::fork() ──────────────────────────────────────

    #[test]
    fn fork_returns_usable_registry() {
        let reg = ToolRegistry::local().register(Arc::new(Echo));
        let forked = reg.fork();
        // fork() should return a registry that can invoke tools
        assert!(
            forked.get("echo").is_some(),
            "forked registry should contain 'echo'"
        );
        assert!(
            forked.get("nope").is_none(),
            "forked registry should not contain unknown tools"
        );
    }
}
