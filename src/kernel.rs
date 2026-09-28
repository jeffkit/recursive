//! Turn-level types for the Agent Run Kernel architecture.
//!
//! This module defines the input/output contract for a single turn of
//! agent execution:
//!
//! * [`TurnContext`] — everything the kernel needs to execute one turn
//!   (messages, tools, config, event sink).
//! * [`TurnOutcome`] — the result of executing one turn (new messages,
//!   usage, finish reason, side effects).
//! * [`AgentKernel`] — the stateless single-turn executor. Its `run()`
//!   method delegates to [`crate::run_core::RunCore`] which owns the
//!   ReAct step loop.
//!
//! # Design
//!
//! The Kernel is stateless and knows nothing about transcripts, sessions,
//! or cross-turn state. The Wrapper (`AgentRuntime`) prepares a
//! `TurnContext` from its transcript, calls the kernel, and then
//! incorporates the `TurnOutcome` back into its state.
//!
//! The kernel passes the caller's `AgentEvent` channel directly to
//! `RunCore` — no internal bridge (introduced in Goal 219).

use crate::agent::FinishReason;
use crate::compact::Compactor;
use crate::event::AgentEvent;
use crate::hooks::HookRegistry;
use crate::llm::{ChatProvider, TokenUsage, ToolSpec};
use crate::message::Message;
use crate::permissions::PermissionMode;
use crate::storage::{NoopSessionStore, SessionStore, StorageBackend};
use crate::tool_set_provider::ToolSetProvider;
use crate::tools::PermissionHook;
use crate::tools::ToolRegistry;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// TurnContext
// ---------------------------------------------------------------------------

/// Everything the Kernel needs to execute one turn.
///
/// Prepared by the Wrapper (AgentRuntime). The Kernel does not know
/// where these messages came from — could be fresh, compacted, or resumed.
///
/// # Ownership
///
/// `messages` is an **owned copy** of the wrapper's transcript. The kernel
/// takes ownership and may mutate it freely during the ReAct loop (trimming
/// old tool results, intra-turn compaction). The wrapper retains the canonical
/// transcript and incorporates only the new messages from
/// [`TurnOutcome::new_messages`] after the turn completes.
///
/// A full clone per turn is intentional: `RunCore` mutates the list in-place,
/// so sharing via `Arc` would not eliminate the allocation. The clone is
/// bounded by the `max_transcript_chars` trim that runs before each turn.
pub struct TurnContext {
    /// Shared reference to the wrapper's transcript for this turn.
    ///
    /// The kernel may mutate this list in-place via `Arc::make_mut`;
    /// the wrapper's canonical transcript is unaffected until the
    /// kernel drops its reference.
    pub messages: Arc<Vec<Message>>,

    /// Channel to send agent events to the caller (runtime or test harness).
    ///
    /// The kernel passes this channel directly to `RunCore` (Goal 219 Commit
    /// 1), so callers receive the same `AgentEvent` stream the kernel sees —
    /// no internal conversion.
    pub step_events_tx: Option<tokio::sync::mpsc::UnboundedSender<AgentEvent>>,

    /// Tool specifications to advertise to the LLM.
    pub tool_specs: Vec<ToolSpec>,

    /// Whether to stream LLM responses token-by-token.
    pub streaming: bool,

    /// Optional permission hook for gating tool calls.
    pub permission_hook: Option<Arc<dyn PermissionHook>>,

    /// Goal-165: shared flag that enables agent-driven read-only plan mode.
    /// When `true`, write tools are blocked until `exit_plan_mode` is called.
    pub exploring_plan_mode: Arc<AtomicBool>,

    /// Goal-190: default permission mode for tools not covered by explicit
    /// config lists. Mirrors `PermissionsConfig.mode` for quick access.
    pub permission_mode: PermissionMode,

    /// Optional mailbox for mid-run message injection from a coordinator.
    ///
    /// When set, the kernel drains this mailbox at the start of every step
    /// and appends any pending messages as user turns.  This powers the
    /// `send_message` tool's bidirectional coordinator ↔ worker flow.
    pub mailbox: Option<crate::tools::send_message::WorkerMailbox>,

