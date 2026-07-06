//! External process-based hooks.
//!
//! External hooks are executable scripts/programs placed in hook directories
//! (`~/.recursive/hooks/` or `<workspace>/.recursive/hooks/`). They receive
//! a JSON event on stdin and must reply with a JSON decision on stdout within
//! 5 seconds. Timeout or non-parseable output is treated as "continue".
//!
//! # Protocol
//!
//! **Input** (stdin, single line JSON):
//! ```json
//! {
//!   "event": "preToolCall",
//!   "toolName": "Bash",
//!   "args": {"command": "rm -rf /"},
//!   "mode": "ask"
//! }
//! ```
//!
//! **Output** (stdout, single line JSON):
//! ```json
//! {"action": "continue"}
//! {"action": "skip", "message": "dangerous command"}
//! {"action": "error", "message": "not allowed"}
//! ```

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crate::error::{Error, Result};
use crate::event::AgentEvent;
use crate::hooks::config::{matches_hook, HookCommand, HookCommandType, HookFailMode, HooksConfig};
use crate::llm::ChatProvider;
use crate::message::Message;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

pub use crate::hooks::HookAction;

/// Time limit for a single external hook to respond.
/// Can be overridden via `RECURSIVE_HOOK_TIMEOUT_SECS` for slow CI / test
/// environments — e.g. `RECURSIVE_HOOK_TIMEOUT_SECS=30`.
const HOOK_TIMEOUT: Duration = Duration::from_secs(5);

fn default_hook_timeout() -> Duration {
    std::env::var("RECURSIVE_HOOK_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(HOOK_TIMEOUT)
}

// ── JSON protocol types ────────────────────────────────────────────

/// The kind of lifecycle event sent to the external hook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HookEvent {
    // existing
    PreToolCall,
    PostToolCall,
    PermissionRequest,
    // new in Goal 204
    PostToolCallFailure,
    PermissionDenied,
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    Stop,
    SubagentStart,
    SubagentStop,
    Notification,
    Setup,
}

/// Input payload sent to the external hook on stdin.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookInput {
    pub event: HookEvent,
    /// Tool name — `None` for non-tool events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// Tool arguments — `None` for non-tool events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<serde_json::Value>,
    pub mode: String,
    // optional context fields for specific events
    /// User's input content (UserPromptSubmit).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Notification message (Notification).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Nesting depth (SubagentStart / SubagentStop).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depth: Option<usize>,
    /// Denial reason (PermissionDenied).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Error message (PostToolCallFailure).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Action returned by the external hook, as deserialized from JSON.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
enum JsonAction {
    Continue,
    Skip,
    Error,
}

/// Output payload expected from the external hook on stdout.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct HookOutput {
    #[serde(default)]
    action: Option<JsonAction>,
    #[serde(default)]
    message: Option<String>,

    // ── Goal 205: extended output fields ──────────────────────────
    /// Append to the next LLM system prompt.
    #[serde(default)]
    additional_context: Option<String>,
    /// Override tool arguments (PreToolCall only).
    #[serde(default)]
    updated_input: Option<serde_json::Value>,
    /// Warning message shown to the user (via AgentEvent).
    #[serde(default)]
    system_message: Option<String>,
    /// When true, suppress writing hook stdout to the transcript.
    #[serde(default)]
    suppress_output: bool,
    /// Permission decision: "allow" / "deny" / "passthrough".
    #[serde(default)]
    permission_decision: Option<String>,
    /// Reason for the permission decision (audit log only).
    #[serde(default)]
    permission_decision_reason: Option<String>,
}

/// Permission decision returned by a hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    Deny,
    Passthrough,
}

impl PermissionDecision {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "allow" => Some(Self::Allow),
            "deny" => Some(Self::Deny),
            "passthrough" => Some(Self::Passthrough),
            _ => None,
        }
    }
}

/// Rich result returned by a hook dispatch, containing the primary action
/// as well as optional structured metadata for the caller to consume.
#[derive(Debug, Clone)]
pub struct HookResult {
    /// Primary decision.
    pub action: HookAction,
    /// Extra context to inject into the next LLM system prompt.
    pub additional_context: Option<String>,
    /// Override tool arguments (meaningful only for PreToolCall).
    pub updated_input: Option<serde_json::Value>,
    /// User-facing warning message.
    pub system_message: Option<String>,
    /// When `true`, suppress the hook's raw stdout from the transcript.
    pub suppress_output: bool,
    /// Explicit permission decision (overrides normal permission check).
    pub permission_decision: Option<PermissionDecision>,
    /// Reason for the permission decision (for audit logging).
    pub permission_decision_reason: Option<String>,
}

impl Default for HookResult {
    fn default() -> Self {
        Self {
            action: HookAction::Continue,
            additional_context: None,
            updated_input: None,
            system_message: None,
            suppress_output: false,
            permission_decision: None,
            permission_decision_reason: None,
        }
    }
}

impl HookResult {
    /// Shorthand for the default "do nothing" result.
    pub fn continue_default() -> Self {
        Self::default()
    }

    /// Produce a hook result based on the fail mode.
    ///
    /// - `Open` (default): returns `Continue` — preserves pre-Goal-281
    ///   fail-open behavior for notification hooks.
    /// - `Closed`: for gating events (`PreToolCall`, `UserPromptSubmit`,
    ///   `PermissionRequest`) returns `Error(reason)` so the agent sees
    ///   the failure as a rejection. For all other event types
    ///   (notification hooks like `PostToolCall`, `SessionStart`, etc.)
    ///   returns `Continue` since a notification failure shouldn't block.
    pub fn from_fail_mode(mode: HookFailMode, reason: &str, event: &HookEvent) -> Self {
        match mode {
            HookFailMode::Open => Self::continue_default(),
            HookFailMode::Closed => {
                let action = match event {
                    HookEvent::PreToolCall
                    | HookEvent::UserPromptSubmit
                    | HookEvent::PermissionRequest => HookAction::Error(reason.to_string()),
                    _ => HookAction::Continue,
                };
                Self {
                    action,
                    ..Self::default()
                }
            }
        }
    }
}

impl HookOutput {
    /// Convert the external hook's JSON output into a `HookResult`.
    fn into_hook_result(self) -> HookResult {
        let action = match self.action.unwrap_or(JsonAction::Continue) {
            JsonAction::Continue => HookAction::Continue,
            JsonAction::Skip => HookAction::Skip,
            JsonAction::Error => HookAction::Error(
                self.message
                    .clone()
                    .unwrap_or_else(|| "external hook blocked".to_string()),
            ),
        };
        let permission_decision = self
            .permission_decision
            .as_deref()
            .and_then(PermissionDecision::from_str);
        HookResult {
            action,
            additional_context: self.additional_context,
            updated_input: self.updated_input,
            system_message: self.system_message,
            suppress_output: self.suppress_output,
            permission_decision,
            permission_decision_reason: self.permission_decision_reason,
        }
    }
}

// ── Resolved hook entry ────────────────────────────────────────────

/// What kind of execution a resolved hook uses.
#[derive(Clone, Debug)]
enum ResolvedHookKind {
    /// Local executable (binary or shell script) with optional arguments.
    ///
    /// The command field in `HooksConfig` is split via `shell_words::split`
    /// so users can write `"/path/to/hook arg1 arg2"` and get proper quoting.
    Command(PathBuf, Vec<String>),
    /// HTTP POST to a remote endpoint.
    Http {
        url: String,
        headers: Option<std::collections::HashMap<String, String>>,
        allowed_env_vars: Option<Vec<String>>,
    },
    /// LLM prompt evaluation — the prompt template is evaluated by an LLM.
    Prompt {
        /// Template string; `$ARGUMENTS` is replaced with serialised `HookInput`.
        prompt: String,
    },
}

/// A single resolved hook entry (command path + metadata from config).
#[derive(Clone, Debug)]
struct ResolvedHook {
    /// How to execute this hook.
    kind: ResolvedHookKind,
    /// Per-hook timeout override (None = use global HOOK_TIMEOUT).
    timeout_secs: Option<u64>,
    /// Event name this hook is registered for (None = all events).
    event_name: Option<String>,
    /// Tool/arg filter pattern (None = all tools).
    matcher: Option<String>,
    /// When `true`, run once and then mark as executed.
    once: bool,
    /// When `true`, run in background — Agent continues immediately.
    r#async: bool,
    /// When `true`, run in background and cancel Agent on exit code 2.
    async_rewake: bool,
    /// Fail behavior on timeout / error / non-zero exit.
    fail_mode: HookFailMode,
}

