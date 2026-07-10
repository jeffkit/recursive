//! Tool registry: the [`Tool`] trait, [`ToolRegistry`] collection, and
//! [`build_standard_tools`] factory.
//!
//! Tools are orthogonal to the agent and to each other. To add a capability
//! you implement [`Tool`] and register it; no other file changes.

use async_trait::async_trait;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio::sync::RwLock;

use crate::agent::PermissionDecision;
use crate::error::Result;
use crate::llm::ToolSpec;
use crate::permissions::auto_classifier::AutoClassifier;
use crate::permissions::SharedPermissions;
use crate::permissions::{PermissionMode, PermissionsConfig};
use crate::tools::fs::ReadFileState;

use super::audit::TouchedFiles;
use super::policy_sandbox;

/// A `(ToolSpec, optional_search_hint)` pair returned by
/// [`ToolRegistry::split_eager_deferred`].
pub type SpecWithHint = (ToolSpec, Option<String>);

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn execute(&self, arguments: Value) -> Result<String>;

    /// Classify this tool's observable side-effects. Default is the most
    /// conservative value (`External`) so any unannotated tool is treated
    /// as risky on resume. Override to `ReadOnly` or `Mutating` for
    /// built-in tools; MCP tools derive this from their annotations.
    fn side_effect_class(&self) -> super::audit::ToolSideEffect {
        super::audit::ToolSideEffect::External
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
        matches!(
            self.side_effect_class(),
            super::audit::ToolSideEffect::ReadOnly
        )
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
    transport: Arc<dyn super::transport::ToolTransport>,
    /// Goal-197: thread-safe shared permissions for runtime rule updates.
    /// When `Some`, `invoke_with_audit` reads through the lock at call time,
    /// so `add_session_rule` / `remove_session_rule` changes are immediately
    /// visible. When `None`, all tools are allowed (backward-compatible).
    pub(crate) permissions: Option<SharedPermissions>,
    /// Default permission mode for tools not covered by the config lists.
    /// Mirrors `PermissionsConfig.mode` for quick access without config lookup.
    pub(crate) permission_mode: PermissionMode,
    pub(crate) touched: Option<Arc<Mutex<TouchedFiles>>>,
    /// Partial-read guard: shared state written by `ReadFile` and checked by
    /// `EditTool`. `None` disables the guard (backward-compatible).
    read_file_state: Option<Arc<Mutex<ReadFileState>>>,
    /// Runtime-mutable extra sandbox roots (Claude `register_repo_root`).
    /// Tools hold clones of the same Arc; this field lets control code
    /// recover the shared slot after the registry is built.
    session_roots: Option<super::dispatch::SharedSandboxRoots>,
    /// Shared MCP elicitation handler slot (Claude control `elicitation`).
    #[cfg(feature = "mcp")]
    elicitation: Option<crate::mcp::SharedElicitationHandler>,
    /// Goal-161: optional runtime permission hook. When `Some`, called
    /// before every tool invocation. `None` means allow all (backward-
    /// compatible default).
    pub(crate) permission_hook: Option<Arc<dyn PermissionHook>>,
    /// Goal-184: optional L1 policy config. Stored here so individual tools
    /// can query it at call time. Does not enforce anything by itself;
    /// tools must call `registry.policy()` and check before executing.
    pub(crate) policy: Option<policy_sandbox::PolicyConfig>,
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

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::local()
    }
}

impl ToolRegistry {
    pub fn new(transport: Arc<dyn super::transport::ToolTransport>) -> Self {
        Self {
            tools: BTreeMap::new(),
            aliases: BTreeMap::new(),
            transport,
            permissions: None,
            auto_classifier: None,
            permission_mode: PermissionMode::Default,
            touched: None,
            read_file_state: None,
            session_roots: None,
            #[cfg(feature = "mcp")]
            elicitation: None,
            permission_hook: None,
            policy: None,
            headless: false,
            hook_runner: crate::hooks::ExternalHookRunner::discover(&[]),
        }
    }