    /// Turn index (0-based), used to scope [`crate::tools::AuditMeta`]
    /// keys so that tool-call-id collisions across turns cannot cause
    /// audit metadata to be overwritten or lost.
    pub turn: u32,

    /// Goal-328: structured prompt segments from `assemble_system_prompt`,
    /// forwarded to `RunCore` so it can size the static breakdown
    /// buckets. `None` when the caller did not provide one (legacy
    /// channels, tests).
    pub prompt_segments: Option<crate::system_prompt::PromptSegments>,

    /// Goal 345: wall-clock timeout in seconds. When > 0, the step
    /// loop checks elapsed time and terminates cleanly with
    /// [`FinishReason::WallClockExceeded`]. Default 0 = unset.
    pub wall_timeout_secs: u64,
}

// ---------------------------------------------------------------------------
// TurnOutcome
// ---------------------------------------------------------------------------

/// The result of executing one turn.
///
/// Returned to the Wrapper, which appends new_messages to its transcript,
/// persists them, handles side effects, and tracks costs.
#[derive(Debug)]
pub struct TurnOutcome {
    /// All messages produced during this turn (assistant responses + tool results).
    /// Does NOT include the input messages — only what the kernel generated.
    pub new_messages: Vec<Message>,

    /// The final assistant text (convenience — also the last assistant msg in new_messages).
    pub final_text: Option<String>,

    /// Why the turn ended.
    pub finish_reason: FinishReason,

    /// Cumulative token usage across all LLM calls in this turn.
    pub usage: TokenUsage,

    /// `prompt_tokens` from the last LLM call only (not accumulated).
    /// Use for cross-turn compaction; `usage.prompt_tokens` is the turn sum.
    pub last_prompt_tokens: u32,

    /// Total LLM call latency in milliseconds (excluding tool execution time).
    pub llm_latency_ms: u64,

    /// Number of steps (LLM invocations) executed in this turn.
    pub steps: usize,

    /// Goal-153: audit records for tool results, keyed by `(turn, tool_call_id)`.
    /// Passed through from `RunInnerOutcome` so the persistence layer
    /// can emit `MessageAppendedWithAudit` for tool messages.
    pub tool_audits: std::collections::HashMap<crate::tools::AuditKey, crate::tools::AuditMeta>,

    /// Turn index (0-based) this outcome belongs to. Redundant with the key
    /// prefix in `tool_audits`, but needed for `emit_turn_messages` lookup.
    pub turn: u32,
}

// ---------------------------------------------------------------------------
// AgentKernel
// ---------------------------------------------------------------------------

/// The stateless Agent Kernel — a single-turn ReAct executor.
///
/// Cheap to create, safe to clone, safe to share across threads.
/// Does not own transcript, session, or any cross-turn state.
///
#[derive(Clone)]
pub struct AgentKernel {
    /// The LLM provider to use for completions.
    pub(crate) llm: Arc<dyn ChatProvider>,
    /// The tool registry (tools available to the agent).
    pub(crate) tools: ToolRegistry,
    /// Maximum number of LLM calls per turn.
    pub(crate) max_steps: usize,
    /// Maximum transcript characters before trimming (None = no limit).
    pub(crate) max_transcript_chars: Option<usize>,
    /// Optional compactor for summarising old messages.
    pub(crate) compactor: Option<Compactor>,
    /// Optional microcompactor for no-LLM proactive pruning of old tool results.
    pub(crate) microcompactor: Option<crate::compact::Microcompactor>,
    /// Hook registry for lifecycle hooks.
    pub(crate) hooks: HookRegistry,
    /// Optional cancellation token for graceful shutdown. When the token
    /// is cancelled, the kernel's step loop terminates with
    /// [`FinishReason::Cancelled`](crate::agent::FinishReason::Cancelled)
    /// at the next step boundary.
    pub(crate) shutdown_token: Option<tokio_util::sync::CancellationToken>,
    /// Pluggable storage backend (transcript + memory). Defaults to a
    /// `LocalStorageBackend` when not set; cloud deployments inject S3.
    pub(crate) storage: Arc<dyn StorageBackend>,
    /// Pluggable session hot-state store (checkpoint step/transcript_len).
    /// Defaults to `NoopSessionStore`; cloud deployments inject Redis.
    pub(crate) session_store: Arc<dyn SessionStore>,
    /// Sliding window size for stuck detection (from Config). Default 10.
    pub(crate) stuck_window: usize,
    /// Error rate threshold within the window to declare stuck. Default 0.8.
    pub(crate) stuck_error_rate: f64,
    /// Goal-318: Globs-mode skills passed to `SkillInjector` each run.
    pub(crate) globs_skills: Vec<crate::skills::Skill>,
    /// Goal 399: session wall-clock budget in seconds. Serves as the default
    /// for [`TurnContext::wall_timeout_secs`] when the caller leaves it at 0.
    /// 0 = unlimited (legacy behaviour, unchanged).
    pub(crate) wall_timeout_secs: u64,
}

