//! Tool registry: the [`Tool`] trait, [`ToolRegistry`] collection, and
//! [`build_standard_tools`] factory.
//!
//! Tools are orthogonal to the agent and to each other. To add a capability
//! you implement [`Tool`] and register it; no other file changes.

use async_trait::async_trait;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use tokio::sync::RwLock;

use crate::agent::PermissionDecision;
use crate::error::Result;
use crate::llm::ToolSpec;
use crate::permissions::auto_classifier::AutoClassifier;
use crate::permissions::SharedPermissions;
use crate::permissions::{PermissionMode, PermissionsConfig};
use crate::tools::fs::ReadFileState;
use crate::tools::tool_kind::ToolKind;

use super::audit::TouchedFiles;
use super::policy_sandbox;

/// A `(ToolSpec, optional_search_hint)` pair returned by
/// [`ToolRegistry::split_eager_deferred`].
pub type SpecWithHint = (ToolSpec, Option<String>);

/// Goal 394: freshly allocated, session-scoped state slots handed to
/// [`Tool::fork_box`] by [`ToolRegistry::fork_session`].
///
/// One instance exists per fork. Stateful tools rewire themselves to SHARE
/// these slots with their siblings *inside* the fork — keeping cross-tool
/// chains intact (e.g. `Read` records → `Edit` enforces) — while sharing
/// nothing with the parent registry or sibling forks (session isolation).
pub struct SessionToolState {
    /// Fresh read-before-edit guard slot. `Some` iff the source registry had
    /// one; holds a *copy* of the parent's records at fork time (fork
    /// semantics: the child inherits what was already read; reads made after
    /// the fork stay invisible across registries).
    pub(crate) read_state: Option<Arc<Mutex<ReadFileState>>>,
    /// Fresh, EMPTY background-job manager. Running jobs belong to the
    /// session that spawned them and are deliberately not inherited.
    pub(crate) bg_manager: Arc<tokio::sync::Mutex<super::run_background::BackgroundJobManager>>,
    /// Fresh sandbox-roots slot seeded from the parent's current roots, so
    /// roots granted before the fork keep working while post-fork expansions
    /// (TUI `/add-dir`) stay session-local.
    pub(crate) session_roots: Option<super::dispatch::SharedSandboxRoots>,
    /// Fresh, EMPTY deliverables ledger (Goal #133) — same workspace and
    /// budgets, private turn bookkeeping and shadow index. `Some` iff the
    /// source registry had one; `Present` / `ChangeLedger` rewire themselves
    /// to it, so a fork can never render (or declare into) the parent's
    /// ledger.
    pub(crate) deliverables: Option<Arc<crate::deliverables::Deliverables>>,
}

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

    /// Return the ACP `ToolKind` for tool_call notifications (Sprint 3).
    /// Defaults to `ToolKind::Other`. Concrete tools override to map
    /// to their corresponding ACP category.
    fn kind(&self) -> ToolKind {
        ToolKind::Other
    }

    /// The MCP server this tool is proxied from, or `None` for native tools.
    /// Overridden by [`crate::mcp::McpTool`] so a registry rebuild (e.g. the
    /// container sandbox tier) can re-attach already-spawned MCP tools rather
    /// than silently dropping them.
    fn mcp_server_name(&self) -> Option<&str> {
        None
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

    /// Goal 394: dyn-compatible session-fork hook used by
    /// [`ToolRegistry::fork_session`].
    ///
    /// Tools holding session-scoped mutable state override this to return a
    /// clone of themselves rewired to the fresh slots in `state`:
    ///
    /// - `ReadFile` / `WriteFile` / `EditTool` — the shared
    ///   `Arc<Mutex<ReadFileState>>` guard slot (injected at construction,
    ///   so replacing only the registry field would leave the tools holding
    ///   the parent's `Arc`);
    /// - `RunBackground` / `CheckBackground` / `WatchFile` / `StopLoop` —
    ///   the shared background-job manager;
    /// - the structured fs tools — the runtime-mutable sandbox-roots slot;
    /// - `Present` / `ChangeLedger` — the session's deliverables ledger.
    ///
    /// The default `None` means "session-stateless": every field the tool
    /// holds is immutable configuration (workspace root, transport,
    /// permissions, policy, …), so `fork_session` keeps sharing the original
    /// `Arc` — semantically identical to a clone, allocation-free.
    ///
    /// When adding session state to a tool, override this method or the
    /// state will silently stay shared across `fork_session()` boundaries.
    fn fork_box(&self, _state: &SessionToolState) -> Option<Arc<dyn Tool>> {
        None
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

/// No-op permission hook that allows every tool call.
///
/// Used as the default when no ACP permission bridge is configured.
/// Always returns [`PermissionDecision::Allow`] without any side effects.
pub struct PermissionHookDisabled;

#[async_trait]
impl PermissionHook for PermissionHookDisabled {
    async fn check(&self, _tool_name: &str, _args: &serde_json::Value) -> PermissionDecision {
        PermissionDecision::Allow
    }
}

/// NOTE: `Clone` shares every `Arc` with the source registry — including the
/// read-before-edit guard, the touched-files collector, and the
/// runtime-mutable sandbox-roots slot. Use [`ToolRegistry::fork_session`]
/// (Goal 394) when per-session isolation is required; [`ToolRegistry::fork`]
/// is a legacy alias for `clone()`.
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
    elicitation: Option<crate::tools::elicitation::SharedElicitationHandler>,
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
    pub(crate) headless: bool,
    /// Goal-199: external hook runner for headless permission checks.
    pub(crate) hook_runner: crate::hooks::ExternalHookRunner,

    /// Goal-200: optional auto classifier for `PermissionMode::Auto`.
    /// When `Some`, each tool call in Auto mode is classified by the
    /// LLM before execution. Wrapped in a `Mutex` (tokio) because `classify()`
    /// takes `&mut self` (it updates the denial tracker).
    pub(crate) auto_classifier: Option<Arc<tokio::sync::Mutex<AutoClassifier>>>,
    /// Issue #31: the session's background-job manager. `Clone`/`fork_session`
    /// semantics differ — `Clone` SHARES it (same session continues),
    /// `fork_session` creates a fresh one (jobs are not inherited). Kept at
    /// registry level so `destroy_environment` can drain it wherever the
    /// registry went. Empty (unused) for empty `new()`/`local()` registries.
    pub(crate) bg_manager: Arc<tokio::sync::Mutex<super::run_background::BackgroundJobManager>>,
    /// Issue #65: set by [`Self::retain_tools`] — an explicit surface
    /// decision happened (operator allow-list / coordinator prune). Lets
    /// `AgentRuntimeBuilder::build` distinguish "registry never had
    /// TodoWrite because it's the default empty/local one" (safe to add the
    /// real tool) from "a filter deliberately dropped it" (must stay strict).
    surface_filtered: bool,
    /// Goal #133: session-scoped deliverables ledger (declared outputs +
    /// per-turn change ledger). `None` when the workspace cannot host one
    /// (unwritable user data dir) or the subsystem is disabled — the
    /// `Present` / `ChangeLedger` tools are then not registered either.
    /// The baseline is captured from `dispatch_after_permission_check`
    /// *before* the first mutating tool call of a turn.
    pub(crate) deliverables: Option<Arc<crate::deliverables::Deliverables>>,
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
            bg_manager: Arc::new(tokio::sync::Mutex::new(
                super::run_background::BackgroundJobManager::new(),
            )),
            surface_filtered: false,
            deliverables: None,
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

    /// The registry's background-job manager (issue #31). Same `Arc` as the
    /// `RunBackground` / `CheckBackground` tools hold, so draining it kills
    /// the bookkeeping for every job this session spawned.
    pub fn bg_manager(
        &self,
    ) -> &Arc<tokio::sync::Mutex<super::run_background::BackgroundJobManager>> {
        &self.bg_manager
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
            // Issue #31: empty registry, same session manager (Clone shares).
            bg_manager: self.bg_manager.clone(),
            // Fresh empty registry — any later filter marks it itself.
            surface_filtered: false,
            deliverables: self.deliverables.clone(),
        }
    }

    /// Create a session-isolated copy of this registry (Goal 394).
    ///
    /// [`Clone`] — and the legacy [`fork`](Self::fork) — share every `Arc`
    /// with the source registry. `fork_session()` instead REBUILDS the
    /// session-scoped mutable state and keeps only genuinely external or
    /// process-level resources shared.
    ///
    /// Rebuilt (fresh per fork):
    ///
    /// - `read_file_state` — the read-before-edit guard. A fresh slot holding
    ///   a *copy* of the parent's records at fork time (fork semantics: the
    ///   child inherits what was already read; reads after the fork are
    ///   invisible across registries). `ReadFile` / `WriteFile` / `EditTool`
    ///   are re-constructed against the new slot via [`Tool::fork_box`] —
    ///   replacing only the registry field would leave the tools holding the
    ///   parent's `Arc`.
    /// - `touched` — the touched-files collector starts empty, so
    ///   checkpoint / audit attribution belongs to the fork, not the parent.
    /// - `bg_manager` — background jobs are not inherited; the fork starts
    ///   with an empty manager (`RunBackground` / `CheckBackground` /
    ///   `WatchFile` / `StopLoop` are rewired to it).
    /// - `session_roots` — a fresh slot seeded from the parent's current
    ///   roots; `/add-dir`-style expansions after the fork stay
    ///   session-local.
    /// - `deliverables` — a fresh ledger for the same workspace (private
    ///   turn bookkeeping and shadow index), rewired into `Present` /
    ///   `ChangeLedger` via [`Tool::fork_box`]. Without that rewiring the
    ///   tools would keep rendering the fork source's ledger.
    ///
    /// Shared on purpose — process-level or external resources; sharing is
    /// what makes a fork cheap and is NOT a bug:
    ///
    /// - `transport` — the execution-environment binding (Goal 401/402);
    ///   immutable configuration, one instance per registry by design.
    /// - `permissions` / `permission_mode` / `permission_hook` / `policy` /
    ///   `auto_classifier` / `headless` / `hook_runner` — permission
    ///   configuration is a property of the process, not of a session.
    /// - MCP client(s) and the elicitation handler — MCP servers are
    ///   external resources, deliberately shared per server (they are
    ///   startup-time connections; a fork must not reconnect).
    /// - `aliases` — a static name → name mapping.
    /// - workspace-level stores inside tools (scratchpad / memory / facts /
    ///   todo list) — those belong to the WORKSPACE, not the session (see
    ///   `.dev/goals/394-per-session-tool-state-isolation.md`).
    pub fn fork_session(&self) -> Self {
        // ONE fresh read-state slot per fork; every stateful tool in the
        // fork is rewired to it, so the cross-tool guard chain survives
        // inside the fork while becoming invisible across forks.
        let fork_read_state = self.read_file_state.as_ref().map(|slot| {
            Arc::new(Mutex::new(
                slot.lock().unwrap_or_else(|p| p.into_inner()).clone(),
            ))
        });
        let fork_roots = self.session_roots.as_ref().map(|slot| {
            let seeded = slot.read().map(|roots| roots.clone()).unwrap_or_default();
            Arc::new(std::sync::RwLock::new(seeded))
        });
        // ONE fresh ledger per fork, shared by the fork's `Present` /
        // `ChangeLedger` (via `state`) and by its runtime (via the registry
        // field) — a fork is a different session, so a sub-agent's
        // declarations and change set stay out of the parent's turn ledger,
        // and two instances never share one shadow index file.
        let fork_deliverables = self.fresh_deliverables();
        let state = SessionToolState {
            read_state: fork_read_state.clone(),
            bg_manager: Arc::new(tokio::sync::Mutex::new(
                super::run_background::BackgroundJobManager::new(),
            )),
            session_roots: fork_roots.clone(),
            deliverables: fork_deliverables.clone(),
        };
        let tools: BTreeMap<String, Arc<dyn Tool>> = self
            .tools
            .iter()
            .map(|(name, tool)| {
                // Session-stateless tools (fork_box → None) keep sharing the
                // original Arc: every field they hold is immutable config.
                (
                    name.clone(),
                    tool.fork_box(&state).unwrap_or_else(|| tool.clone()),
                )
            })
            .collect();
        Self {
            tools,
            aliases: self.aliases.clone(),
            transport: self.transport.clone(),
            permissions: self.permissions.clone(),
            permission_mode: self.permission_mode.clone(),
            // Touched-files attribution starts over for the new session.
            touched: self
                .touched
                .as_ref()
                .map(|_| Arc::new(Mutex::new(TouchedFiles::new()))),
            read_file_state: fork_read_state,
            session_roots: fork_roots,
            #[cfg(feature = "mcp")]
            elicitation: self.elicitation.clone(),
            permission_hook: self.permission_hook.clone(),
            policy: self.policy.clone(),
            headless: self.headless,
            hook_runner: self.hook_runner.clone(),
            auto_classifier: self.auto_classifier.clone(),
            // Fresh manager built above via `state.bg_manager` semantics.
            bg_manager: state.bg_manager.clone(),
            // Session forks inherit the surface contract (issue #65).
            surface_filtered: self.surface_filtered,
            // Built above, so the tools rewired via `state` and the runtime
            // that reads this field share ONE ledger. Falls back to the
            // parent's ledger if a fresh one cannot be created.
            deliverables: fork_deliverables,
        }
    }

    /// Goal #133: build a ledger for the same workspace but with private
    /// per-instance state (shadow index, turn bookkeeping). `None` when this
    /// registry has no ledger at all.
    fn fresh_deliverables(&self) -> Option<Arc<crate::deliverables::Deliverables>> {
        let parent = self.deliverables.as_ref()?;
        let root =
            parent
                .root()
                .join("forks")
                .join(format!("{}-{}", std::process::id(), next_fork_seq()));
        crate::deliverables::Deliverables::new(
            parent.workspace().to_path_buf(),
            root,
            parent.budgets().clone(),
        )
        .ok()
        .map(Arc::new)
        .or_else(|| Some(parent.clone()))
    }

    /// Legacy fork entry point — a plain [`Clone`], kept for existing call
    /// sites.
    ///
    /// Historical versions of this doc claimed fork "isolates" tools; that
    /// was never true. `Clone` shares every `Arc` — the read-before-edit
    /// guard, the touched-files collector, the sandbox-roots slot — so two
    /// registries derived this way observe each other's state, and the
    /// `Tool` trait has no per-tool `fork()` method. Use
    /// [`fork_session`](Self::fork_session) (Goal 394) when per-session
    /// isolation is required (HTTP session hosts, sub-agents).
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

    /// Attach (or clear) the deliverables ledger (goal #133). The registry
    /// only routes to it — the runtime picks the same `Arc` up via
    /// [`Self::deliverables`] so per-turn bookkeeping and the tools agree.
    pub fn with_deliverables(
        mut self,
        ledger: Option<Arc<crate::deliverables::Deliverables>>,
    ) -> Self {
        self.deliverables = ledger;
        self
    }

    /// The deliverables ledger shared by this registry's `Present` /
    /// `ChangeLedger` tools, if the subsystem is active.
    pub fn deliverables(&self) -> Option<Arc<crate::deliverables::Deliverables>> {
        self.deliverables.clone()
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
    pub fn with_elicitation_slot(
        mut self,
        slot: crate::tools::elicitation::SharedElicitationHandler,
    ) -> Self {
        self.elicitation = Some(slot);
        self
    }

    /// Return the shared elicitation-handler slot, if any.
    #[cfg(feature = "mcp")]
    pub fn elicitation_slot(&self) -> Option<crate::tools::elicitation::SharedElicitationHandler> {
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

    /// Builder-style conditional registration: registers every tool produced
    /// by `make` only when `cond` is true. Lets factory code express
    /// "these tools are host-only" inline in the registration chain.
    fn register_when(mut self, cond: bool, make: impl FnOnce() -> Vec<Arc<dyn Tool>>) -> Self {
        if cond {
            for tool in make() {
                let name = tool.spec().name;
                self.tools.insert(name, tool);
            }
        }
        self
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

    /// Every registered tool proxied from an MCP server. Used to re-attach
    /// MCP routing after a registry is rebuilt from scratch (the container
    /// sandbox tier builds a fresh registry per session).
    pub fn mcp_tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools
            .values()
            .filter(|t| t.mcp_server_name().is_some())
            .cloned()
            .collect()
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

    /// Remove tools by primary name (case-sensitive). Aliases pointing at
    /// removed tools are dropped too. Used to disable host-executing tools
    /// in sandbox tiers where they would bypass the environment binding
    /// (e.g. `run_background` in the container tier, Goal 403).
    pub fn remove_tools(&mut self, names: &[&str]) {
        let drop_set: std::collections::HashSet<&str> = names.iter().copied().collect();
        self.tools
            .retain(|name, _| !drop_set.contains(name.as_str()));
        self.aliases
            .retain(|_, primary| self.tools.contains_key(primary));
    }

    /// Restrict the registry to only the named tools, removing all others.
    /// Tool names are matched case-insensitively. Aliases for removed tools
    /// are also dropped. Used by `--allow-tools` to give agents a limited
    /// tool set (e.g. read-only review agents).
    ///
    /// Marks the registry as explicitly filtered (see
    /// [`Self::surface_filtered`]) so downstream assembly steps don't
    /// re-add tools the operator removed.
    pub fn retain_tools(&mut self, allow: &[String]) {
        let allowed: std::collections::HashSet<String> =
            allow.iter().map(|n| n.to_lowercase()).collect();
        self.tools
            .retain(|name, _| allowed.contains(&name.to_lowercase()));
        self.aliases
            .retain(|_, primary| self.tools.contains_key(primary));
        self.surface_filtered = true;
    }

    /// Whether an explicit surface filter (`retain_tools`) ran on this
    /// registry. `true` means a missing tool was removed on purpose, not
    /// merely never registered.
    pub(crate) fn surface_filtered(&self) -> bool {
        self.surface_filtered
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

    /// Build a `HashMap<String, ToolKind>` mapping every registered tool
    /// name to its ACP `ToolKind` (Sprint 3). Used by `AcpBridge` to
    /// populate the `kind` field in `tool_call` notifications.
    pub fn build_kind_map(&self) -> HashMap<String, ToolKind> {
        self.tools
            .iter()
            .map(|(name, tool)| (name.clone(), tool.kind()))
            .collect()
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

/// Monotonic counter giving every session fork a private deliverables root,
/// so two ledgers never share one shadow index file.
fn next_fork_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
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
    // Goal 401/402: every I/O tool (Read / Write / Edit / Glob / Grep /
    // count_lines) must share the registry's single transport instance, so an
    // environment binding chosen here follows the tools automatically
    // (container tier, Goal 403). Do NOT let tools construct their own
    // LocalTransport — that would silently pin them to the host and undo the
    // sandbox.
    build_standard_tools_with_transport(
        Arc::new(super::transport::LocalTransport),
        workspace,
        extra_roots,
        session_roots,
        skills,
        shell_timeout_secs,
        web_search_provider,
        web_search_api_key,
        web_search_jina_key,
        Some(bg_manager),
    )
}

/// Same as [`build_standard_tools_with_roots`] but every I/O tool is bound
/// to the given shared transport (Goal 403 container tier: pass the
/// [`super::container_transport::ContainerTransport`] `Arc` here instead of
/// swapping it post-hoc — tools hold their own `Arc` clones, so a registry
/// built with `LocalTransport` can never be re-bound after the fact).
#[allow(clippy::too_many_arguments)]
pub fn build_standard_tools_with_transport(
    transport: Arc<dyn super::transport::ToolTransport>,
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
    build_standard_tools_with_transport_opt(
        transport,
        workspace,
        extra_roots,
        session_roots,
        skills,
        shell_timeout_secs,
        web_search_provider,
        web_search_api_key,
        web_search_jina_key,
        bg_manager,
        false,
    )
}

/// Extended form of [`build_standard_tools_with_transport`] with
/// `disable_host_exec`: when `true`, the host-process-executing tools
/// (`run_background` / `check_background` / `watch_file` / `stop_loop`) are
/// removed from the registry. They spawn commands via the HOST
/// `/bin/sh` regardless of the shared transport, so keeping them would let
/// the model bypass the container sandbox (issue #30: "container runs the
/// commands, host executes them" split). Isolating them inside the
/// container is future work; until then the container tier honestly drops
/// them instead of silently exposing host execution.
#[allow(clippy::too_many_arguments)]
pub fn build_standard_tools_with_transport_opt(
    transport: Arc<dyn super::transport::ToolTransport>,
    workspace: &std::path::Path,
    extra_roots: &[(std::path::PathBuf, super::dispatch::AccessTier)],
    session_roots: Option<super::dispatch::SharedSandboxRoots>,
    skills: &[crate::skills::Skill],
    shell_timeout_secs: u64,
    web_search_provider: Option<String>,
    web_search_api_key: Option<String>,
    web_search_jina_key: Option<String>,
    bg_manager: Option<Arc<tokio::sync::Mutex<super::run_background::BackgroundJobManager>>>,
    disable_host_exec: bool,
) -> ToolRegistry {
    // Only consumed when the `web_search` feature is enabled.
    #[cfg(not(feature = "web_search"))]
    let _ = (
        &web_search_provider,
        &web_search_api_key,
        &web_search_jina_key,
    );
    let bg_manager = bg_manager.unwrap_or_else(|| {
        Arc::new(tokio::sync::Mutex::new(
            super::run_background::BackgroundJobManager::new(),
        ))
    });
    let todo_list = Arc::new(std::sync::RwLock::new(Vec::<super::todo::TodoItem>::new()));
    let read_state = Arc::new(Mutex::new(ReadFileState::new()));
    // Goal 401/402: every I/O tool (Read / Write / Edit / Glob / Grep /
    // count_lines / Bash) must share the registry's single transport
    // instance, so an environment binding chosen here follows the tools
    // automatically (container tier, Goal 403). Do NOT let tools construct
    // their own LocalTransport — that would silently pin them to the host
    // and undo the sandbox.
    let shared_transport = transport;
    // Issue #93: one vector-memory backend pair per registry, shared by
    // remember / recall / forget so the write, read and delete paths agree on
    // the same index. Without this the tools keep their per-instance
    // `NoopVectorStore`, so semantic recall is dead code.
    let (memory_store, memory_embedding) = crate::memory::default_backends(workspace);
    let mut registry = ToolRegistry::new(shared_transport.clone())
        .with_read_file_state(read_state.clone())
        .register_with_aliases(
            Arc::new(
                super::fs::ReadFile::new(workspace)
                    .with_extra_roots(extra_roots.iter().cloned())
                    .with_session_roots_opt(session_roots.clone())
                    .with_read_state(read_state.clone())
                    .with_transport(shared_transport.clone()),
            ),
            &["read_file"],
        )
        .register_with_aliases(
            Arc::new(
                super::fs::WriteFile::new(workspace)
                    .with_extra_roots(extra_roots.iter().cloned())
                    .with_session_roots_opt(session_roots.clone())
                    .with_read_state(read_state.clone())
                    .with_transport(shared_transport.clone()),
            ),
            &["write_file"],
        )
        .register(Arc::new(
            super::edit::EditTool::new(workspace)
                .with_extra_roots(extra_roots.iter().cloned())
                .with_session_roots_opt(session_roots.clone())
                .with_read_state(read_state.clone())
                .with_transport(shared_transport.clone()),
        ))
        .register(Arc::new(
            super::shell::RunShell::new(workspace)
                .with_timeout(std::time::Duration::from_secs(shell_timeout_secs))
                .with_transport(shared_transport.clone()),
        ))
        .register(Arc::new(
            super::search::SearchFiles::new(workspace)
                .with_extra_roots(extra_roots.iter().cloned())
                .with_session_roots_opt(session_roots.clone())
                .with_transport(shared_transport.clone()),
        ))
        .register_with_aliases(
            Arc::new(
                super::glob::GlobTool::new(workspace)
                    .with_extra_roots(extra_roots.iter().cloned())
                    .with_session_roots_opt(session_roots.clone())
                    .with_transport(shared_transport.clone()),
            ),
            &["list_dir", "glob"],
        )
        .register_with_aliases(
            Arc::new(
                super::count_lines::CountLines::new(workspace)
                    .with_extra_roots(extra_roots.iter().cloned())
                    .with_session_roots_opt(session_roots.clone())
                    .with_transport(shared_transport.clone()),
            ),
            &["count_lines"],
        )
        .register(Arc::new(
            super::estimate_tokens::EstimateTokens::new(workspace)
                .with_extra_roots(extra_roots.iter().cloned())
                .with_session_roots_opt(session_roots.clone()),
        ))
        .register(Arc::new(
            super::memory::Remember::new(workspace)
                .with_vector_store(memory_store.clone(), memory_embedding.clone()),
        ))
        .register(Arc::new(
            super::memory::Recall::new(workspace)
                .with_vector_store(memory_store.clone(), memory_embedding.clone()),
        ))
        .register(Arc::new(
            super::memory::Forget::new(workspace).with_vector_store(memory_store.clone()),
        ))
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
        .register(Arc::new(super::a2a::A2aTaskCheckTool::new()))
        // Host-process-executing tools (`/bin/sh` on the host, host fs
        // polling): only registered when the transport actually IS the
        // host. In container-bound registries (disable_host_exec) they
        // would be a sandbox bypass — see the doc on
        // `build_standard_tools_with_transport_opt`.
        .register_when(!disable_host_exec, || {
            vec![
                // Issue #31 §3: run_background executes via the shared
                // transport when bound (container tier), so background jobs
                // live in the sandbox and die with it. check_background only
                // reads the shared job manager — no host exec.
                Arc::new(
                    super::run_background::RunBackground::new(workspace, bg_manager.clone())
                        .with_transport(shared_transport.clone()),
                ) as Arc<dyn Tool>,
                Arc::new(super::run_background::CheckBackground::new(
                    bg_manager.clone(),
                )),
                Arc::new(super::watch_file::WatchFile::new(
                    workspace,
                    bg_manager.clone(),
                )),
                Arc::new(super::stop_loop::StopLoop::new(
                    workspace,
                    bg_manager.clone(),
                )),
            ]
        });

    // Goal #133: deliverables — declared outputs (`Present`) plus the
    // per-turn change ledger (`ChangeLedger`). Both tools and the runtime
    // share ONE ledger instance (`ToolRegistry::deliverables`), so the
    // ledger the tools write is the ledger the turn finalizes.
    //
    // Skipped in the container tier (`disable_host_exec`): the ledger
    // observes the host filesystem, which is not the environment the tools
    // actually mutate, so registering it there would report another
    // machine's state. Disabled by `RECURSIVE_DELIVERABLES=0`, and
    // fail-soft when the per-workspace data dir is unwritable.
    if !disable_host_exec && crate::deliverables::enabled_from_env() {
        match crate::deliverables::Deliverables::for_workspace(workspace) {
            Ok(ledger) => {
                let ledger = Arc::new(ledger);
                registry = registry
                    .with_deliverables(Some(ledger.clone()))
                    .register(Arc::new(super::present::PresentTool::new(
                        ledger.clone(),
                        Arc::new(crate::event::NullSink),
                    )))
                    .register(Arc::new(super::ledger::ChangeLedgerTool::new(ledger)));
            }
            Err(err) => {
                tracing::warn!(error = %err, "deliverables: unavailable for this workspace");
            }
        }
    }

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
    #[cfg(not(feature = "web_search"))]
    {
        let _ = (web_search_provider, web_search_api_key, web_search_jina_key);
    }

    if !skills.is_empty() {
        registry = registry.register(Arc::new(super::load_skill::LoadSkill::from_source(
            Arc::new(crate::skills::StaticSkillSource::new(skills.to_vec())),
        )));
    }

    // Register ACP client FS tools (S2-E20).
    // These are always registered so they appear in tools/list.
    // execute() checks AcpClientFsState to determine if the capability
    // has been declared — it returns an error when not in an ACP session
    // or when the client hasn't declared the capability.
    registry = registry.register(Arc::new(
        super::client_fs::ClientReadFile::new(workspace)
            .with_extra_roots(extra_roots.iter().cloned())
            .with_session_roots_opt(session_roots.clone()),
    ));
    registry = registry.register(Arc::new(
        super::client_fs::ClientWriteFile::new(workspace)
            .with_extra_roots(extra_roots.iter().cloned())
            .with_session_roots_opt(session_roots.clone()),
    ));

    if let Some(roots) = session_roots {
        registry = registry.with_session_roots(roots);
    }
    // Issue #31: the registry must own the SAME manager the background
    // tools were built with, so `destroy_environment` drains the manager
    // the jobs actually live in (not the empty one `ToolRegistry::new`
    // allocated).
    registry.bg_manager = bg_manager;
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
        fn kind(&self) -> ToolKind {
            ToolKind::Read
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
        fn kind(&self) -> ToolKind {
            ToolKind::Other
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

    /// Issue #65: `retain_tools` marks the registry as explicitly filtered so
    /// `AgentRuntimeBuilder::build` can tell "dropped on purpose" from
    /// "default registry that simply never had the tool". Fresh and cloned
    /// registries carry the flag; only a `retain_tools` call sets it.
    #[test]
    fn retain_tools_marks_surface_filtered() {
        let mut reg = make_registry().register(Arc::new(ReadOnlyTool { name: "Alpha" }));
        assert!(!reg.surface_filtered(), "fresh registry is not filtered");

        let clone = reg.clone();
        assert!(
            !clone.surface_filtered(),
            "clone of unfiltered stays unfiltered"
        );

        reg.retain_tools(&["Alpha".to_string()]);
        assert!(
            reg.surface_filtered(),
            "retain_tools marks the surface filtered"
        );
        assert!(
            reg.clone().surface_filtered(),
            "the flag survives Clone (session registries inherit it)"
        );

        let mut empty_allow = make_registry().register(Arc::new(ReadOnlyTool { name: "Alpha" }));
        empty_allow.retain_tools(&[]);
        assert!(empty_allow.surface_filtered());
        assert!(
            empty_allow.names().is_empty(),
            "empty allow-list drops everything"
        );
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

    #[test]
    fn legacy_fork_still_shares_state() {
        // fork() is documented as a plain clone (Goal 394): it must keep
        // sharing the guard slot, not silently grow isolation semantics.
        let tmp = tempfile::tempdir().expect("tempdir");
        let reg = build_standard_tools(tmp.path(), &[], 30);
        let legacy = reg.fork();
        assert!(
            Arc::ptr_eq(
                reg.read_file_state().as_ref().expect("parent slot"),
                &legacy.read_file_state().expect("legacy slot"),
            ),
            "fork() is a clone: it must keep sharing the read-state slot"
        );
    }

    // --- fork_session (Goal 394): per-session tool state isolation ---

    #[test]
    fn fork_session_allocates_fresh_state_slots() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let reg = build_standard_tools(tmp.path(), &[], 30);
        let a = reg.fork_session();
        let b = reg.fork_session();

        let parent = reg.read_file_state().expect("parent slot");
        let sa = a.read_file_state().expect("fork A slot");
        let sb = b.read_file_state().expect("fork B slot");
        assert!(
            !Arc::ptr_eq(&parent, &sa),
            "fork A must not alias the parent guard"
        );
        assert!(
            !Arc::ptr_eq(&parent, &sb),
            "fork B must not alias the parent guard"
        );
        assert!(!Arc::ptr_eq(&sa, &sb), "forks must not share a guard slot");
    }

    #[tokio::test]
    async fn fork_session_isolates_read_guard_between_forks() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("note.txt");
        std::fs::write(&file, "note body\n").expect("fixture");
        let reg = build_standard_tools(tmp.path(), &[], 30);
        let a = reg.fork_session();
        let b = reg.fork_session();

        // Fork A reads the file — recorded in A's guard slot only.
        a.invoke("Read", serde_json::json!({"path": "note.txt"}))
            .await
            .expect("read via fork A");

        let a_slot = a.read_file_state().expect("fork A slot");
        assert!(
            a_slot
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&file)
                .is_some(),
            "fork A must record its own read"
        );
        let b_slot = b.read_file_state().expect("fork B slot");
        assert!(
            b_slot
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&file)
                .is_none(),
            "fork B must not observe fork A's reads"
        );
    }

    #[tokio::test]
    async fn fork_session_read_guard_is_enforced_within_and_isolated_across() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("target.txt");
        std::fs::write(&file, "hello\n").expect("fixture");
        let reg = build_standard_tools(tmp.path(), &[], 30);
        let a = reg.fork_session();
        let b = reg.fork_session();

        // A reads → A may edit (the guard chain survives inside the fork).
        a.invoke("Read", serde_json::json!({"path": "target.txt"}))
            .await
            .expect("A reads");
        let a_edit = a
            .invoke(
                "Edit",
                serde_json::json!({
                    "file_path": "target.txt",
                    "old_string": "hello",
                    "new_string": "hi"
                }),
            )
            .await;
        assert!(a_edit.is_ok(), "A edited after reading: {a_edit:?}");

        // B never read the file in its own registry → the guard must deny
        // the edit even though A did read (per-session isolation).
        std::fs::write(&file, "hello\n").expect("reset fixture");
        let b_edit = b
            .invoke(
                "Edit",
                serde_json::json!({
                    "file_path": "target.txt",
                    "old_string": "hello",
                    "new_string": "bonjour"
                }),
            )
            .await;
        assert!(b_edit.is_err(), "B must not inherit A's read: {b_edit:?}");

        // After B reads on its own, the guard allows the edit — isolation
        // must not have disabled the guard.
        b.invoke("Read", serde_json::json!({"path": "target.txt"}))
            .await
            .expect("B reads");
        let b_edit2 = b
            .invoke(
                "Edit",
                serde_json::json!({
                    "file_path": "target.txt",
                    "old_string": "hello",
                    "new_string": "bonjour"
                }),
            )
            .await;
        assert!(b_edit2.is_ok(), "B edited after its own read: {b_edit2:?}");
    }

    #[tokio::test]
    async fn clone_shares_read_state_but_fork_session_snapshots_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let reg = build_standard_tools(tmp.path(), &[], 30);
        let file = tmp.path().join("shared.txt");
        std::fs::write(&file, "x\n").expect("fixture");
        let later = tmp.path().join("later.txt");
        std::fs::write(&later, "later\n").expect("fixture");

        reg.invoke("Read", serde_json::json!({"path": "shared.txt"}))
            .await
            .expect("parent reads");

        // clone() aliases the slot → the clone sees the parent's read…
        let cloned = reg.clone();
        let c_slot = cloned.read_file_state().expect("clone slot");
        assert!(
            Arc::ptr_eq(
                reg.read_file_state().as_ref().expect("parent slot"),
                &c_slot
            ),
            "clone must share the guard slot (legacy semantics)"
        );

        // …while fork_session() copies it at fork time (the child inherits
        // what the parent already read) into a FRESH Arc: reads made after
        // the fork stay invisible to the fork.
        let forked = reg.fork_session();
        let f_slot = forked.read_file_state().expect("fork slot");
        assert!(
            !Arc::ptr_eq(
                reg.read_file_state().as_ref().expect("parent slot"),
                &f_slot
            ),
            "fork_session must allocate a fresh guard slot"
        );
        assert!(
            f_slot
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&file)
                .is_some(),
            "fork inherits the parent's pre-fork reads"
        );
        reg.invoke("Read", serde_json::json!({"path": "later.txt"}))
            .await
            .expect("parent reads again");
        assert!(
            f_slot
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&later)
                .is_none(),
            "parent's post-fork reads must not appear in the fork"
        );
    }

    #[tokio::test]
    async fn fork_session_isolates_touched_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let reg = build_standard_tools(tmp.path(), &[], 30)
            .with_touched_files(Arc::new(std::sync::Mutex::new(TouchedFiles::new())));
        let a = reg.fork_session();
        let b = reg.fork_session();

        let _ = a
            .invoke_with_audit(
                "Write",
                serde_json::json!({"path": "out.txt", "contents": "hi\n"}),
            )
            .await;

        let touched_of = |r: &ToolRegistry| {
            r.touched_files()
                .map(|s| s.lock().unwrap_or_else(|p| p.into_inner()).paths_sorted())
                .unwrap_or_default()
        };
        assert_eq!(
            touched_of(&a),
            vec!["out.txt".to_string()],
            "fork A records its own write"
        );
        assert!(
            touched_of(&b).is_empty(),
            "fork B must not see A's touched files"
        );
        assert!(
            touched_of(&reg).is_empty(),
            "parent must not see the fork's touched files"
        );
    }

    #[tokio::test]
    /// `run_background` spawns through a hard-coded `/bin/sh`, so the fixture
    /// is Unix-only — on Windows the tool honestly errors out and there is no
    /// job to check. The fork-isolation semantics under test are
    /// platform-independent.
    #[cfg(unix)]
    async fn fork_session_isolates_background_jobs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let reg = build_standard_tools(tmp.path(), &[], 30);
        let a = reg.fork_session();
        let b = reg.fork_session();

        let spawned = a
            .invoke(
                "run_background",
                serde_json::json!({"command": "sleep 0.5"}),
            )
            .await
            .expect("spawn via fork A");
        let job_id = serde_json::from_str::<serde_json::Value>(&spawned)
            .expect("run_background returns JSON")["job_id"]
            .as_str()
            .expect("job_id")
            .to_string();

        // Fork A's manager knows the job…
        let seen_a = a
            .invoke("check_background", serde_json::json!({"job_id": job_id}))
            .await
            .expect("check via fork A");
        assert!(
            !seen_a.contains("\"unknown\""),
            "fork A must see its own job: {seen_a}"
        );
        // …fork B's fresh manager does not.
        let seen_b = b
            .invoke("check_background", serde_json::json!({"job_id": job_id}))
            .await
            .expect("check via fork B");
        assert!(
            seen_b.contains("\"unknown\""),
            "fork B must not see fork A's background job: {seen_b}"
        );
    }

    #[tokio::test]
    async fn fork_session_isolates_sandbox_root_expansions() {
        use super::super::dispatch::{new_shared_sandbox_roots, AccessTier};

        let tmp = tempfile::tempdir().expect("tempdir");
        let extra = tempfile::tempdir().expect("extra tempdir");
        let slot = new_shared_sandbox_roots();
        // Mirror the CLI builder wiring: the factory hands the slot to the
        // tools; with_session_roots ALSO recovers it on the registry itself
        // so fork_session can seed fresh slots from it.
        let reg = build_standard_tools_with_roots(
            tmp.path(),
            &[],
            Some(slot.clone()),
            &[],
            30,
            None,
            None,
            None,
            None,
        )
        .with_session_roots(slot.clone());
        let early_fork = reg.fork_session(); // forked BEFORE the expansion

        let secret = extra.path().join("secret.txt");
        std::fs::write(&secret, "top secret\n").expect("fixture");

        // Expand the PARENT's roots after forking…
        slot.write()
            .unwrap_or_else(|p| p.into_inner())
            .push((extra.path().to_path_buf(), AccessTier::ReadOnly));

        // …the parent can read the out-of-workspace file…
        let parent_read = reg
            .invoke(
                "Read",
                serde_json::json!({"path": secret.to_string_lossy()}),
            )
            .await;
        assert!(
            parent_read.is_ok(),
            "parent sees its own expansion: {parent_read:?}"
        );

        // …but the pre-expansion fork must not (fresh slot seeded at fork).
        let fork_read = early_fork
            .invoke(
                "Read",
                serde_json::json!({"path": secret.to_string_lossy()}),
            )
            .await;
        assert!(
            fork_read.is_err(),
            "fork must not inherit post-fork expansions: {fork_read:?}"
        );

        // A fork taken AFTER the expansion is seeded with it (fork
        // semantics: the child inherits grants made before it was forked).
        let late_fork = reg.fork_session();
        let late_read = late_fork
            .invoke(
                "Read",
                serde_json::json!({"path": secret.to_string_lossy()}),
            )
            .await;
        assert!(
            late_read.is_ok(),
            "late fork inherits pre-fork grants: {late_read:?}"
        );
        assert!(
            !Arc::ptr_eq(
                &reg.session_roots().expect("parent slot"),
                &late_fork.session_roots().expect("late fork slot"),
            ),
            "session_roots must be re-allocated per fork"
        );
    }

    // --- fork_session: deliverables ledger isolation (Goal #133) ---

    /// A registry carrying a ledger plus the two tools bound to it.
    fn ledger_registry(
        tmp: &tempfile::TempDir,
    ) -> (ToolRegistry, Arc<crate::deliverables::Deliverables>) {
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).expect("workspace");
        let ledger = Arc::new(
            crate::deliverables::Deliverables::new(
                &ws,
                tmp.path().join("private"),
                crate::deliverables::Budgets::default(),
            )
            .expect("ledger"),
        );
        let reg = make_registry()
            .with_deliverables(Some(ledger.clone()))
            .register(Arc::new(crate::tools::PresentTool::new(
                ledger.clone(),
                Arc::new(crate::event::NullSink),
            )))
            .register(Arc::new(crate::tools::ChangeLedgerTool::new(
                ledger.clone(),
            )));
        (reg, ledger)
    }

    #[tokio::test]
    async fn fork_session_rebinds_the_deliverables_tools_to_the_fork_ledger() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (reg, parent) = ledger_registry(&tmp);
        let fork = reg.fork_session();
        let fork_ledger = fork.deliverables().expect("fork ledger");
        assert!(
            !Arc::ptr_eq(&parent, &fork_ledger),
            "a fork allocates its own ledger"
        );

        // The parent's turn is armed and has changes…
        let ws = parent.workspace().to_path_buf();
        parent.begin_turn(1);
        parent.ensure_baseline().expect("parent baseline");
        std::fs::write(ws.join("parent.txt"), "parent\n").expect("fixture");

        // …which the fork must not render. Without `fork_box` on
        // `ChangeLedgerTool` the tool would still hold the parent's `Arc` and
        // report the parent's ledger.
        let out = fork
            .invoke("ChangeLedger", serde_json::json!({}))
            .await
            .expect("ChangeLedger");
        assert!(out.contains("no change ledger"), "{out}");

        // Declarations land on the fork's ledger only.
        std::fs::write(ws.join("out.txt"), "x\n").expect("fixture");
        fork_ledger.begin_turn(1);
        fork.invoke("Present", serde_json::json!({"files": ["out.txt"]}))
            .await
            .expect("Present");
        assert_eq!(fork_ledger.presented().len(), 1);
        assert!(
            parent.presented().is_empty(),
            "a fork's declaration must not leak into the parent ledger"
        );
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

    #[test]
    fn standard_tool_schemas_have_no_top_level_combinators() {
        // issue #15: Anthropic's Messages API rejects oneOf/allOf/anyOf at the
        // top level of input_schema with HTTP 400 — on the FIRST request, so a
        // single bad tool spec takes down every Anthropic-protocol run. This
        // guards the canonical tool set every entry point ships.
        let tmp = tempfile::tempdir().expect("tempdir");
        let registry = build_standard_tools(tmp.path(), &[], 30);
        for spec in registry.specs() {
            for key in ["oneOf", "allOf", "anyOf"] {
                assert!(
                    spec.parameters.get(key).is_none(),
                    "tool `{}` has top-level `{}` in its schema — Anthropic rejects it with HTTP 400",
                    spec.name,
                    key
                );
            }
        }
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
        fn kind(&self) -> ToolKind {
            ToolKind::Other
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

    // ── Sprint 3: ToolKind and build_kind_map ──────────────────────────────

    struct DefaultKindTool;

    #[async_trait]
    impl Tool for DefaultKindTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "DefaultKind".into(),
                description: "defaults to Other".into(),
                parameters: serde_json::json!({"type":"object","properties":{}}),
            }
        }
        async fn execute(&self, _args: Value) -> crate::error::Result<String> {
            Ok("ok".into())
        }
    }

    #[test]
    fn default_kind_is_other() {
        let tool = DefaultKindTool;
        assert_eq!(tool.kind(), ToolKind::Other);
    }

    #[test]
    fn build_kind_map_works() {
        let reg = make_registry()
            .register(Arc::new(ReadOnlyTool { name: "ReadTool" }))
            .register(Arc::new(MutatingTool { name: "MutTool" }));
        let map = reg.build_kind_map();
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("ReadTool"), Some(&ToolKind::Read));
        assert_eq!(map.get("MutTool"), Some(&ToolKind::Other));
    }

    #[test]
    fn build_kind_map_empty_registry() {
        let map = make_registry().build_kind_map();
        assert!(map.is_empty());
    }

    /// Issue #104: a native tool reports no MCP server, so `mcp_tools`
    /// excludes it.
    #[test]
    fn mcp_tools_excludes_native_tools() {
        let reg = make_registry().register(Arc::new(ReadOnlyTool { name: "ReadTool" }));
        assert!(reg.mcp_tools().is_empty());
    }

    /// Issue #104: a proxied MCP tool is reattachable after a registry
    /// rebuild (the container sandbox tier builds a fresh registry).
    #[test]
    fn mcp_tools_collects_proxied_tools() {
        struct McpLikeTool {
            name: &'static str,
            server: &'static str,
        }
        #[async_trait]
        impl Tool for McpLikeTool {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: self.name.to_string(),
                    description: "mcp proxied test tool".into(),
                    parameters: serde_json::json!({"type":"object","properties":{}}),
                }
            }
            fn mcp_server_name(&self) -> Option<&str> {
                Some(self.server)
            }
            async fn execute(&self, _args: Value) -> crate::error::Result<String> {
                Ok("ok".into())
            }
        }

        let reg = make_registry()
            .register(Arc::new(ReadOnlyTool { name: "ReadTool" }))
            .register(Arc::new(McpLikeTool {
                name: "mcp__gh__search",
                server: "gh",
            }));
        let mcp = reg.mcp_tools();
        assert_eq!(mcp.len(), 1);
        assert_eq!(mcp[0].spec().name, "mcp__gh__search");
        assert_eq!(mcp[0].mcp_server_name(), Some("gh"));

        // Re-attaching the collected tools reconstructs MCP routing.
        let rebuilt = make_registry().register(mcp.into_iter().next().unwrap());
        assert!(rebuilt.find_by_name("mcp__gh__search").is_some());
    }
}