// ── Runner ─────────────────────────────────────────────────────────

// ── Runner ─────────────────────────────────────────────────────────

/// Discovers and runs external hook executables.
///
/// External hooks are scanned from one or more directories. Each
/// executable file is treated as a hook. When `dispatch` is called,
/// the runner sends the event to each hook in order and returns the
/// first non-`Continue` decision. Hooks that timeout or return
/// invalid output are treated as `Continue` (fail-open).
#[derive(Clone)]
pub struct ExternalHookRunner {
    hooks: Vec<ResolvedHook>,
    /// Optional LLM provider for prompt-type hooks.
    llm: Option<Arc<dyn ChatProvider>>,
    /// Tracks indices of `once: true` hooks that have already been executed.
    executed_once: Arc<Mutex<HashSet<usize>>>,
    /// Optional cancellation token for `asyncRewake` hooks.
    /// Test-only: pre-canned `HookResult`s indexed by hook position.
    /// When set, `run_hook` returns the mock result instead of spawning a process.
    #[cfg(test)]
    mock_results: Vec<Option<HookResult>>,
    /// Test-only: pre-canned exit codes for `asyncRewake` path.
    /// When set, `run_command_exit_code` returns the mock exit code instead of spawning.
    #[cfg(test)]
    mock_exit_codes: Vec<Option<i32>>,
    pub cancel_token: Option<CancellationToken>,
    /// Optional event channel for TUI progress events.
    event_tx: Option<mpsc::UnboundedSender<AgentEvent>>,
}