impl std::fmt::Debug for AgentKernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let tools_count = self.tools.names().len();
        let hooks_count = self.hooks.len();
        f.debug_struct("AgentKernel")
            .field("llm", &"<ChatProvider>")
            .field("tools_count", &tools_count)
            .field("max_steps", &self.max_steps)
            .field("max_transcript_chars", &self.max_transcript_chars)
            .field("compactor", &self.compactor)
            .field("hooks_count", &hooks_count)
            .field("storage", &"<StorageBackend>")
            .field("session_store", &"<SessionStore>")
            .finish()
    }
}

impl AgentKernel {
    /// Create a new builder for `AgentKernel`.
    #[cfg_attr(test, mutants::skip)]
    pub fn builder() -> AgentKernelBuilder {
        AgentKernelBuilder::default()
    }

    /// Access the LLM provider.
    pub fn llm(&self) -> &Arc<dyn ChatProvider> {
        &self.llm
    }

    /// Hot-swap the LLM provider.
    ///
    /// Replaces the provider used for subsequent completions. Used by the
    /// TUI `/model` picker to switch models without restarting the process.
    /// Safe to call between turns; callers must not invoke it while a turn
    /// is in flight on the same kernel (the runtime's turn task owns the
    /// kernel via `&mut self` for the duration of the turn).
    pub fn set_llm(&mut self, llm: Arc<dyn ChatProvider>) {
        self.llm = llm;
    }

    /// Access the tool registry.
    pub fn tools(&self) -> &ToolRegistry {
        &self.tools
    }

    /// Mutable access to the tool registry.
    ///
    /// Used by [`AgentRuntime::enable_checkpoints`] to register
    /// session-scoped read-only tools (`checkpoint_list`,
    /// `checkpoint_diff`) once the session id is known.
    pub fn tools_mut(&mut self) -> &mut ToolRegistry {
        &mut self.tools
    }

    /// Access the cancellation token, if one was configured.
    ///
    /// Useful for tests verifying that token propagation through
    /// `with_tools` (and other clones) preserves the handle.
    pub fn shutdown_token(&self) -> Option<&tokio_util::sync::CancellationToken> {
        self.shutdown_token.as_ref()
    }

    /// Access the storage backend.
    pub fn storage(&self) -> &Arc<dyn StorageBackend> {
        &self.storage
    }

    /// Access the session store.
    pub fn session_store(&self) -> &Arc<dyn SessionStore> {
        &self.session_store
    }

    /// Public access to the hook registry. Used by `AgentRuntime` to
    /// dispatch cross-turn `PreCompact` / `PostCompact` events that are
    /// not handled by `RunCore`.
    pub fn hooks(&self) -> &HookRegistry {
        &self.hooks
    }

    /// Create a new kernel with a different tool registry (same LLM, same config).
    /// Useful for Multi-Agent scenarios where sub-agents get restricted tool subsets.
    pub fn with_tools(&self, tools: ToolRegistry) -> Self {
        let mut clone = self.clone();
        clone.tools = tools;
        clone
    }

