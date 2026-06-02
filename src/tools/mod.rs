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

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::permissions::{Permission, PermissionsConfig};

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
    /// replay at any time. Examples: `read_file`, `search_files`,
    /// `recall`, `checkpoint_list`.
    ReadOnly,
    /// Modifies local state (filesystem, scratchpad) in an idempotent-
    /// friendly way. Examples: `write_file`, `apply_patch`, `remember`.
    Mutating,
    /// Reaches out to the external world or triggers opaque side-effects.
    /// Cannot determine safe re-execution from local state alone.
    /// Examples: `run_shell`, `sub_agent`, `schedule_wakeup`.
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
pub mod apply_patch;
pub mod checkpoint;
pub mod episodic_recall;
pub mod estimate_tokens;
pub mod facts;
pub mod fs;
pub mod load_skill;
pub mod memory;
pub mod plan_mode;
pub mod run_background;
pub mod run_skill_script;
pub mod schedule_wakeup;
pub mod search;
pub mod send_message;
pub mod shell;
pub mod spawn_worker;
pub mod sub_agent;
pub mod team_manage;
pub mod todo;
pub mod transport;
#[cfg(feature = "web_fetch")]
pub mod web_fetch;

pub use a2a::{A2aCallTool, A2aCardTool, A2aTaskCheckTool};
pub use apply_patch::ApplyPatch;
pub use checkpoint::{build_checkpoint_tools, CheckpointDiff, CheckpointList, CheckpointToolCtx};
pub use episodic_recall::{episodic_recall_summary, EpisodicRecall};
pub use estimate_tokens::EstimateTokens;
pub use facts::{
    facts_path, facts_summary, load_facts, search_facts, Fact, FactStore, ForgetFact, RecallFact,
    RememberFact, ScoredFact, UpdateFact,
};
pub use fs::{ListDir, ReadFile, WriteFile};
pub use load_skill::LoadSkill;
pub use memory::{
    load_scratchpad, scratchpad_path, scratchpad_summary, Scratchpad, ScratchpadDelete,
    ScratchpadGet, ScratchpadList, WorkingMemoryTool,
};
pub use memory::{Forget, Recall, Remember};
pub use plan_mode::{EnterPlanModeTool, ExitPlanModeTool, PlanApprovalGate, PlanApprovalResult};
pub use run_background::{BackgroundJobManager, CheckBackground, Job, JobState, RunBackground};
pub use run_skill_script::RunSkillScript;
pub use schedule_wakeup::{ScheduleWakeup, WakeupRequest, WakeupSlot};
pub use search::SearchFiles;
pub use send_message::{SendMessageTool, WorkerMailbox, WorkerRegistry};
pub use shell::RunShell;
pub use spawn_worker::{SpawnWorkerTool, WorkerType};
pub use sub_agent::SubAgent;
pub use team_manage::{TeamAddRole, TeamListRoles, TeamRemoveRole};
pub use todo::{TodoItem, TodoStatus, TodoWriteTool};
pub use transport::{DirEntry, ExecResult, LocalTransport, ReadResult, ToolTransport};
#[cfg(feature = "web_fetch")]
pub use web_fetch::WebFetch;

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
/// every tool invocation before it runs. Return `true` to allow or
/// `false` to deny. When no hook is registered, all tools are allowed.
#[async_trait]
pub trait PermissionHook: Send + Sync {
    /// Called before every tool dispatch.
    async fn ask_permission(&self, tool_name: &str, args_preview: &str) -> bool;
}

#[derive(Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    transport: Arc<dyn ToolTransport>,
    permissions: Option<PermissionsConfig>,
    touched: Option<Arc<Mutex<TouchedFiles>>>,
    /// Goal-161: optional runtime permission hook. When `Some`, called
    /// before every tool invocation. `None` means allow all (backward-
    /// compatible default).
    permission_hook: Option<Arc<dyn PermissionHook>>,
}

/// Observer that records files touched by structured filesystem tools
/// during a single agent turn. Owned by `AgentRuntime` and reset at
/// every turn boundary; passed by `Arc<Mutex<...>>` to the
/// `ToolRegistry` so tool dispatch can record `path` arguments.
#[derive(Debug, Default, Clone)]
pub struct TouchedFiles {
    /// Workspace-relative file paths recorded from `write_file`,
    /// `apply_patch`, etc.
    pub paths: HashSet<String>,
    /// True if the agent invoked `run_shell` this turn — runtime will
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
        "write_file" => {
            if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
                t.paths.insert(p.to_string());
            }
        }
        "apply_patch" => {
            // V4A patch headers carry the file paths. The agent passes
            // the patch as a single string under "patch" or "input".
            let body = args
                .get("patch")
                .or_else(|| args.get("input"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            for line in body.lines() {
                for prefix in ["*** Update File: ", "*** Add File: ", "*** Delete File: "] {
                    if let Some(rest) = line.strip_prefix(prefix) {
                        t.paths.insert(rest.trim().to_string());
                    }
                }
            }
        }
        "run_shell" => {
            t.saw_shell = true;
        }
        _ => {}
    }
}