impl ExternalHookRunner {
    /// Scan the given directories and collect all executable files.
    ///
    /// This is the legacy discovery mode: every executable in the directory
    /// is treated as a hook that fires for all events.
    pub fn discover(dirs: &[PathBuf]) -> Self {
        let mut hooks = Vec::new();
        for dir in dirs {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if is_executable(&path) {
                        hooks.push(ResolvedHook {
                            kind: ResolvedHookKind::Command(path, vec![]),
                            timeout_secs: None,
                            event_name: None,
                            matcher: None,
                            once: false,
                            r#async: false,
                            async_rewake: false,
                            fail_mode: HookFailMode::Open,
                        });
                    }
                }
            }
        }
        #[cfg(not(test))]
        return Self {
            hooks,
            llm: None,
            executed_once: Arc::new(Mutex::new(HashSet::new())),
            cancel_token: None,
            event_tx: None,
        };
        #[cfg(test)]
        Self {
            hooks,
            llm: None,
            executed_once: Arc::new(Mutex::new(HashSet::new())),
            cancel_token: None,
            event_tx: None,
            mock_results: vec![],
            mock_exit_codes: vec![],
        }
    }

    /// Build a runner from a structured `HooksConfig`.
    ///
    /// Supports `command` and `http` types; `prompt` / `agent` require an LLM
    /// provider and are handled by [`from_config_with_llm`].
    pub fn from_config(config: HooksConfig) -> Self {
        Self::from_config_with_llm(config, None)
    }

    /// Build a runner from a structured `HooksConfig`, with an optional LLM
    /// provider for prompt/agent-type hooks.
    pub fn from_config_with_llm(config: HooksConfig, llm: Option<Arc<dyn ChatProvider>>) -> Self {
        let mut hooks = Vec::new();
        for (event_name, matchers) in config.events {
            for matcher_entry in matchers {
                for cmd in &matcher_entry.hooks {
                    if let Some(resolved) = Self::resolve_command(
                        cmd,
                        Some(event_name.clone()),
                        matcher_entry.matcher.clone(),
                    ) {
                        hooks.push(resolved);
                    }
                }
            }
        }
        #[cfg(not(test))]
        return Self {
            hooks,
            llm,
            executed_once: Arc::new(Mutex::new(HashSet::new())),
            cancel_token: None,
            event_tx: None,
        };
        #[cfg(test)]
        Self {
            hooks,
            llm,
            executed_once: Arc::new(Mutex::new(HashSet::new())),
            cancel_token: None,
            event_tx: None,
            mock_results: vec![],
            mock_exit_codes: vec![],
        }
    }

    /// Attach a `CancellationToken` for `asyncRewake` hooks.
    pub fn with_cancel_token(mut self, token: CancellationToken) -> Self {
        self.cancel_token = Some(token);
        self
    }

    /// Attach an `AgentEvent` channel for TUI progress events.
    pub fn with_event_tx(mut self, tx: mpsc::UnboundedSender<AgentEvent>) -> Self {
        self.event_tx = Some(tx);
        self
    }

    /// Inject canned `HookResult`s for tests that must not spawn real processes.
    ///
    /// `results[i]` is the response returned for hook at index `i`.
    /// `None` means "run the real hook" (useful to test a mix of mocked and real).
    /// When the index is beyond the `results` slice the real hook is executed.
    ///
    /// This replaces process execution entirely — the hook's command path is
    /// ignored. Use this to test dispatch logic (event filtering, short-circuit,
    /// once, async rewake) without any OS process creation.
    #[cfg(test)]
    fn with_mock_results(mut self, results: Vec<Option<HookResult>>) -> Self {
        self.mock_results = results;
        self
    }

    /// Inject pre-canned exit codes for `asyncRewake` path tests.
    ///
    /// Indexed by hook position; `None` means "exit 0 (no cancellation)".
    #[cfg(test)]
    fn with_mock_exit_codes(mut self, codes: Vec<Option<i32>>) -> Self {
        self.mock_exit_codes = codes;
        self
    }

    /// Emit an `AgentEvent` if an event channel is registered.
    fn emit(&self, event: AgentEvent) {
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(event);
        }
    }

    /// Convert a single `HookCommand` entry to a `ResolvedHook`.
    ///
    /// Returns `None` for types that need an LLM (prompt/agent) or missing fields.
    fn resolve_command(
        cmd: &HookCommand,
        event_name: Option<String>,
        matcher: Option<String>,
    ) -> Option<ResolvedHook> {
        let base = |kind: ResolvedHookKind| ResolvedHook {
            kind,
            timeout_secs: Some(cmd.timeout),
            event_name,
            matcher,
            once: cmd.once,
            r#async: cmd.r#async,
            async_rewake: cmd.async_rewake,
            fail_mode: cmd.fail_mode,
        };

        match cmd.r#type {
            HookCommandType::Command => {
                let command_str = cmd.command.as_deref()?;
                // Parse the command string with shell-quoting rules so that
                // commands like `/path/to/hook '{"action":"skip"}'` split
                // correctly into (program, args).
                let mut parts = shell_words::split(command_str)
                    .unwrap_or_else(|_| vec![command_str.to_string()]);
                if parts.is_empty() {
                    return None;
                }
                let path = expand_tilde(&parts.remove(0));
                Some(base(ResolvedHookKind::Command(path, parts)))
            }
            HookCommandType::Http => {
                let url = cmd.url.clone()?;
                Some(base(ResolvedHookKind::Http {
                    url,
                    headers: None,
                    allowed_env_vars: None,
                }))
            }
            HookCommandType::Prompt | HookCommandType::Agent => {
                let prompt = cmd.prompt.clone()?;
                Some(base(ResolvedHookKind::Prompt { prompt }))
            }
        }
    }

    /// Number of registered hooks.
    pub fn len(&self) -> usize {
        self.hooks.len()
    }

    /// True when no hooks are registered.
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// Dispatch an event to all matching hooks.
    ///
    /// Returns the first non-`Continue` `HookResult`. Hooks that fail,
    /// timeout, or return unparseable output are silently skipped (fail-open).
    pub async fn dispatch(&self, input: &HookInput) -> HookResult {
        let event_str = serde_json::to_string(&input.event)
            .unwrap_or_default()
            .trim_matches('"')
            .to_string();
        let tool_name = input.tool_name.as_deref().unwrap_or("");
        let empty_args = serde_json::Value::Object(Default::default());
        let args = input.args.as_ref().unwrap_or(&empty_args);

        for (idx, hook) in self.hooks.iter().enumerate() {
            // Filter by event name (if registered for a specific event).
            if let Some(ref ev) = hook.event_name {
                if !event_names_match(ev, &event_str) {
                    continue;
                }
            }
            // Filter by tool/arg matcher.
            if !matches_hook(&hook.matcher, tool_name, args) {
                continue;
            }
            // Skip once-hooks that already ran.
            if hook.once {
                let already_ran = {
                    let guard = self.executed_once.lock().unwrap_or_else(|e| e.into_inner());
                    guard.contains(&idx)
                };
                if already_ran {
                    continue;
                }
            }

            // Async fire-and-forget: spawn and return Continue immediately.
            if hook.r#async && !hook.async_rewake {
                let hook_clone = hook.clone();
                let input_clone = input.clone();
                let self_clone = self.clone();
                tokio::spawn(async move {
                    if let Err(e) = self_clone.run_hook(&hook_clone, idx, &input_clone).await {
                        tracing::warn!("async hook error: {e}");
                    }
                });
                if hook.once {
                    self.executed_once
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(idx);
                }
                continue;
            }

            // asyncRewake: spawn, cancel Agent if hook exits with code 2.
            if hook.async_rewake {
                let hook_clone = hook.clone();
                let input_clone = input.clone();
                let self_clone = self.clone();
                let cancel = self.cancel_token.clone();
                tokio::spawn(async move {
                    let exit_code = self_clone
                        .run_command_exit_code(&hook_clone, idx, &input_clone)
                        .await
                        .unwrap_or(None);
                    if exit_code == Some(2) {
                        tracing::warn!("asyncRewake hook exited with code 2 — cancelling agent");
                        if let Some(token) = cancel {
                            token.cancel();
                        }
                    }
                });
                if hook.once {
                    self.executed_once
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(idx);
                }
                continue;
            }

            // Synchronous execution.
            let hook_display_name = match &hook.kind {
                ResolvedHookKind::Command(path, _) => path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "hook".to_string()),
                ResolvedHookKind::Http { url, .. } => url.clone(),
                ResolvedHookKind::Prompt { .. } => "prompt-hook".to_string(),
            };
            self.emit(AgentEvent::HookStarted {
                hook_event: event_str.clone(),
                hook_name: hook_display_name.clone(),
                status_message: None,
            });
            let start = std::time::Instant::now();
            if let Ok(result) = self.run_hook(hook, idx, input).await {
                if hook.once {
                    self.executed_once
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(idx);
                }
                let duration_ms = start.elapsed().as_millis() as u64;
                let outcome = match &result.action {
                    HookAction::Continue => "continue".to_string(),
                    HookAction::Skip => "skip".to_string(),
                    HookAction::Error(reason) => format!("error: {reason}"),
                };
                if let Some(msg) = &result.system_message {
                    self.emit(AgentEvent::HookSystemMessage { text: msg.clone() });
                }
                self.emit(AgentEvent::HookFinished {
                    hook_event: event_str.clone(),
                    hook_name: hook_display_name.clone(),
                    outcome,
                    duration_ms,
                });
                if !matches!(result.action, HookAction::Continue) {
                    return result;
                }
            }
        }
        HookResult::continue_default()
    }

    /// Run a single hook entry and return a `HookResult`.
    async fn run_hook(
        &self,
        hook: &ResolvedHook,
        #[cfg_attr(not(test), allow(unused_variables))] hook_idx: usize,
        input: &HookInput,
    ) -> Result<HookResult> {
        // In test builds, short-circuit with the pre-canned result if set.
        #[cfg(test)]
        if let Some(Some(result)) = self.mock_results.get(hook_idx) {
            return Ok(result.clone());
        }

        let hook_timeout = hook
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or_else(default_hook_timeout);

        match &hook.kind {
            ResolvedHookKind::Http {
                url,
                headers,
                allowed_env_vars,
            } => {
                let result = run_http_hook(
                    url,
                    input,
                    hook_timeout.as_secs(),
                    headers.as_ref(),
                    allowed_env_vars.as_deref(),
                    hook.fail_mode,
                )
                .await;
                Ok(result)
            }
            ResolvedHookKind::Prompt { prompt } => {
                if let Some(llm) = &self.llm {
                    let result =
                        run_prompt_hook(llm.as_ref(), prompt, input, hook_timeout, hook.fail_mode)
                            .await;
                    Ok(result)
                } else {
                    // No LLM configured — fail-open with a warning.
                    tracing::warn!(
                        "prompt hook configured but no LLM provider available; skipping"
                    );
                    Ok(HookResult::continue_default())
                }
            }
            ResolvedHookKind::Command(path, args) => {
                self.run_command_hook(path, args, input, hook_timeout, hook.fail_mode)
                    .await
            }
        }
    }

    /// Run a command hook and return only the exit code (for asyncRewake).
    async fn run_command_exit_code(
        &self,
        hook: &ResolvedHook,
        #[cfg_attr(not(test), allow(unused_variables))] hook_idx: usize,
        input: &HookInput,
    ) -> Result<Option<i32>> {
        // In test builds, short-circuit with the pre-canned exit code if set.
        #[cfg(test)]
        if let Some(maybe_code) = self.mock_exit_codes.get(hook_idx) {
            return Ok(*maybe_code);
        }

        let ResolvedHookKind::Command(path, args) = &hook.kind else {
            return Ok(None);
        };
        let hook_timeout = hook
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or_else(default_hook_timeout);
        let input_json = serde_json::to_string(input).map_err(|e| Error::Config {
            message: format!("hook input serialize: {e}"),
        })?;
        let mut child = Command::new(path)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| Error::Config {
                message: format!("hook spawn {}: {e}", path.display()),
            })?;
        let status = timeout(hook_timeout, async {
            use tokio::io::AsyncWriteExt;
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(input_json.as_bytes()).await;
                let _ = stdin.shutdown().await;
            }
            child.wait().await
        })
        .await;
        match status {
            Ok(Ok(s)) => Ok(s.code()),
            _ => Ok(None),
        }
    }

    /// Run a single hook command executable and return a `HookResult`.
    async fn run_command_hook(
        &self,
        path: &PathBuf,
        args: &[String],
        input: &HookInput,
        hook_timeout: Duration,
        fail_mode: HookFailMode,
    ) -> Result<HookResult> {
        let input_json = match serde_json::to_string(input) {
            Ok(j) => j,
            Err(e) => {
                return Ok(HookResult::from_fail_mode(
                    fail_mode,
                    &format!("hook input serialize: {e}"),
                    &input.event,
                ));
            }
        };

        let mut child = match Command::new(path)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                return Ok(HookResult::from_fail_mode(
                    fail_mode,
                    &format!("hook spawn {}: {e}", path.display()),
                    &input.event,
                ));
            }
        };

        // Write stdin then wait for output, respecting per-hook timeout.
        let output = timeout(hook_timeout, async {
            use tokio::io::AsyncWriteExt;
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(input_json.as_bytes()).await;
                // Close stdin so the child knows input is done.
                let _ = stdin.shutdown().await;
            }
            child.wait_with_output().await
        })
        .await;

        let output = match output {
            Err(_elapsed) => {
                return Ok(HookResult::from_fail_mode(
                    fail_mode,
                    "hook timed out",
                    &input.event,
                ));
            }
            Ok(Err(e)) => {
                return Ok(HookResult::from_fail_mode(
                    fail_mode,
                    &format!("hook wait {}: {e}", path.display()),
                    &input.event,
                ));
            }
            Ok(Ok(o)) => o,
        };

        if !output.status.success() {
            return Ok(HookResult::from_fail_mode(
                fail_mode,
                &format!("hook exited with code {:?}", output.status.code()),
                &input.event,
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        match serde_json::from_str::<HookOutput>(stdout.trim()) {
            Ok(parsed) => Ok(parsed.into_hook_result()),
            Err(e) => Ok(HookResult::from_fail_mode(
                fail_mode,
                &format!("hook output parse {}: {e}", path.display()),
                &input.event,
            )),
        }
    }
}

/// Expand a leading `~` to the home directory.
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

/// Compare event names case-insensitively (config may use PascalCase, wire uses camelCase).
fn event_names_match(config_name: &str, wire_name: &str) -> bool {
    config_name.to_lowercase() == wire_name.to_lowercase()
}

// ── Prompt hook ────────────────────────────────────────────────────

/// Evaluate a prompt template via `llm`, replacing `$ARGUMENTS` with
/// the serialised `HookInput`. Respects `fail_mode` on timeout or LLM error.
async fn run_prompt_hook(
    llm: &dyn ChatProvider,
    prompt_template: &str,
    input: &HookInput,
    hook_timeout: Duration,
    fail_mode: HookFailMode,
) -> HookResult {
    let args_json = serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string());
    let prompt = prompt_template.replace("$ARGUMENTS", &args_json);

    let messages = [Message::user(prompt)];
    let completion_future = llm.complete(&messages, &[]);
    let completion = match timeout(hook_timeout, completion_future).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            tracing::warn!("prompt hook LLM error: {e}");
            return HookResult::from_fail_mode(
                fail_mode,
                &format!("prompt hook LLM error: {e}"),
                &input.event,
            );
        }
        Err(_) => {
            tracing::warn!("prompt hook timeout");
            return HookResult::from_fail_mode(fail_mode, "prompt hook timeout", &input.event);
        }
    };

    match serde_json::from_str::<HookOutput>(completion.content.trim()) {
        Ok(output) => output.into_hook_result(),
        Err(_) => {
            // Non-JSON or empty response — use fail_mode.
            HookResult::from_fail_mode(fail_mode, "prompt hook returned non-JSON", &input.event)
        }
    }
}