    /// Execute one turn of the ReAct loop.
    ///
    /// Takes a [`TurnContext`] prepared by the Wrapper and returns a
    /// [`TurnOutcome`] containing only the new messages produced during
    /// this turn, plus usage stats and finish reason.
    ///
    /// The Kernel is stateless: it does not retain any state between calls.
    /// All cross-turn concerns (transcript accumulation, compaction, persistence)
    /// are the Wrapper's responsibility.
    pub async fn run(&self, ctx: TurnContext) -> crate::error::Result<TurnOutcome> {
        let input_len = ctx.messages.len();

        // Goal 399: resolve the effective wall-clock budget. An explicit
        // per-turn `ctx.wall_timeout_secs` wins when non-zero; otherwise the
        // kernel-level budget (set via `AgentKernelBuilder::wall_timeout_secs`)
        // applies. 0 on both = unlimited (legacy behaviour).
        let wall_timeout_secs = if ctx.wall_timeout_secs > 0 {
            ctx.wall_timeout_secs
        } else {
            self.wall_timeout_secs
        };

        let core = {
            use crate::run_core::{RunCore, StaticBreakdownCache};
            // Goal-328: size the static breakdown cache from the provided
            // prompt segments + the tool registry's specs. The cache is
            // read-only for the rest of the run (no field on RunCore
            // mutates it after construction); only `conversation` and
            // `overhead` are recomputed per step.
            let static_breakdown = match ctx.prompt_segments.as_ref() {
                Some(segments) => {
                    StaticBreakdownCache::build(segments, &self.tools.specs(), &self.tools)
                }
                None => StaticBreakdownCache::default(),
            };
            RunCore {
                messages: ctx.messages,
                llm: self.llm.clone(),
                tools: Arc::new(self.tools.clone()),
                max_steps: self.max_steps,
                max_transcript_chars: self.max_transcript_chars,
                events: ctx.step_events_tx,
                streaming: ctx.streaming,
                compactor: self.compactor.clone(),
                microcompactor: self.microcompactor.clone(),
                permission_hook: ctx.permission_hook,
                hooks: &self.hooks,
                total_llm_latency_ms: 0,
                exploring_plan_mode: ctx.exploring_plan_mode,
                shutdown_token: self.shutdown_token.clone(),
                mailbox: ctx.mailbox,
                stuck_window: self.stuck_window,
                stuck_error_rate: self.stuck_error_rate,
                turn: ctx.turn,
                globs_skills: self.globs_skills.clone(),
                prompt_segments: ctx.prompt_segments,
                static_breakdown,
                last_prompt_tokens: 0,
                consecutive_compact_failures: 0,
                wall_timeout_secs,
                wall_start: if wall_timeout_secs > 0 {
                    Some(std::time::Instant::now())
                } else {
                    None
                },
            }
        };

        let inner = core.run_inner().await?;

        // Extract only the messages produced during this turn.
        //
        // If `RunCore` performed intra-turn compaction, a summary message
        // (marked with `is_compaction_summary`) is inserted at position 0.
        // `inner.messages[input_len..]` would miss that summary, so detect
        // it and prepend.
        let mut new_messages = turn_delta_messages(&inner.messages, input_len);
        if !inner.messages.is_empty() && inner.messages[0].is_compaction_summary {
            new_messages.insert(0, inner.messages[0].clone());
        }

        Ok(TurnOutcome {
            new_messages,
            final_text: inner.final_message,
            finish_reason: inner.finish_reason,
            usage: inner.total_usage,
            last_prompt_tokens: inner.last_prompt_tokens,
            llm_latency_ms: inner.total_llm_latency_ms,
            steps: inner.steps,
            tool_audits: inner.tool_audits,
            turn: ctx.turn,
        })
    }
}