impl ToolRegistry {
    pub fn new(transport: Arc<dyn ToolTransport>) -> Self {
        Self {
            tools: BTreeMap::new(),
            transport,
            permissions: None,
            touched: None,
            permission_hook: None,
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
            transport: self.transport.clone(),
            permissions: self.permissions.clone(),
            touched: self.touched.clone(),
            permission_hook: self.permission_hook.clone(),
        }
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

    /// Set the permissions configuration for this registry.
    pub fn with_permissions(mut self, permissions: PermissionsConfig) -> Self {
        self.permissions = Some(permissions);
        self
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

    pub fn register(mut self, tool: Arc<dyn Tool>) -> Self {
        let name = tool.spec().name;
        self.tools.insert(name, tool);
        self
    }

    /// Register a tool via mutable reference (for use with shared registries).
    pub fn register_mut(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.spec().name;
        self.tools.insert(name, tool);
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec()).collect()
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
        if let Some(hook) = &self.permission_hook {
            let preview = args_preview_for_permission(&arguments);
            if !hook.ask_permission(name, &preview).await {
                return Err(Error::PermissionDenied { name: name.into() });
            }
        }
        self.invoke_with_audit(name, arguments).await.result
    }

    /// Invoke a tool and return both its result and a populated
    /// [`AuditMeta`]. Callers that need to persist audit data should
    /// use this method; callers that don't can call `invoke` which
    /// discards the audit half.
    pub async fn invoke_with_audit(&self, name: &str, arguments: Value) -> ToolDispatch {
        // Static permission check before any tool execution.
        if let Some(ref config) = self.permissions {
            match config.check_static(name) {
                Permission::Denied(_reason) => {
                    return ToolDispatch {
                        result: Err(Error::PermissionDenied { name: name.into() }),
                        audit: AuditMeta::synthetic_unknown_tool(name),
                    };
                }
                Permission::Allowed => {}
            }
        }

        // Record touched files for the active turn (if a collector is attached).
        if let Some(slot) = &self.touched {
            record_touched(name, &arguments, slot);
        }

        let Some(tool) = self.get(name) else {
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
fn args_preview_for_permission(arguments: &Value) -> String {
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
/// Both the root and the candidate are normalised to an absolute, dot-free
/// form before comparison so that `--workspace .` works exactly the same as
/// `--workspace /abs/path`.
pub(crate) fn resolve_within(root: &std::path::Path, path: &str) -> Result<std::path::PathBuf> {
    let candidate = std::path::Path::new(path);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    let abs_root = absolutise(root);
    let abs_joined = absolutise(&joined);
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
    let mut registry = ToolRegistry::local()
        .register(Arc::new(ReadFile::new(workspace)))
        .register(Arc::new(WriteFile::new(workspace)))
        .register(Arc::new(ApplyPatch::new(workspace)))
        .register(Arc::new(ListDir::new(workspace)))
        .register(Arc::new(
            RunShell::new(workspace)
                .with_timeout(std::time::Duration::from_secs(shell_timeout_secs)),
        ))
        .register(Arc::new(SearchFiles::new(workspace)))
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

    // Goal-165: plan mode 2.0 tools (NullSink / default gate placeholder).
    // AgentRuntimeBuilder::build() re-registers these with the real gate and sink.
    let default_gate = Arc::new(plan_mode::PlanApprovalGate::new());
    registry = registry
        .register(Arc::new(plan_mode::EnterPlanModeTool::new(
            default_gate.clone(),
        )))
        .register(Arc::new(plan_mode::ExitPlanModeTool::new(
            default_gate,
            Arc::new(crate::event::NullSink),
        )));

    #[cfg(feature = "web_fetch")]
    {
        registry = registry.register(Arc::new(WebFetch::new()));
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
    use crate::permissions::PermissionsConfig;
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
        let config = PermissionsConfig {
            allow: vec!["echo".into()],
            deny: vec!["echo".into()],
            interactive: vec![],
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
        async fn ask_permission(&self, _name: &str, _args: &str) -> bool {
            true
        }
    }

    #[async_trait]
    impl PermissionHook for DenyHook {
        async fn ask_permission(&self, _name: &str, _args: &str) -> bool {
            false
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
}