// ── HTTP hook ──────────────────────────────────────────────────────

/// POST `input` as JSON to `url` and parse the response as `HookOutput`.
///
/// Respects `fail_mode`: returns `Continue` on failure when Open,
/// returns `Error(reason)` on failure when Closed (for gating events).
pub(crate) async fn run_http_hook(
    url: &str,
    input: &HookInput,
    timeout_secs: u64,
    headers: Option<&std::collections::HashMap<String, String>>,
    allowed_env_vars: Option<&[String]>,
    fail_mode: HookFailMode,
) -> HookResult {
    let client_result = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .connect_timeout(Duration::from_secs(timeout_secs))
        .build();

    let client = match client_result {
        Ok(c) => c,
        Err(_) => {
            return HookResult::from_fail_mode(
                fail_mode,
                "http hook client build failed",
                &input.event,
            );
        }
    };

    let mut builder = client.post(url).json(input);

    if let Some(headers_map) = headers {
        for (k, v) in headers_map {
            let interpolated = interpolate_env_vars(v, allowed_env_vars);
            builder = builder.header(k.as_str(), interpolated);
        }
    }

    let response = match builder.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("http hook request failed: {e}");
            return HookResult::from_fail_mode(
                fail_mode,
                &format!("http hook request failed: {e}"),
                &input.event,
            );
        }
    };

    let body = match response.text().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("http hook response read failed: {e}");
            return HookResult::from_fail_mode(
                fail_mode,
                &format!("http hook response read failed: {e}"),
                &input.event,
            );
        }
    };

    match serde_json::from_str::<HookOutput>(body.trim()) {
        Ok(output) => output.into_hook_result(),
        Err(e) => {
            tracing::warn!("http hook response parse failed: {e}");
            HookResult::from_fail_mode(
                fail_mode,
                &format!("http hook response parse failed: {e}"),
                &input.event,
            )
        }
    }
}

/// Interpolate `$VAR` and `${VAR}` patterns in a string, replacing only
/// variables in `allowed_env_vars`. Non-allowed variables are replaced with
/// empty string.
fn interpolate_env_vars(value: &str, allowed: Option<&[String]>) -> String {
    let mut result = value.to_string();

    // Replace ${VAR} form first, then $VAR form.
    for (pattern, var_name) in find_env_var_refs(&result) {
        let replacement = if is_allowed(&var_name, allowed) {
            std::env::var(&var_name).unwrap_or_default()
        } else {
            String::new()
        };
        result = result.replacen(&pattern, &replacement, 1);
    }
    result
}

/// Find all `${VAR}` and `$VAR` patterns in `s`, returning `(full_pattern, var_name)` pairs.
fn find_env_var_refs(s: &str) -> Vec<(String, String)> {
    let mut refs = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                // ${VAR} form
                if let Some(end) = s[i + 2..].find('}') {
                    let var_name = &s[i + 2..i + 2 + end];
                    refs.push((format!("${{{var_name}}}"), var_name.to_string()));
                    i += 3 + end;
                    continue;
                }
            } else {
                // $VAR form — collect alphanumeric + underscore
                let start = i + 1;
                let mut end = start;
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_')
                {
                    end += 1;
                }
                if end > start {
                    let var_name = &s[start..end];
                    refs.push((format!("${var_name}"), var_name.to_string()));
                    i = end;
                    continue;
                }
            }
        }
        i += 1;
    }
    refs
}

fn is_allowed(var: &str, allowed: Option<&[String]>) -> bool {
    let Some(list) = allowed else { return true };
    list.iter().any(|a| a == var)
}

// ── helpers ────────────────────────────────────────────────────────

/// Check whether `path` is a regular file with an executable bit set.
///
/// On Unix/macOS this checks the owner/group/world execute permission
/// bits. On Windows this function always returns `false` because the
/// concept of an executable bit doesn't exist on that platform.
fn is_executable(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.is_file()
            && std::fs::metadata(path)
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        // On non-Unix platforms we fall back to checking the extension.
        let _ = path;
        false
    }
}

// ── tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Convenience helper: build a tool-call `HookInput` for tests.
    fn make_tool_input(event: HookEvent, tool: &str, args: serde_json::Value) -> HookInput {
        HookInput {
            event,
            tool_name: Some(tool.to_string()),
            args: Some(args),
            mode: "ask".to_string(),
            content: None,
            message: None,
            depth: None,
            reason: None,
            error: None,
        }
    }

    // ── JSON parsing ────────────────────────────────────────────

    #[test]
    fn hook_output_parse_continue() {
        let json = r#"{"action":"continue"}"#;
        let out: HookOutput = serde_json::from_str(json).unwrap();
        assert!(matches!(out.action, Some(JsonAction::Continue)));
        assert!(out.message.is_none());
    }

    #[test]
    fn hook_output_parse_skip() {
        let json = r#"{"action":"skip","message":"blocked"}"#;
        let out: HookOutput = serde_json::from_str(json).unwrap();
        assert!(matches!(out.action, Some(JsonAction::Skip)));
        assert_eq!(out.message.as_deref(), Some("blocked"));
    }

    #[test]
    fn hook_output_parse_error() {
        let json = r#"{"action":"error","message":"not allowed"}"#;
        let out: HookOutput = serde_json::from_str(json).unwrap();
        assert!(matches!(out.action, Some(JsonAction::Error)));
        assert_eq!(out.message.as_deref(), Some("not allowed"));
    }

    #[test]
    fn hook_output_parse_camel_case() {
        let json = r#"{"action":"continue"}"#;
        let out: HookOutput = serde_json::from_str(json).unwrap();
        assert!(matches!(out.action, Some(JsonAction::Continue)));

        let json = r#"{"action":"skip","message":"nope"}"#;
        let out: HookOutput = serde_json::from_str(json).unwrap();
        assert!(matches!(out.action, Some(JsonAction::Skip)));
    }

    #[test]
    fn hook_output_parse_missing_message() {
        // message is optional
        let json = r#"{"action":"error"}"#;
        let out: HookOutput = serde_json::from_str(json).unwrap();
        assert!(matches!(out.action, Some(JsonAction::Error)));
        assert!(out.message.is_none());
    }

    #[test]
    fn hook_output_into_hook_action_continue() {
        let json = r#"{"action":"continue"}"#;
        let out: HookOutput = serde_json::from_str(json).unwrap();
        let result = out.into_hook_result();
        assert!(matches!(result.action, HookAction::Continue));
    }

    #[test]
    fn hook_output_into_hook_action_skip() {
        let json = r#"{"action":"skip","message":"blocked"}"#;
        let out: HookOutput = serde_json::from_str(json).unwrap();
        let result = out.into_hook_result();
        assert!(matches!(result.action, HookAction::Skip));
    }

    #[test]
    fn hook_output_into_hook_action_error_with_message() {
        let json = r#"{"action":"error","message":"not allowed"}"#;
        let out: HookOutput = serde_json::from_str(json).unwrap();
        let result = out.into_hook_result();
        assert!(matches!(result.action, HookAction::Error(ref msg) if msg == "not allowed"));
    }

    #[test]
    fn hook_output_into_hook_action_error_without_message() {
        let json = r#"{"action":"error"}"#;
        let out: HookOutput = serde_json::from_str(json).unwrap();
        let result = out.into_hook_result();
        assert!(
            matches!(result.action, HookAction::Error(ref msg) if msg == "external hook blocked")
        );
    }

    // ── Runner semantics ────────────────────────────────────────

    #[test]
    fn empty_runner_returns_continue() {
        let runner = ExternalHookRunner::discover(&[]);
        assert!(runner.is_empty());
        assert_eq!(runner.len(), 0);
    }

    #[test]
    fn discover_skips_non_executable() {
        // Create a temp dir with a non-executable file.
        let tmp = tempfile::tempdir().unwrap();
        let non_exec = tmp.path().join("script.sh");
        std::fs::write(&non_exec, "echo hello").unwrap();
        // Ensure it's not executable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&non_exec).unwrap().permissions();
            perms.set_mode(0o644);
            std::fs::set_permissions(&non_exec, perms).unwrap();
        }
        let runner = ExternalHookRunner::discover(&[tmp.path().to_path_buf()]);
        assert!(runner.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn discover_collects_executable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let exec = tmp.path().join("hook.sh");
        std::fs::write(&exec, "#!/bin/sh\necho '{\"action\":\"continue\"}'").unwrap();
        let mut perms = std::fs::metadata(&exec).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&exec, perms).unwrap();
        let runner = ExternalHookRunner::discover(&[tmp.path().to_path_buf()]);
        assert_eq!(runner.len(), 1);
    }

    #[test]
    fn is_executable_rejects_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("subdir");
        std::fs::create_dir(&dir).unwrap();
        // Even if the directory has the x bit, is_executable requires is_file().
        assert!(!is_executable(&dir));
    }

    #[test]
    fn is_executable_rejects_nonexistent() {
        assert!(!is_executable(std::path::Path::new(
            "/nonexistent/path/script"
        )));
    }

    // ── Integration tests: run a compiled hook binary ───────────
    //
    // We use the `hook_echo` compiled Rust binary (tests/bin/hook_echo.rs)
    // instead of shell scripts to avoid /bin/sh startup latency on
    // CPU-starved systems (e.g. while cargo-mutants workers hold all cores).
    // `hook_echo <json>` prints <json> to stdout and exits 0.
    // `hook_echo --exit <n>` exits with code n without output.

    #[tokio::test]
    async fn dispatch_runs_executable_hook_and_returns_decision() {
        use crate::hooks::config::{HookCommand, HookCommandType, HookMatcher, HooksConfig};
        // Use a mock result so this test doesn't spawn any OS process.
        // The dispatch logic under test: runner finds the hook, calls run_hook,
        // returns the result.  Mock replaces the real process execution.
        let cfg = HooksConfig {
            events: std::collections::HashMap::from([(
                "PreToolCall".to_string(),
                vec![HookMatcher {
                    matcher: None,
                    hooks: vec![HookCommand {
                        r#type: HookCommandType::Command,
                        command: Some("/bin/true".to_string()), // never actually spawned
                        timeout: 5,
                        ..Default::default()
                    }],
                }],
            )]),
        };
        let mock_skip = HookResult {
            action: HookAction::Skip,
            system_message: Some("blocked by test hook".to_string()),
            ..Default::default()
        };
        let runner = ExternalHookRunner::from_config(cfg).with_mock_results(vec![Some(mock_skip)]);
        let input = make_tool_input(
            HookEvent::PreToolCall,
            "Bash",
            serde_json::json!({"command": "ls"}),
        );
        let result = runner.dispatch(&input).await;
        assert!(
            matches!(result.action, HookAction::Skip),
            "expected Skip; got {:?}",
            result.action
        );
    }

    #[tokio::test]
    async fn dispatch_returns_continue_when_no_hooks() {
        let runner = ExternalHookRunner::discover(&[]);
        let input = make_tool_input(
            HookEvent::PreToolCall,
            "Read",
            serde_json::json!({"path": "foo.txt"}),
        );
        let result = runner.dispatch(&input).await;
        assert!(matches!(result.action, HookAction::Continue));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn dispatch_treats_timeout_as_continue() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let hook_path = tmp.path().join("hang.sh");

        // A hook that hangs (sleeps 30s — longer than the 5s timeout).
        let script = "#!/bin/sh\nsleep 30\n";
        std::fs::write(&hook_path, script).unwrap();
        let mut perms = std::fs::metadata(&hook_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hook_path, perms).unwrap();

        let runner = ExternalHookRunner::discover(&[tmp.path().to_path_buf()]);
        assert_eq!(runner.len(), 1);
        let input = make_tool_input(
            HookEvent::PreToolCall,
            "Bash",
            serde_json::json!({"command": "ls"}),
        );
        let result = runner.dispatch(&input).await;
        // Timeout → fail-open → Continue.
        assert!(matches!(result.action, HookAction::Continue));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn dispatch_treats_bad_output_as_continue() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let hook_path = tmp.path().join("bad.sh");

        // A hook that outputs invalid JSON.
        let script = "#!/bin/sh\necho 'not json'\n";
        std::fs::write(&hook_path, script).unwrap();
        let mut perms = std::fs::metadata(&hook_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hook_path, perms).unwrap();

        let runner = ExternalHookRunner::discover(&[tmp.path().to_path_buf()]);
        assert_eq!(runner.len(), 1);
        let input = make_tool_input(
            HookEvent::PreToolCall,
            "Bash",
            serde_json::json!({"command": "ls"}),
        );
        let result = runner.dispatch(&input).await;
        // Bad output → fail-open → Continue.
        assert!(matches!(result.action, HookAction::Continue));
    }

    #[tokio::test]
    async fn dispatch_short_circuits_on_first_non_continue() {
        use crate::hooks::config::{HookCommand, HookCommandType, HookMatcher, HooksConfig};
        // Two hooks registered for PreToolCall.
        // hook0 (mock) → Skip  → dispatch should short-circuit and NOT call hook1.
        // hook1 (mock) → Continue (should NOT be reached).
        let cfg = HooksConfig {
            events: std::collections::HashMap::from([(
                "PreToolCall".to_string(),
                vec![HookMatcher {
                    matcher: None,
                    hooks: vec![
                        HookCommand {
                            r#type: HookCommandType::Command,
                            command: Some("/bin/true".to_string()),
                            timeout: 5,
                            ..Default::default()
                        },
                        HookCommand {
                            r#type: HookCommandType::Command,
                            command: Some("/bin/true".to_string()),
                            timeout: 5,
                            ..Default::default()
                        },
                    ],
                }],
            )]),
        };
        let mock_skip = HookResult {
            action: HookAction::Skip,
            system_message: Some("first".to_string()),
            ..Default::default()
        };
        // Index 0 → Skip; index 1 → Continue. The dispatch must stop at index 0.
        let runner =
            ExternalHookRunner::from_config(cfg).with_mock_results(vec![Some(mock_skip), None]);
        let input = make_tool_input(
            HookEvent::PreToolCall,
            "Write",
            serde_json::json!({"path": "test.txt"}),
        );
        let result = runner.dispatch(&input).await;
        assert!(matches!(result.action, HookAction::Skip));
    }

    // ── Goal 204 new tests ──────────────────────────────────────

    #[test]
    fn new_hook_events_serialize_camel_case() {
        let cases: &[(&str, HookEvent)] = &[
            ("postToolCallFailure", HookEvent::PostToolCallFailure),
            ("permissionDenied", HookEvent::PermissionDenied),
            ("sessionStart", HookEvent::SessionStart),
            ("sessionEnd", HookEvent::SessionEnd),
            ("userPromptSubmit", HookEvent::UserPromptSubmit),
            ("stop", HookEvent::Stop),
            ("subagentStart", HookEvent::SubagentStart),
            ("subagentStop", HookEvent::SubagentStop),
            ("notification", HookEvent::Notification),
            ("setup", HookEvent::Setup),
        ];
        for (expected, event) in cases {
            let json = serde_json::to_string(event).unwrap();
            assert_eq!(
                json,
                format!("\"{expected}\""),
                "wrong camelCase for {expected}"
            );
        }
    }

    #[test]
    fn hook_input_optional_fields_absent_when_none() {
        let input = HookInput {
            event: HookEvent::UserPromptSubmit,
            tool_name: None,
            args: None,
            mode: "ask".to_string(),
            content: Some("hello".to_string()),
            message: None,
            depth: None,
            reason: None,
            error: None,
        };
        let json = serde_json::to_string(&input).unwrap();
        // tool_name and args should be absent
        assert!(
            !json.contains("tool_name") && !json.contains("toolName"),
            "toolName should be absent"
        );
        assert!(!json.contains("\"args\""), "args should be absent");
        assert!(json.contains("\"hello\""), "content should be present");
    }

    #[test]
    fn hook_input_tool_name_present_for_tool_events() {
        let input = make_tool_input(
            HookEvent::PreToolCall,
            "Bash",
            serde_json::json!({"command": "ls"}),
        );
        let json = serde_json::to_string(&input).unwrap();
        assert!(json.contains("Bash"));
        assert!(json.contains("command"));
    }

    // ── Goal 205 tests ─────────────────────────────────────────────

    #[test]
    fn hook_output_parses_additional_context() {
        let json = r#"{"action":"continue","additionalContext":"extra info"}"#;
        let output: HookOutput = serde_json::from_str(json).unwrap();
        let result = output.into_hook_result();
        assert!(matches!(result.action, HookAction::Continue));
        assert_eq!(result.additional_context.as_deref(), Some("extra info"));
    }

    #[test]
    fn hook_output_parses_updated_input() {
        let json = r#"{"action":"continue","updatedInput":{"command":"ls -la"}}"#;
        let output: HookOutput = serde_json::from_str(json).unwrap();
        let result = output.into_hook_result();
        assert_eq!(
            result.updated_input,
            Some(serde_json::json!({"command": "ls -la"}))
        );
    }

    #[test]
    fn hook_output_parses_permission_decision_allow() {
        let json = r#"{"action":"continue","permissionDecision":"allow","permissionDecisionReason":"safe"}"#;
        let output: HookOutput = serde_json::from_str(json).unwrap();
        let result = output.into_hook_result();
        assert_eq!(result.permission_decision, Some(PermissionDecision::Allow));
        assert_eq!(result.permission_decision_reason.as_deref(), Some("safe"));
    }

    #[test]
    fn hook_output_parses_permission_decision_deny() {
        let json = r#"{"action":"skip","permissionDecision":"deny"}"#;
        let output: HookOutput = serde_json::from_str(json).unwrap();
        let result = output.into_hook_result();
        assert_eq!(result.permission_decision, Some(PermissionDecision::Deny));
        assert!(matches!(result.action, HookAction::Skip));
    }

    #[test]
    fn hook_output_parses_permission_decision_passthrough() {
        let json = r#"{"action":"continue","permissionDecision":"passthrough"}"#;
        let output: HookOutput = serde_json::from_str(json).unwrap();
        let result = output.into_hook_result();
        assert_eq!(
            result.permission_decision,
            Some(PermissionDecision::Passthrough)
        );
    }

    #[test]
    fn hook_result_default_is_continue_with_no_extras() {
        let result = HookResult::default();
        assert!(matches!(result.action, HookAction::Continue));
        assert!(result.additional_context.is_none());
        assert!(result.updated_input.is_none());
        assert!(result.system_message.is_none());
        assert!(!result.suppress_output);
        assert!(result.permission_decision.is_none());
        assert!(result.permission_decision_reason.is_none());
    }

    #[test]
    fn hook_output_system_message_included() {
        let json = r#"{"action":"continue","systemMessage":"Please review before proceeding"}"#;
        let output: HookOutput = serde_json::from_str(json).unwrap();
        let result = output.into_hook_result();
        assert_eq!(
            result.system_message.as_deref(),
            Some("Please review before proceeding")
        );
    }

    #[test]
    fn hook_output_suppress_output_default_false() {
        let json = r#"{"action":"continue"}"#;
        let output: HookOutput = serde_json::from_str(json).unwrap();
        let result = output.into_hook_result();
        assert!(!result.suppress_output);
    }

    #[test]
    fn hook_output_suppress_output_true() {
        let json = r#"{"action":"continue","suppressOutput":true}"#;
        let output: HookOutput = serde_json::from_str(json).unwrap();
        let result = output.into_hook_result();
        assert!(result.suppress_output);
    }

    // ── Goal 207 HTTP hook tests ───────────────────────────────────

    fn make_non_tool_input() -> HookInput {
        HookInput {
            event: HookEvent::UserPromptSubmit,
            tool_name: None,
            args: None,
            mode: "ask".to_string(),
            content: Some("hello".to_string()),
            message: None,
            depth: None,
            reason: None,
            error: None,
        }
    }

    #[tokio::test]
    async fn http_hook_posts_json_input_and_parses_response() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/hook")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"action":"skip","message":"blocked by http hook"}"#)
            .create_async()
            .await;

        let input = make_non_tool_input();
        let url = format!("{}/hook", server.url());
        let result = run_http_hook(&url, &input, 10, None, None, HookFailMode::Open).await;
        assert!(matches!(result.action, HookAction::Skip));
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn http_hook_parses_continue_response() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/hook")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"action":"continue"}"#)
            .create_async()
            .await;

        let input = make_non_tool_input();
        let url = format!("{}/hook", server.url());
        let result = run_http_hook(&url, &input, 10, None, None, HookFailMode::Open).await;
        assert!(matches!(result.action, HookAction::Continue));
    }

    #[tokio::test]
    async fn http_hook_connection_error_returns_continue() {
        // Connect to a port that doesn't exist — should fail-open.
        let input = make_non_tool_input();
        let result = run_http_hook(
            "http://127.0.0.1:19999/hook",
            &input,
            2,
            None,
            None,
            HookFailMode::Open,
        )
        .await;
        assert!(matches!(result.action, HookAction::Continue));
    }

    #[tokio::test]
    async fn http_hook_bad_json_response_returns_continue() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/hook")
            .with_status(200)
            .with_body("not json at all")
            .create_async()
            .await;

        let input = make_non_tool_input();
        let url = format!("{}/hook", server.url());
        let result = run_http_hook(&url, &input, 10, None, None, HookFailMode::Open).await;
        assert!(matches!(result.action, HookAction::Continue));
    }

    #[test]
    fn env_var_interpolation_respects_allowlist() {
        std::env::set_var("MY_TOKEN", "secret123");
        let allowed = vec!["MY_TOKEN".to_string()];
        let result = interpolate_env_vars("Bearer $MY_TOKEN", Some(&allowed));
        assert_eq!(result, "Bearer secret123");
    }

    #[test]
    fn env_var_interpolation_empty_for_non_allowed() {
        std::env::set_var("BLOCKED_VAR", "should-not-appear");
        let allowed = vec!["OTHER_VAR".to_string()];
        let result = interpolate_env_vars("Bearer $BLOCKED_VAR", Some(&allowed));
        assert_eq!(result, "Bearer ");
    }

    #[test]
    fn env_var_interpolation_curly_brace_form() {
        std::env::set_var("API_KEY", "mykey");
        let allowed = vec!["API_KEY".to_string()];
        let result = interpolate_env_vars("key=${API_KEY}", Some(&allowed));
        assert_eq!(result, "key=mykey");
    }

    #[test]
    fn env_var_interpolation_no_allowlist_replaces_all() {
        std::env::set_var("UNGUARDED", "value");
        let result = interpolate_env_vars("$UNGUARDED", None);
        assert_eq!(result, "value");
    }

    // ── Goal 208 Prompt hook tests ─────────────────────────────────

    #[tokio::test]
    async fn prompt_hook_replaces_arguments_placeholder() {
        use crate::llm::{Completion, MockProvider};
        // The mock provider will capture what prompt it received.
        let captured = Arc::new(std::sync::Mutex::new(String::new()));
        let c = captured.clone();

        // We can't easily intercept the prompt with MockProvider, so just
        // verify the integration: mock returns skip JSON.
        let provider = MockProvider::new(vec![Completion {
            content: r#"{"action":"skip","message":"prompt blocked"}"#.to_string(),
            ..Default::default()
        }]);
        drop(c); // suppress unused warning

        let input = make_non_tool_input();
        let result = run_prompt_hook(
            &provider,
            "Is this safe? $ARGUMENTS",
            &input,
            Duration::from_secs(10),
            HookFailMode::Open,
        )
        .await;
        assert!(matches!(result.action, HookAction::Skip));
    }

    #[tokio::test]
    async fn prompt_hook_uses_llm_response_continue() {
        use crate::llm::{Completion, MockProvider};
        let provider = MockProvider::new(vec![Completion {
            content: r#"{"action":"continue"}"#.to_string(),
            ..Default::default()
        }]);
        let input = make_non_tool_input();
        let result = run_prompt_hook(
            &provider,
            "Check: $ARGUMENTS",
            &input,
            Duration::from_secs(10),
            HookFailMode::Open,
        )
        .await;
        assert!(matches!(result.action, HookAction::Continue));
    }

    #[tokio::test]
    async fn prompt_hook_falls_back_on_non_json_response() {
        use crate::llm::{Completion, MockProvider};
        let provider = MockProvider::new(vec![Completion {
            content: "Sorry, I cannot evaluate this.".to_string(),
            ..Default::default()
        }]);
        let input = make_non_tool_input();
        let result = run_prompt_hook(
            &provider,
            "Is this safe? $ARGUMENTS",
            &input,
            Duration::from_secs(10),
            HookFailMode::Open,
        )
        .await;
        // Non-JSON → fail-open → Continue.
        assert!(matches!(result.action, HookAction::Continue));
    }

    #[tokio::test]
    async fn no_llm_prompt_hook_returns_continue() {
        use crate::hooks::config::HooksConfig;
        let json = r#"{"PreToolCall":[{"hooks":[{"type":"prompt","prompt":"Is this safe? $ARGUMENTS"}]}]}"#;
        let cfg: HooksConfig = serde_json::from_str(json).unwrap();
        // No LLM provided.
        let runner = ExternalHookRunner::from_config_with_llm(cfg, None);
        assert_eq!(runner.len(), 1); // hook is registered

        let input = make_tool_input(HookEvent::PreToolCall, "Bash", serde_json::json!({}));
        let result = runner.dispatch(&input).await;
        // Should fail-open because no LLM is configured.
        assert!(matches!(result.action, HookAction::Continue));
    }

    // ── Goal 206 tests ─────────────────────────────────────────────

    #[test]
    fn from_config_creates_hooks_from_json() {
        use crate::hooks::config::HooksConfig;
        let json = r#"{"PreToolCall":[{"matcher":"Bash","hooks":[{"type":"command","command":"/usr/bin/true"}]}]}"#;
        let cfg: HooksConfig = serde_json::from_str(json).unwrap();
        let runner = ExternalHookRunner::from_config(cfg);
        assert_eq!(runner.len(), 1);
    }

    #[test]
    fn from_config_http_type_is_registered() {
        use crate::hooks::config::HooksConfig;
        // http type is now supported and should be registered
        let json = r#"{"PreToolCall":[{"hooks":[{"type":"http","url":"https://example.com"}]}]}"#;
        let cfg: HooksConfig = serde_json::from_str(json).unwrap();
        let runner = ExternalHookRunner::from_config(cfg);
        assert_eq!(runner.len(), 1);
    }

    #[test]
    fn from_config_prompt_without_prompt_field_is_skipped() {
        use crate::hooks::config::HooksConfig;
        // prompt type without a prompt field → None from resolve_command → skipped
        let json = r#"{"PreToolCall":[{"hooks":[{"type":"prompt"}]}]}"#;
        let cfg: HooksConfig = serde_json::from_str(json).unwrap();
        let runner = ExternalHookRunner::from_config(cfg);
        assert_eq!(runner.len(), 0);
    }

    #[test]
    fn from_config_http_without_url_is_skipped() {
        use crate::hooks::config::HooksConfig;
        // http type without a url field → None from resolve_command → skipped
        let json = r#"{"PreToolCall":[{"hooks":[{"type":"http"}]}]}"#;
        let cfg: HooksConfig = serde_json::from_str(json).unwrap();
        let runner = ExternalHookRunner::from_config(cfg);
        assert_eq!(runner.len(), 0);
    }

    #[test]
    fn from_config_empty_config_gives_empty_runner() {
        use crate::hooks::config::HooksConfig;
        let runner = ExternalHookRunner::from_config(HooksConfig::default());
        assert!(runner.is_empty());
    }

    #[tokio::test]
    async fn from_config_respects_matcher_event_filter() {
        use crate::hooks::config::{HookCommand, HookCommandType, HookMatcher, HooksConfig};
        // Hook registered for "PostToolCall" matching "Bash".
        // The mock result ensures we never spawn a process; we only test the event-filter logic.
        let cfg = HooksConfig {
            events: std::collections::HashMap::from([(
                "PostToolCall".to_string(),
                vec![HookMatcher {
                    matcher: Some("Bash".to_string()),
                    hooks: vec![HookCommand {
                        r#type: HookCommandType::Command,
                        command: Some("/bin/true".to_string()),
                        timeout: 5,
                        ..Default::default()
                    }],
                }],
            )]),
        };
        let mock_skip = HookResult {
            action: HookAction::Skip,
            ..Default::default()
        };
        let runner = ExternalHookRunner::from_config(cfg).with_mock_results(vec![Some(mock_skip)]);

        // Dispatch PreToolCall — should NOT trigger the PostToolCall hook.
        let pre_input = make_tool_input(HookEvent::PreToolCall, "Bash", serde_json::json!({}));
        let result = runner.dispatch(&pre_input).await;
        assert!(
            matches!(result.action, HookAction::Continue),
            "PreToolCall should not trigger PostToolCall hook"
        );

        // Dispatch PostToolCall — SHOULD trigger the mock Skip.
        let post_input = make_tool_input(HookEvent::PostToolCall, "Bash", serde_json::json!({}));
        let result = runner.dispatch(&post_input).await;
        assert!(
            matches!(result.action, HookAction::Skip),
            "PostToolCall should trigger hook"
        );
    }

    // ── Goal 209 async / once tests ───────────────────────────────

    #[tokio::test]
    async fn once_hook_runs_only_first_time() {
        use crate::hooks::config::{HookCommand, HookCommandType, HookMatcher, HooksConfig};
        // Hook returns "Skip" on first invocation (via mock).
        // With `once: true`, the second dispatch skips the hook entirely → "Continue".
        let cfg = HooksConfig {
            events: std::collections::HashMap::from([(
                "PreToolCall".to_string(),
                vec![HookMatcher {
                    matcher: None,
                    hooks: vec![HookCommand {
                        r#type: HookCommandType::Command,
                        command: Some("/bin/true".to_string()),
                        once: true,
                        timeout: 5,
                        ..Default::default()
                    }],
                }],
            )]),
        };
        let mock_skip = HookResult {
            action: HookAction::Skip,
            ..Default::default()
        };
        let runner = ExternalHookRunner::from_config(cfg).with_mock_results(vec![Some(mock_skip)]);
        let input = make_tool_input(HookEvent::PreToolCall, "Bash", serde_json::json!({}));
        let first = runner.dispatch(&input).await;
        let second = runner.dispatch(&input).await;
        assert!(
            matches!(first.action, HookAction::Skip),
            "first dispatch must run the hook → Skip"
        );
        assert!(
            matches!(second.action, HookAction::Continue),
            "second dispatch must skip the once-hook → Continue"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn async_hook_returns_continue_immediately() {
        use crate::hooks::config::{HookCommand, HookCommandType, HookMatcher, HooksConfig};
        let tmp = tempfile::tempdir().unwrap();
        let hook_path = tmp.path().join("slow_hook.sh");
        // Hook that sleeps 10s — if async works, dispatch should return immediately.
        // No stdin read: avoids stalling before the sleep starts under CPU load.
        std::fs::write(
            &hook_path,
            "#!/bin/sh\nsleep 10\necho '{\"action\":\"skip\"}'\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&hook_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&hook_path, perms).unwrap();
        }
        let cfg = HooksConfig {
            events: std::collections::HashMap::from([(
                "PreToolCall".to_string(),
                vec![HookMatcher {
                    matcher: None,
                    hooks: vec![HookCommand {
                        r#type: HookCommandType::Command,
                        command: Some(hook_path.to_string_lossy().to_string()),
                        r#async: true,
                        timeout: 5,
                        ..Default::default()
                    }],
                }],
            )]),
        };
        let runner = ExternalHookRunner::from_config(cfg);
        let input = make_tool_input(HookEvent::PreToolCall, "Bash", serde_json::json!({}));
        let start = std::time::Instant::now();
        let result = runner.dispatch(&input).await;
        let elapsed = start.elapsed();
        // Should return immediately (< 1s) even though hook sleeps 10s.
        assert!(
            elapsed.as_secs() < 1,
            "async hook blocked dispatch: {elapsed:?}"
        );
        // Async hooks always return Continue immediately.
        assert!(matches!(result.action, HookAction::Continue));
    }

    #[tokio::test]
    async fn async_rewake_exit2_triggers_cancel() {
        use crate::hooks::config::{HookCommand, HookCommandType, HookMatcher, HooksConfig};
        // Use mock exit code 2 so no OS process is spawned.
        // The asyncRewake branch sees exit_code == Some(2) and cancels the token.
        let token = CancellationToken::new();
        let child_token = token.clone();
        let cfg = HooksConfig {
            events: std::collections::HashMap::from([(
                "PreToolCall".to_string(),
                vec![HookMatcher {
                    matcher: None,
                    hooks: vec![HookCommand {
                        r#type: HookCommandType::Command,
                        command: Some("/bin/true".to_string()), // never spawned
                        async_rewake: true,
                        timeout: 5,
                        ..Default::default()
                    }],
                }],
            )]),
        };
        let runner = ExternalHookRunner::from_config(cfg)
            .with_cancel_token(child_token)
            .with_mock_exit_codes(vec![Some(2)]);
        let input = make_tool_input(HookEvent::PreToolCall, "Bash", serde_json::json!({}));
        runner.dispatch(&input).await;
        // The background task should complete and cancel the token almost immediately.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !token.is_cancelled() {
            if std::time::Instant::now() >= deadline {
                panic!("timed out waiting for asyncRewake cancellation");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            token.is_cancelled(),
            "exit code 2 should have triggered cancellation"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn async_rewake_exit0_no_cancel() {
        use crate::hooks::config::{HookCommand, HookCommandType, HookMatcher, HooksConfig};
        let tmp = tempfile::tempdir().unwrap();
        let hook_path = tmp.path().join("exit0.sh");
        // Hook that exits with code 0 (success). No stdin read to avoid stall under load.
        std::fs::write(&hook_path, "#!/bin/sh\nexit 0\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&hook_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&hook_path, perms).unwrap();
        }
        let token = CancellationToken::new();
        let child_token = token.clone();
        let cfg = HooksConfig {
            events: std::collections::HashMap::from([(
                "PreToolCall".to_string(),
                vec![HookMatcher {
                    matcher: None,
                    hooks: vec![HookCommand {
                        r#type: HookCommandType::Command,
                        command: Some(hook_path.to_string_lossy().to_string()),
                        async_rewake: true,
                        timeout: 5,
                        ..Default::default()
                    }],
                }],
            )]),
        };
        let runner = ExternalHookRunner::from_config(cfg).with_cancel_token(child_token);
        let input = make_tool_input(HookEvent::PreToolCall, "Bash", serde_json::json!({}));
        runner.dispatch(&input).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !token.is_cancelled(),
            "exit code 0 should NOT trigger cancellation"
        );
    }

    // ── Goal 281: fail_mode tests ──────────────────────────────────

    #[tokio::test]
    #[cfg(unix)]
    async fn open_hook_times_out_returns_continue() {
        use crate::hooks::config::{HookCommand, HookCommandType, HookMatcher, HooksConfig};
        let tmp = tempfile::tempdir().unwrap();
        let hook_path = tmp.path().join("open_hang.sh");
        let script = "#!/bin/sh\nsleep 30\n";
        std::fs::write(&hook_path, script).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&hook_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&hook_path, perms).unwrap();
        }
        let cfg = HooksConfig {
            events: std::collections::HashMap::from([(
                "PreToolCall".to_string(),
                vec![HookMatcher {
                    matcher: None,
                    hooks: vec![HookCommand {
                        r#type: HookCommandType::Command,
                        command: Some(hook_path.to_string_lossy().to_string()),
                        timeout: 1,
                        fail_mode: HookFailMode::Open,
                        ..Default::default()
                    }],
                }],
            )]),
        };
        let runner = ExternalHookRunner::from_config(cfg);
        let input = make_tool_input(
            HookEvent::PreToolCall,
            "Bash",
            serde_json::json!({"command": "ls"}),
        );
        let result = runner.dispatch(&input).await;
        // Fail-open: timeout → Continue.
        assert!(
            matches!(result.action, HookAction::Continue),
            "Open hook timeout should return Continue, got {:?}",
            result.action
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn closed_hook_times_out_returns_error() {
        use crate::hooks::config::{HookCommand, HookCommandType, HookMatcher, HooksConfig};
        let tmp = tempfile::tempdir().unwrap();
        let hook_path = tmp.path().join("closed_hang.sh");
        let script = "#!/bin/sh\nsleep 30\n";
        std::fs::write(&hook_path, script).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&hook_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&hook_path, perms).unwrap();
        }
        let cfg = HooksConfig {
            events: std::collections::HashMap::from([(
                "PreToolCall".to_string(),
                vec![HookMatcher {
                    matcher: None,
                    hooks: vec![HookCommand {
                        r#type: HookCommandType::Command,
                        command: Some(hook_path.to_string_lossy().to_string()),
                        timeout: 1,
                        fail_mode: HookFailMode::Closed,
                        ..Default::default()
                    }],
                }],
            )]),
        };
        let runner = ExternalHookRunner::from_config(cfg);
        let input = make_tool_input(
            HookEvent::PreToolCall,
            "Bash",
            serde_json::json!({"command": "ls"}),
        );
        let result = runner.dispatch(&input).await;
        // Fail-closed on PreToolCall: timeout → Error.
        assert!(
            matches!(result.action, HookAction::Error(_)),
            "Closed hook timeout should return Error, got {:?}",
            result.action
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn closed_hook_exits_nonzero_returns_error() {
        use crate::hooks::config::{HookCommand, HookCommandType, HookMatcher, HooksConfig};
        let tmp = tempfile::tempdir().unwrap();
        let hook_path = tmp.path().join("closed_exit1.sh");
        let script = "#!/bin/sh\nexit 1\n";
        std::fs::write(&hook_path, script).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&hook_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&hook_path, perms).unwrap();
        }
        let cfg = HooksConfig {
            events: std::collections::HashMap::from([(
                "PreToolCall".to_string(),
                vec![HookMatcher {
                    matcher: None,
                    hooks: vec![HookCommand {
                        r#type: HookCommandType::Command,
                        command: Some(hook_path.to_string_lossy().to_string()),
                        timeout: 5,
                        fail_mode: HookFailMode::Closed,
                        ..Default::default()
                    }],
                }],
            )]),
        };
        let runner = ExternalHookRunner::from_config(cfg);
        let input = make_tool_input(
            HookEvent::PreToolCall,
            "Bash",
            serde_json::json!({"command": "rm -rf /"}),
        );
        let result = runner.dispatch(&input).await;
        // Fail-closed on PreToolCall: non-zero exit → Error.
        assert!(
            matches!(result.action, HookAction::Error(_)),
            "Closed hook non-zero exit should return Error, got {:?}",
            result.action
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn closed_hook_post_tool_call_timeout_returns_continue() {
        use crate::hooks::config::{HookCommand, HookCommandType, HookMatcher, HooksConfig};
        let tmp = tempfile::tempdir().unwrap();
        let hook_path = tmp.path().join("closed_post_hang.sh");
        let script = "#!/bin/sh\nsleep 30\n";
        std::fs::write(&hook_path, script).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&hook_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&hook_path, perms).unwrap();
        }
        let cfg = HooksConfig {
            events: std::collections::HashMap::from([(
                "PostToolCall".to_string(),
                vec![HookMatcher {
                    matcher: None,
                    hooks: vec![HookCommand {
                        r#type: HookCommandType::Command,
                        command: Some(hook_path.to_string_lossy().to_string()),
                        timeout: 1,
                        fail_mode: HookFailMode::Closed,
                        ..Default::default()
                    }],
                }],
            )]),
        };
        let runner = ExternalHookRunner::from_config(cfg);
        let input = make_tool_input(
            HookEvent::PostToolCall,
            "Bash",
            serde_json::json!({"command": "ls"}),
        );
        let result = runner.dispatch(&input).await;
        // Closed on PostToolCall is a no-op: timeout → Continue.
        assert!(
            matches!(result.action, HookAction::Continue),
            "Closed PostToolCall timeout should return Continue, got {:?}",
            result.action
        );
    }
}