/// Messages produced after `input_len` in this turn. Soft-skipped: `>` vs `>=`
/// is equivalent when lengths are equal (empty suffix slice).
#[cfg_attr(test, mutants::skip)]
fn turn_delta_messages(
    inner: &[crate::message::Message],
    input_len: usize,
) -> Vec<crate::message::Message> {
    if inner.len() > input_len {
        inner[input_len..].to_vec()
    } else {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// AgentKernelBuilder
// ---------------------------------------------------------------------------

/// Builder for [`AgentKernel`].
#[derive(Default)]
pub struct AgentKernelBuilder {
    llm: Option<Arc<dyn ChatProvider>>,
    tools: Option<ToolRegistry>,
    max_steps: Option<usize>,
    max_transcript_chars: Option<usize>,
    compactor: Option<Compactor>,
    microcompactor: Option<crate::compact::Microcompactor>,
    hooks: Option<HookRegistry>,
    shutdown_token: Option<tokio_util::sync::CancellationToken>,
    /// Pluggable storage backend. When `None`, `build()` falls back to
    /// `LocalStorageBackend` rooted at the current directory.
    storage: Option<Arc<dyn StorageBackend>>,
    /// Pluggable session hot-state store. When `None`, `build()` uses
    /// `NoopSessionStore` (no-op, zero overhead).
    session_store: Option<Arc<dyn SessionStore>>,
    /// Pluggable tool set provider. When `Some`, `build()` calls
    /// `provider.build_registry()` unless `tools` was set explicitly.
    tool_set_provider: Option<Arc<dyn ToolSetProvider>>,
    /// Stuck detection window (default 10).
    stuck_window: Option<usize>,
    /// Stuck detection error rate threshold (default 0.8).
    stuck_error_rate: Option<f64>,
    /// Goal-318: Globs-mode skills.
    globs_skills: Vec<crate::skills::Skill>,
    /// Goal 399: session wall-clock budget in seconds (default 0 = unlimited).
    wall_timeout_secs: u64,
}

impl std::fmt::Debug for AgentKernelBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let tools_desc = self.tools.as_ref().map(|t| t.names().len());
        let hooks_desc = self.hooks.as_ref().map(|h| h.len());
        f.debug_struct("AgentKernelBuilder")
            .field("llm", &self.llm.as_ref().map(|_| "<ChatProvider>"))
            .field("tools", &tools_desc)
            .field("max_steps", &self.max_steps)
            .field("max_transcript_chars", &self.max_transcript_chars)
            .field("compactor", &self.compactor)
            .field("hooks", &hooks_desc)
            .field(
                "storage",
                &self.storage.as_ref().map(|_| "<StorageBackend>"),
            )
            .field(
                "session_store",
                &self.session_store.as_ref().map(|_| "<SessionStore>"),
            )
            .field(
                "tool_set_provider",
                &self.tool_set_provider.as_ref().map(|_| "<ToolSetProvider>"),
            )
            .finish()
    }
}

impl AgentKernelBuilder {
    /// Set the LLM provider.
    pub fn llm(mut self, llm: Arc<dyn ChatProvider>) -> Self {
        self.llm = Some(llm);
        self
    }

    /// Set the tool registry.
    pub fn tools(mut self, tools: ToolRegistry) -> Self {
        self.tools = Some(tools);
        self
    }

    /// Set the maximum number of LLM calls per turn.
    pub fn max_steps(mut self, n: usize) -> Self {
        self.max_steps = Some(n);
        self
    }

    /// Goal 399: set the session wall-clock budget in seconds (default 0 =
    /// unlimited, preserving the existing contract). When > 0, a turn that
    /// runs longer finishes with
    /// [`FinishReason::WallClockExceeded`](crate::agent::FinishReason::WallClockExceeded)
    /// — data, not an error (invariant #7).
    pub fn wall_timeout_secs(mut self, secs: u64) -> Self {
        self.wall_timeout_secs = secs;
        self
    }

    /// Set the maximum transcript characters before trimming.
    pub fn max_transcript_chars(mut self, n: usize) -> Self {
        self.max_transcript_chars = Some(n);
        self
    }

    /// Goal-393: test-only read access for the transcript cap — the runtime
    /// builder forwards the cap here, and `context_management` tests assert
    /// on the forwarded value.
    #[cfg(test)]
    pub(crate) fn max_transcript_chars_for_test(&self) -> Option<usize> {
        self.max_transcript_chars
    }

    /// Set the compactor for summarising old messages.
    pub fn compactor(mut self, compactor: Compactor) -> Self {
        self.compactor = Some(compactor);
        self
    }