    /// Create a registry with the default local transport.
    pub fn local() -> Self {
        Self::new(Arc::new(super::transport::LocalTransport))
    }

    /// Returns a reference to the transport layer.
    pub fn transport(&self) -> &Arc<dyn super::transport::ToolTransport> {
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
            session_roots: self.session_roots.clone(),
            #[cfg(feature = "mcp")]
            elicitation: self.elicitation.clone(),
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

    /// Update the permission mode at runtime (Claude `set_permission_mode`).
    ///
    /// Writes through to the shared [`LayeredPermissionsConfig`] so
    /// `invoke_with_audit` sees the new mode. If no shared config is attached
    /// yet, one is created with the given mode and empty rule layers.
    pub fn set_permission_mode(&mut self, mode: PermissionMode) {
        self.permission_mode = mode.clone();
        match &self.permissions {
            Some(sp) => {
                if let Ok(mut guard) = sp.try_write() {
                    guard.mode = mode;
                }
            }
            None => {
                self.permissions = Some(Arc::new(RwLock::new(
                    crate::permissions::LayeredPermissionsConfig {
                        mode,
                        layers: Vec::new(),
                    },
                )));
            }
        }
    }

    /// Shared permissions handle for mid-run control updates, if any.
    pub fn shared_permissions(&self) -> Option<SharedPermissions> {
        self.permissions.clone()
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

    /// Attach a shared sandbox-roots slot for mid-run expansion.
    pub fn with_session_roots(mut self, roots: super::dispatch::SharedSandboxRoots) -> Self {
        self.session_roots = Some(roots);
        self
    }

    /// Return the shared sandbox-roots slot, if any.
    pub fn session_roots(&self) -> Option<super::dispatch::SharedSandboxRoots> {
        self.session_roots.clone()
    }

    /// Attach a shared MCP elicitation-handler slot.
    #[cfg(feature = "mcp")]
    pub fn with_elicitation_slot(mut self, slot: crate::mcp::SharedElicitationHandler) -> Self {
        self.elicitation = Some(slot);
        self
    }

    /// Return the shared elicitation-handler slot, if any.
    #[cfg(feature = "mcp")]
    pub fn elicitation_slot(&self) -> Option<crate::mcp::SharedElicitationHandler> {
        self.elicitation.clone()
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
    pub fn freeze_deferred_specs(&mut self) {
        let deferred_specs: Vec<ToolSpec> = self
            .tools
            .values()
            .filter(|t| t.is_deferred())
            .map(|t| t.spec())
            .collect();

        if deferred_specs.is_empty() {
            return;
        }

        let catalog: super::tool_search::DeferredCatalog =
            Arc::new(std::sync::RwLock::new(deferred_specs));
        let tool = Arc::new(super::tool_search::ToolSearchTool::new(catalog));
        self.tools
            .insert(super::tool_search::TOOL_SEARCH_TOOL_NAME.to_string(), tool);
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
}

/// Build the standard tool registry for an agent rooted at `workspace`.
///
/// This is the canonical tool set shared by all entry points (CLI, TUI, HTTP
/// server, etc.). Entry points may register additional tools on top of this
/// baseline (e.g. `ScheduleWakeup` for loop mode, `SubAgent` when enabled).
///
/// Skills are opt-in: pass a non-empty `skills` slice to register
/// `load_skill`. Pass `&[]` to skip.
pub fn build_standard_tools(
    workspace: &std::path::Path,
    skills: &[crate::skills::Skill],
    shell_timeout_secs: u64,
) -> ToolRegistry {
    build_standard_tools_with_roots(
        workspace,
        &[],
        None,
        skills,
        shell_timeout_secs,
        None,
        None,
        None,
        None,
    )
}

/// Same as [`build_standard_tools`] but accepts additional sandbox roots
/// beyond the primary workspace. Each `(root, tier)` entry expands the
/// containment boundary used by the structured filesystem tools
/// (`Read` / `Write` / `Edit` / `Glob` / `Grep` / `count_lines` /
/// `estimate_tokens`). `ReadOnly` roots permit reads only; `ReadWrite`
/// roots also permit writes. The primary workspace is always treated as
/// `ReadWrite` in addition to whatever is passed here.
///
/// `session_roots` is an optional shared, runtime-mutable slot
/// ([`super::dispatch::SharedSandboxRoots`]); when `Some`, every structured
/// fs tool receives a clone and consults it on each call, so the TUI
/// `/add-dir` command (and future interactive grants) can expand the
/// sandbox mid-session without rebuilding the runtime. Pass `None` for
/// headless/CLI runs that don't need runtime expansion.
///
/// This is how `--add-dir`, `[sandbox] extra_dirs`, and the TUI `/add-dir`
/// command make out-of-workspace files reachable by the agent without
/// weakening the sandbox for any other tool.
///
/// `web_search_provider`, `web_search_api_key`, `web_search_jina_key` are
/// optional search config values from the runtime Config. When `None`,
/// `WebSearch` falls back to env vars / Jina zero-config. These are exposed
/// at this level so all frontends (CLI, TUI, HTTP) get kernel-level config
/// propagation without each frontend wiring them separately.
///
/// `bg_manager` is an optional shared background-job manager. When `Some`,
/// `RunBackground` and `CheckBackground` tools use the shared manager
/// instead of creating their own. This lets the TUI backend observe job
/// completions via the same manager. When `None` (default for CLI/HTTP
/// paths), a new manager is created internally.
#[allow(clippy::too_many_arguments)]
pub fn build_standard_tools_with_roots(
    workspace: &std::path::Path,
    extra_roots: &[(std::path::PathBuf, super::dispatch::AccessTier)],
    session_roots: Option<super::dispatch::SharedSandboxRoots>,
    skills: &[crate::skills::Skill],
    shell_timeout_secs: u64,
    web_search_provider: Option<String>,
    web_search_api_key: Option<String>,
    web_search_jina_key: Option<String>,
    bg_manager: Option<Arc<tokio::sync::Mutex<super::run_background::BackgroundJobManager>>>,
) -> ToolRegistry {
    let bg_manager = bg_manager.unwrap_or_else(|| {
        Arc::new(tokio::sync::Mutex::new(
            super::run_background::BackgroundJobManager::new(),
        ))
    });
    let todo_list = Arc::new(std::sync::RwLock::new(Vec::<super::todo::TodoItem>::new()));
    let read_state = Arc::new(Mutex::new(ReadFileState::new()));
    let mut registry = ToolRegistry::local()
        .with_read_file_state(read_state.clone())
        .register(Arc::new(
            super::fs::ReadFile::new(workspace)
                .with_extra_roots(extra_roots.iter().cloned())
                .with_session_roots_opt(session_roots.clone())
                .with_read_state(read_state.clone()),
        ))
        .register(Arc::new(
            super::fs::WriteFile::new(workspace)
                .with_extra_roots(extra_roots.iter().cloned())
                .with_session_roots_opt(session_roots.clone()),
        ))
        .register(Arc::new(
            super::edit::EditTool::new(workspace)
                .with_extra_roots(extra_roots.iter().cloned())
                .with_session_roots_opt(session_roots.clone())
                .with_read_state(read_state.clone()),
        ))
        .register(Arc::new(
            super::shell::RunShell::new(workspace)
                .with_timeout(std::time::Duration::from_secs(shell_timeout_secs)),
        ))
        .register(Arc::new(
            super::search::SearchFiles::new(workspace)
                .with_extra_roots(extra_roots.iter().cloned())
                .with_session_roots_opt(session_roots.clone()),
        ))
        .register(Arc::new(
            super::glob::GlobTool::new(workspace)
                .with_extra_roots(extra_roots.iter().cloned())
                .with_session_roots_opt(session_roots.clone()),
        ))
        .register(Arc::new(super::run_background::RunBackground::new(
            workspace,
            bg_manager.clone(),
        )))
        .register(Arc::new(super::run_background::CheckBackground::new(
            bg_manager,
        )))
        .register(Arc::new(
            super::estimate_tokens::EstimateTokens::new(workspace)
                .with_extra_roots(extra_roots.iter().cloned())
                .with_session_roots_opt(session_roots.clone()),
        ))
        .register(Arc::new(super::memory::Remember::new(workspace)))
        .register(Arc::new(super::memory::Recall::new(workspace)))
        .register(Arc::new(super::memory::Forget::new(workspace)))
        .register(Arc::new(super::facts::RememberFact::new(workspace)))
        .register(Arc::new(super::facts::RecallFact::new(workspace)))
        .register(Arc::new(super::facts::ForgetFact::new(workspace)))
        .register(Arc::new(super::facts::UpdateFact::new(workspace)))
        .register(Arc::new(super::episodic_recall::EpisodicRecall::new(
            workspace,
        )))
        .register(Arc::new(super::memory::WorkingMemoryTool::new(workspace)))
        .register(Arc::new(super::memory::ScratchpadGet::new(workspace)))
        .register(Arc::new(super::memory::ScratchpadDelete::new(workspace)))
        .register(Arc::new(super::memory::ScratchpadList::new(workspace)))
        .register(Arc::new(super::todo::TodoWriteTool::new(
            todo_list,
            Arc::new(crate::event::NullSink),
        )))
        .register(Arc::new(super::a2a::A2aCallTool::new()))
        .register(Arc::new(super::a2a::A2aCardTool::new()))
        .register(Arc::new(super::a2a::A2aTaskCheckTool::new()));

    // Goal-201: plan mode tools are channel capabilities (TUI / HTTP only).
    // They are registered exclusively by AgentRuntimeBuilder::build() which
    // wires them to the real PlanApprovalGate and EventSink.  Headless /
    // CLI / self-improve runs that call build_standard_tools() directly
    // will not have these tools, preventing the LLM from blocking on an
    // interactive review that can never complete.

    #[cfg(feature = "web_fetch")]
    {
        registry = registry.register(Arc::new(super::web_fetch::WebFetch::new()));
    }

    #[cfg(feature = "web_search")]
    {
        let search = super::web_search::WebSearch::new().with_search_config(
            web_search_provider,
            web_search_api_key,
            web_search_jina_key,
        );
        registry = registry.register(Arc::new(search));
    }

    if !skills.is_empty() {
        registry = registry.register(Arc::new(super::load_skill::LoadSkill::new(skills.to_vec())));
    }

    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    /// Minimal read-only test tool.
    struct ReadOnlyTool {
        name: &'static str,
    }

    #[async_trait]
    impl Tool for ReadOnlyTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.name.to_string(),
                description: "read-only test tool".into(),
                parameters: serde_json::json!({"type":"object","properties":{}}),
            }
        }
        fn side_effect_class(&self) -> super::super::audit::ToolSideEffect {
            super::super::audit::ToolSideEffect::ReadOnly
        }
        async fn execute(&self, _args: Value) -> crate::error::Result<String> {
            Ok("read-result".into())
        }
    }

    /// Minimal mutating (external side-effect) test tool.
    struct MutatingTool {
        name: &'static str,
    }

    #[async_trait]
    impl Tool for MutatingTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.name.to_string(),
                description: "mutating test tool".into(),
                parameters: serde_json::json!({"type":"object","properties":{}}),
            }
        }
        async fn execute(&self, _args: Value) -> crate::error::Result<String> {
            Ok("mutated".into())
        }
    }

    fn make_registry() -> ToolRegistry {
        ToolRegistry::local()
    }

    // --- register / find_by_name / names / specs / get ---

    #[test]
    fn register_and_find_by_name() {
        let reg = make_registry().register(Arc::new(ReadOnlyTool { name: "Alpha" }));
        let found = reg.find_by_name("Alpha");
        assert!(found.is_some(), "registered tool must be findable by name");
        assert!(reg.find_by_name("NoSuch").is_none());
    }

    #[test]
    fn names_returns_registered_tool_names() {
        let reg = make_registry()
            .register(Arc::new(ReadOnlyTool { name: "T1" }))
            .register(Arc::new(MutatingTool { name: "T2" }));
        let mut names = reg.names();
        names.sort();
        assert_eq!(names, vec!["T1", "T2"]);
    }

    #[test]
    fn names_empty_on_fresh_registry() {
        assert!(make_registry().names().is_empty());
    }

    #[test]
    fn specs_returns_tool_specs() {
        let reg = make_registry().register(Arc::new(ReadOnlyTool { name: "SpecTool" }));
        let specs = reg.specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "SpecTool");
    }

    #[test]
    fn get_returns_tool_by_name() {
        let reg = make_registry().register(Arc::new(MutatingTool { name: "Getter" }));
        assert!(reg.get("Getter").is_some());
        assert!(reg.get("Missing").is_none());
    }

    // --- register_mut ---

    #[test]
    fn register_mut_adds_tool() {
        let mut reg = make_registry();
        reg.register_mut(Arc::new(ReadOnlyTool { name: "MutReg" }));
        assert!(reg.find_by_name("MutReg").is_some());
    }

    // --- is_readonly / is_readonly_for_call ---

    #[test]
    fn is_readonly_true_for_readonly_tool() {
        let reg = make_registry().register(Arc::new(ReadOnlyTool { name: "RO" }));
        assert!(
            reg.is_readonly("RO"),
            "ReadOnly tool must be is_readonly=true"
        );
        assert!(
            !reg.is_readonly("Missing"),
            "unknown tool must be is_readonly=false"
        );
    }

    #[test]
    fn is_readonly_false_for_mutating_tool() {
        let reg = make_registry().register(Arc::new(MutatingTool { name: "Mut" }));
        assert!(
            !reg.is_readonly("Mut"),
            "External tool must be is_readonly=false"
        );
    }

    #[test]
    fn is_readonly_for_call_delegates_to_tool() {
        let reg = make_registry().register(Arc::new(ReadOnlyTool { name: "Ro2" }));
        let args = serde_json::json!({});
        assert!(reg.is_readonly_for_call("Ro2", &args));
        assert!(!reg.is_readonly_for_call("Missing", &args));
    }

    // --- retain_tools ---

    #[test]
    fn retain_tools_keeps_only_allowed() {
        let mut reg = make_registry()
            .register(Arc::new(ReadOnlyTool { name: "Keep" }))
            .register(Arc::new(MutatingTool { name: "Drop" }));
        reg.retain_tools(&["Keep".to_string()]);
        assert!(reg.find_by_name("Keep").is_some());
        assert!(reg.find_by_name("Drop").is_none());
    }

    // --- with_same_transport ---

    #[test]
    fn with_same_transport_returns_empty_registry() {
        let reg = make_registry().register(Arc::new(ReadOnlyTool { name: "Src" }));
        let empty = reg.with_same_transport();
        assert!(
            empty.names().is_empty(),
            "with_same_transport must return empty registry"
        );
    }

    // --- fork ---

    #[test]
    fn fork_clones_tools() {
        let reg = make_registry().register(Arc::new(ReadOnlyTool { name: "ForkMe" }));
        let forked = reg.fork();
        assert!(forked.find_by_name("ForkMe").is_some());
    }

    // --- build_standard_tools produces a non-empty registry ---

    #[test]
    fn build_standard_tools_is_nonempty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let registry = build_standard_tools(tmp.path(), &[], 30);
        assert!(
            !registry.names().is_empty(),
            "build_standard_tools must register at least one tool"
        );
    }

    // ── DeferredTool mock for deferred-partition tests ───────────────────────

    struct DeferredTool {
        name: &'static str,
        description: &'static str,
    }

    #[async_trait]
    impl Tool for DeferredTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.name.to_string(),
                description: self.description.to_string(),
                parameters: serde_json::json!({"type":"object","properties":{}}),
            }
        }
        fn is_deferred(&self) -> bool {
            true
        }
        async fn execute(&self, _args: Value) -> crate::error::Result<String> {
            Ok("deferred-result".into())
        }
    }

    // ── register_with_aliases / find_by_name via alias ───────────────────────

    #[test]
    fn register_with_aliases_finds_by_primary_and_alias() {
        let reg = make_registry().register_with_aliases(
            Arc::new(ReadOnlyTool { name: "Primary" }),
            &["alias1", "alias2"],
        );
        assert!(
            reg.find_by_name("Primary").is_some(),
            "primary name must work"
        );
        assert!(reg.find_by_name("alias1").is_some(), "alias1 must resolve");
        assert!(reg.find_by_name("alias2").is_some(), "alias2 must resolve");
        assert!(
            reg.find_by_name("unknown").is_none(),
            "unknown must return None"
        );
    }

    // ── register_mut_with_aliases ─────────────────────────────────────────────

    #[test]
    fn register_mut_with_aliases_adds_alias() {
        let mut reg = make_registry();
        reg.register_mut_with_aliases(Arc::new(ReadOnlyTool { name: "MutAliased" }), &["malias"]);
        assert!(reg.find_by_name("MutAliased").is_some());
        assert!(
            reg.find_by_name("malias").is_some(),
            "mutably-registered alias must resolve"
        );
    }

    // ── specs_partitioned ─────────────────────────────────────────────────────

    #[test]
    fn specs_partitioned_separates_eager_from_deferred() {
        let reg = make_registry()
            .register(Arc::new(ReadOnlyTool { name: "Eager1" }))
            .register(Arc::new(ReadOnlyTool { name: "Eager2" }))
            .register(Arc::new(DeferredTool {
                name: "Defer1",
                description: "deferred.",
            }));
        let (eager, deferred) = reg.specs_partitioned();
        assert_eq!(eager.len(), 2, "two eager tools expected");
        assert_eq!(deferred.len(), 1, "one deferred tool expected");
        assert_eq!(deferred[0].name, "Defer1");
    }

    #[test]
    fn specs_partitioned_all_eager_when_no_deferred() {
        let reg = make_registry()
            .register(Arc::new(ReadOnlyTool { name: "E1" }))
            .register(Arc::new(MutatingTool { name: "E2" }));
        let (eager, deferred) = reg.specs_partitioned();
        assert_eq!(eager.len(), 2);
        assert!(deferred.is_empty(), "no deferred tools expected");
    }

    // ── split_eager_deferred ─────────────────────────────────────────────────

    #[test]
    fn split_eager_deferred_puts_deferred_in_second_slot() {
        let reg = make_registry()
            .register(Arc::new(ReadOnlyTool { name: "EagerX" }))
            .register(Arc::new(DeferredTool {
                name: "DeferX",
                description: "First sentence. Second sentence.",
            }));
        let (eager, deferred) = reg.split_eager_deferred();
        assert_eq!(eager.len(), 1);
        assert_eq!(deferred.len(), 1);
        // hint should be first sentence only
        let (_spec, hint) = &deferred[0];
        assert_eq!(
            hint.as_deref(),
            Some("First sentence"),
            "hint must be first sentence"
        );
    }

    #[test]
    fn split_eager_deferred_hint_is_none_for_empty_description() {
        let reg = make_registry().register(Arc::new(DeferredTool {
            name: "NoDesc",
            description: ".",
        }));
        let (_eager, deferred) = reg.split_eager_deferred();
        // "." splits to ["", ""], first is empty after trim → hint = None
        let (_spec, hint) = &deferred[0];
        assert!(
            hint.is_none(),
            "empty sentence before '.' → hint must be None"
        );
    }

    // ── is_deferred_spec ─────────────────────────────────────────────────────

    #[test]
    fn is_deferred_spec_true_for_deferred_tool() {
        let reg = make_registry().register(Arc::new(DeferredTool {
            name: "DS",
            description: "d.",
        }));
        let specs = reg.specs();
        let ds_spec = specs.iter().find(|s| s.name == "DS").unwrap();
        assert!(
            reg.is_deferred_spec(ds_spec),
            "deferred tool spec must be deferred"
        );
    }

    #[test]
    fn is_deferred_spec_false_for_eager_tool() {
        let reg = make_registry().register(Arc::new(ReadOnlyTool { name: "EagerSpec" }));
        let specs = reg.specs();
        let spec = &specs[0];
        assert!(
            !reg.is_deferred_spec(spec),
            "eager tool must not be deferred"
        );
    }

    #[test]
    fn is_deferred_spec_false_for_unknown_spec() {
        let reg = make_registry();
        let fake_spec = ToolSpec {
            name: "Phantom".to_string(),
            description: "not registered".to_string(),
            parameters: serde_json::json!({}),
        };
        assert!(
            !reg.is_deferred_spec(&fake_spec),
            "unknown spec must return false"
        );
    }

    // ── retain_tools – alias cleanup ─────────────────────────────────────────

    #[test]
    fn retain_tools_removes_aliases_of_dropped_tools() {
        let mut reg = make_registry()
            .register_with_aliases(Arc::new(ReadOnlyTool { name: "Keep2" }), &["k_alias"])
            .register_with_aliases(Arc::new(MutatingTool { name: "Drop2" }), &["d_alias"]);
        reg.retain_tools(&["Keep2".to_string()]);
        assert!(reg.find_by_name("Keep2").is_some());
        assert!(
            reg.find_by_name("k_alias").is_some(),
            "kept tool's alias must survive"
        );
        assert!(reg.find_by_name("Drop2").is_none());
        assert!(
            reg.find_by_name("d_alias").is_none(),
            "dropped tool's alias must be removed"
        );
    }

    // ── with_touched_files / touched_files / clear_touched_files ────────────

    #[test]
    fn with_touched_files_and_clear() {
        use std::sync::{Arc, Mutex};
        let slot: Arc<Mutex<TouchedFiles>> = Arc::new(Mutex::new(TouchedFiles::default()));
        let mut reg = make_registry().with_touched_files(slot);
        assert!(
            reg.touched_files().is_some(),
            "slot must be present after with_touched_files"
        );
        reg.clear_touched_files();
        assert!(
            reg.touched_files().is_none(),
            "slot must be gone after clear_touched_files"
        );
    }

    // ── with_read_file_state / read_file_state ───────────────────────────────

    #[test]
    fn with_read_file_state_roundtrip() {
        let slot: Arc<Mutex<ReadFileState>> = Arc::new(Mutex::new(ReadFileState::default()));
        let reg = make_registry().with_read_file_state(slot);
        assert!(
            reg.read_file_state().is_some(),
            "read_file_state must be Some after setter"
        );
        let reg2 = make_registry();
        assert!(
            reg2.read_file_state().is_none(),
            "fresh registry has no read_file_state"
        );
    }

    // ── permission_mode ───────────────────────────────────────────────────────

    #[test]
    fn permission_mode_default_on_fresh_registry() {
        let reg = make_registry();
        assert!(
            matches!(reg.permission_mode(), PermissionMode::Default),
            "fresh registry must have Default permission mode"
        );
    }

    #[test]
    fn with_permissions_sets_permission_mode() {
        let config = PermissionsConfig {
            mode: PermissionMode::AcceptEdits,
            ..Default::default()
        };
        let reg = make_registry().with_permissions(config);
        assert!(
            matches!(reg.permission_mode(), PermissionMode::AcceptEdits),
            "permission_mode must reflect config.mode"
        );
    }

    // ── permissions_config ────────────────────────────────────────────────────

    #[test]
    fn permissions_config_none_on_fresh_registry() {
        let reg = make_registry();
        assert!(
            reg.permissions_config().is_none(),
            "fresh registry has no permissions_config"
        );
    }

    #[test]
    fn permissions_config_some_after_with_permissions() {
        let config = PermissionsConfig {
            mode: PermissionMode::Default,
            ..Default::default()
        };
        let reg = make_registry().with_permissions(config);
        assert!(
            reg.permissions_config().is_some(),
            "permissions_config must be Some"
        );
    }

    // ── is_plan_mode ──────────────────────────────────────────────────────────

    #[test]
    fn is_plan_mode_false_for_default_tool_and_no_config() {
        let reg = make_registry().register(Arc::new(ReadOnlyTool { name: "PlanTool" }));
        // Without a plan-mode config, must return false
        assert!(
            !reg.is_plan_mode("PlanTool"),
            "no plan-mode config → must be false"
        );
        assert!(!reg.is_plan_mode("Unknown"), "unknown tool → must be false");
    }

    // ── with_permission_hook ──────────────────────────────────────────────────

    struct DenyAllHook;
    #[async_trait]
    impl PermissionHook for DenyAllHook {
        async fn check(&self, _tool_name: &str, _args: &serde_json::Value) -> PermissionDecision {
            PermissionDecision::Deny("no".into())
        }
    }

    #[test]
    fn with_permission_hook_installs_hook() {
        let reg = make_registry().with_permission_hook(Arc::new(DenyAllHook));
        assert!(reg.permission_hook.is_some(), "hook must be installed");
    }

    #[test]
    fn clear_permission_hook_removes_hook() {
        let mut reg = make_registry().with_permission_hook(Arc::new(DenyAllHook));
        reg.clear_permission_hook();
        assert!(reg.permission_hook.is_none(), "hook must be cleared");
    }

    // ── with_policy / policy ──────────────────────────────────────────────────

    #[test]
    fn with_policy_and_policy_roundtrip() {
        use super::policy_sandbox::PolicyConfig;
        let reg = make_registry();
        assert!(reg.policy().is_none(), "fresh registry has no policy");
        let cfg = PolicyConfig::default();
        let reg2 = reg.with_policy(cfg);
        assert!(
            reg2.policy().is_some(),
            "policy must be Some after with_policy"
        );
    }

    // ── with_headless ─────────────────────────────────────────────────────────

    #[test]
    fn with_headless_sets_flag() {
        let reg = make_registry();
        assert!(!reg.headless, "fresh registry must not be headless");
        let reg2 = reg.with_headless(true);
        assert!(
            reg2.headless,
            "headless must be true after with_headless(true)"
        );
    }

    // ── transport roundtrip ───────────────────────────────────────────────────

    #[test]
    fn transport_accessor_returns_same_arc() {
        let reg = make_registry();
        // Just verifying the accessor doesn't crash and returns an Arc
        let _transport = reg.transport();
    }

    // ── fork preserves tools and is independent ───────────────────────────────

    #[test]
    fn fork_is_independent_from_original() {
        let mut reg = make_registry().register(Arc::new(ReadOnlyTool { name: "ForkOrig" }));
        let mut forked = reg.fork();
        // Adding to forked doesn't affect original
        forked.register_mut(Arc::new(MutatingTool { name: "ForkedOnly" }));
        assert!(
            reg.find_by_name("ForkedOnly").is_none(),
            "forked-only tool must not appear in original"
        );
        // Removing from forked doesn't affect original
        reg.retain_tools(&["ForkOrig".to_string()]);
        assert!(
            forked.find_by_name("ForkOrig").is_some(),
            "original retain must not affect fork"
        );
    }

    // ── names returns expected set ────────────────────────────────────────────

    #[test]
    fn names_does_not_include_aliases() {
        let reg = make_registry()
            .register_with_aliases(Arc::new(ReadOnlyTool { name: "PrimeName" }), &["anAlias"]);
        let names = reg.names();
        assert!(
            names.contains(&"PrimeName".to_string()),
            "primary name must be in names()"
        );
        assert!(
            !names.contains(&"anAlias".to_string()),
            "alias must NOT be in names()"
        );
    }
}