    /// Set the microcompactor for no-LLM proactive pruning of old tool results.
    pub fn microcompactor(mut self, microcompactor: crate::compact::Microcompactor) -> Self {
        self.microcompactor = Some(microcompactor);
        self
    }

    /// Set the hook registry.
    pub fn hooks(mut self, hooks: HookRegistry) -> Self {
        self.hooks = Some(hooks);
        self
    }

    /// Set the cancellation token for graceful shutdown. When the token
    /// is cancelled, the kernel's step loop terminates with
    /// [`FinishReason::Cancelled`](crate::agent::FinishReason::Cancelled)
    /// at the next step boundary.
    pub fn shutdown_token(mut self, token: tokio_util::sync::CancellationToken) -> Self {
        self.shutdown_token = Some(token);
        self
    }

    /// Inject a storage backend. If not set, `build()` defaults to
    /// `LocalStorageBackend` rooted at the current working directory.
    pub fn with_storage(mut self, backend: Arc<dyn StorageBackend>) -> Self {
        self.storage = Some(backend);
        self
    }

    /// Inject a session hot-state store. If not set, `build()` uses
    /// `NoopSessionStore` (zero cost, no I/O).
    pub fn with_session_store(mut self, store: Arc<dyn SessionStore>) -> Self {
        self.session_store = Some(store);
        self
    }

    /// Inject a tool set provider. When set and `tools()` is NOT also called,
    /// `build()` delegates `tools` construction to `provider.build_registry()`.
    /// If `tools()` was set explicitly, that registry takes precedence.
    pub fn with_tool_set_provider(mut self, provider: Arc<dyn ToolSetProvider>) -> Self {
        self.tool_set_provider = Some(provider);
        self
    }

    /// Set the stuck-detection sliding window size.
    pub fn stuck_window(mut self, n: usize) -> Self {
        self.stuck_window = Some(n);
        self
    }

    /// Set the stuck-detection error rate threshold.
    pub fn stuck_error_rate(mut self, rate: f64) -> Self {
        self.stuck_error_rate = Some(rate);
        self
    }

    /// Build the `AgentKernel`, or return an error if required fields are missing.
    pub fn build(self) -> crate::error::Result<AgentKernel> {
        let llm = self.llm.ok_or_else(|| crate::error::Error::Config {
            message: "llm provider is required".into(),
        })?;
        // Tools: explicit registry > tool_set_provider > local default.
        let tools = if let Some(registry) = self.tools {
            registry
        } else if let Some(ref provider) = self.tool_set_provider {
            provider.build_registry()
        } else {
            ToolRegistry::local()
        };
        let max_steps = self.max_steps.unwrap_or(0);
        let hooks = self.hooks.unwrap_or_default();
        // Storage defaults: local filesystem, no-op session store.
        let storage: Arc<dyn StorageBackend> = self.storage.unwrap_or_else(|| {
            Arc::new(crate::storage::local::LocalStorageBackend::new(
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            ))
        });
        let session_store: Arc<dyn SessionStore> = self
            .session_store
            .unwrap_or_else(|| Arc::new(NoopSessionStore));
        Ok(AgentKernel {
            llm,
            tools,
            max_steps,
            max_transcript_chars: self.max_transcript_chars,
            compactor: self.compactor,
            microcompactor: self.microcompactor,
            hooks,
            shutdown_token: self.shutdown_token,
            storage,
            session_store,
            stuck_window: self.stuck_window.unwrap_or(10),
            stuck_error_rate: self.stuck_error_rate.unwrap_or(0.8),
            globs_skills: self.globs_skills,
            wall_timeout_secs: self.wall_timeout_secs,
        })
    }

    /// Goal-318: set the skills list (for Globs-mode injection).
    pub fn skills(mut self, skills: Vec<crate::skills::Skill>) -> Self {
        self.globs_skills = skills;
        self
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// Goal 385: unit tests extracted to a sibling file so `kernel.rs` fits the
// invariant #1 line budget (total-lines guard counts this file + tests).
// Pure mechanical move - no behaviour change.
#[cfg(test)]
mod tests;
