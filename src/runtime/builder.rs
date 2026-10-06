//! Builder for [`crate::runtime::AgentRuntime`].
//!
//! Kept in a child module so `runtime.rs` stays under the invariant #1
//! line budget.

use std::sync::{Arc, RwLock};

use crate::compact::Compactor;
use crate::error::Result;
use crate::event::{EventSink, NullSink};
use crate::hooks::HookRegistry;
use crate::kernel::AgentKernelBuilder;
use crate::llm::{ChatProvider, TokenUsage};
use crate::message::Message;
use crate::tools::plan_mode::{
    EnterPlanModeTool, ExitPlanModeTool, PlanApprovalGate, PlanModeRequestGate, RequestPlanModeTool,
};
use crate::tools::{TodoItem, TodoWriteTool, ToolRegistry};

use super::{AgentRuntime, CheckpointState, SessionLifecycle};

/// Builder for [`AgentRuntime`].
///
/// # Required
/// - `llm(...)` — The LLM provider.
///
/// All other methods are optional with sensible defaults.
pub struct AgentRuntimeBuilder {
    kernel_builder: AgentKernelBuilder,
    system_prompt: Option<String>,
    seed: Vec<Message>,
    streaming: bool,
    saved_event_sink: Option<Arc<dyn EventSink>>,
    compactor: Option<Compactor>,
    microcompactor: Option<crate::compact::Microcompactor>,
    /// When `true`, register `enter_plan_mode`, `exit_plan_mode`, and
    /// `request_plan_mode` tools. These tools block waiting for human
    /// approval via the plan approval gate, so they must only be registered
    /// when a live interactive channel (TUI or interactive CLI) is present
    /// to call `confirm_plan()` / `reject_plan()`. Headless and non-interactive
    /// callers must leave this `false` (the default) — the tools simply do not
    /// exist in the registry, so the model cannot invoke them.
    with_plan_mode_tools: bool,
    /// Goal-291: goal-evaluator judge tail-window size. Default 12.
    goal_eval_transcript_tail: usize,
    /// Goal-318: skills passed through to AgentKernel for Globs-mode injection.
    skills: Vec<crate::skills::Skill>,
    /// Goal-328: structured prompt segments from `assemble_system_prompt`,
    /// forwarded to the kernel for the local `ContextBreakdown` estimator.
    prompt_segments: Option<crate::system_prompt::PromptSegments>,
    /// Goal-334: optional file re-injector for post-compaction restoration
    /// of recently-read file contents as System attachments.
    file_reinjector: Option<crate::compact::FileReinjector>,
    /// Goal-335: optional skill re-injector for post-compaction restoration
    /// of invoked skill bodies as System attachments.
    skill_reinjector: Option<crate::compact::SkillReinjector>,
    /// Issue #99: retry policy for turns that fail inside `run_loop`.
    loop_retry: crate::runtime::LoopRetryPolicy,
    /// Issue #99: session directory that pending wakeups are persisted into
    /// (and cleared from). `None` keeps loop/wakeup state in memory only.
    wakeup_store_dir: Option<std::path::PathBuf>,
    /// Issue #127: the agent preset this session was assembled from. Carried
    /// into the runtime so `GET /sessions/:id` can report the effective preset
    /// without re-deriving it.
    preset_id: Option<String>,
}

impl std::fmt::Debug for AgentRuntimeBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRuntimeBuilder")
            .field("kernel_builder", &self.kernel_builder)
            .field("system_prompt", &self.system_prompt)
            .field("seed", &self.seed)
            .field("streaming", &self.streaming)
            .field(
                "event_sink",
                &self.saved_event_sink.as_ref().map(|_| "<EventSink>"),
            )
            .field("goal_eval_transcript_tail", &self.goal_eval_transcript_tail)
            .field("file_reinjector", &self.file_reinjector.is_some())
            .field("skill_reinjector", &self.skill_reinjector.is_some())
            .field("preset_id", &self.preset_id)
            .finish()
    }
}

impl Default for AgentRuntimeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentRuntimeBuilder {
    /// Create a new builder with default values.
    pub fn new() -> Self {
        Self {
            kernel_builder: AgentKernelBuilder::default(),
            system_prompt: None,
            seed: Vec::new(),
            streaming: false,
            saved_event_sink: None,
            compactor: None,
            microcompactor: None,
            with_plan_mode_tools: false,
            goal_eval_transcript_tail: 12,
            skills: Vec::new(),
            prompt_segments: None,
            file_reinjector: None,
            skill_reinjector: None,
            loop_retry: crate::runtime::LoopRetryPolicy::default(),
            wakeup_store_dir: None,
            preset_id: None,
        }
    }

    /// Goal-328: forward structured prompt segments to the kernel so the
    /// local `ContextBreakdown` estimator can size the static buckets
    /// (`system_prompt`, `rules`, `skills`, `subagents`, `tools`,
    /// `mcp_dynamic`). Callers that built the prompt via
    /// [`crate::assemble_system_prompt`] should chain the returned
    /// `segments` through this method:
    ///
    /// ```ignore
    /// let assembled = assemble_system_prompt(base, ws, &skills, sub);
    /// let mut builder = AgentRuntimeBuilder::new()
    ///     .system_prompt(assembled.full())
    ///     .prompt_segments(assembled.segments);
    /// ```
    pub fn prompt_segments(mut self, segments: crate::system_prompt::PromptSegments) -> Self {
        self.prompt_segments = Some(segments);
        self
    }

    /// Register `enter_plan_mode`, `exit_plan_mode`, and `request_plan_mode`
    /// tools. Call this only from channels that have a live human reviewer
    /// (TUI, interactive CLI). Headless and batch callers must NOT set this —
    /// the tools block indefinitely waiting for `confirm_plan()`.
    pub fn with_plan_mode_tools(mut self, enabled: bool) -> Self {
        self.with_plan_mode_tools = enabled;
        self
    }

    /// Set the LLM provider (required).
    pub fn llm(mut self, llm: Arc<dyn ChatProvider>) -> Self {
        self.kernel_builder = self.kernel_builder.llm(llm);
        self
    }

    /// Set the tool registry (optional, defaults to a local empty registry).
    pub fn tools(mut self, tools: ToolRegistry) -> Self {
        self.kernel_builder = self.kernel_builder.tools(tools);
        self
    }

    /// Goal-318: set the skills list for Globs-mode automatic injection.
    pub fn skills(mut self, skills: Vec<crate::skills::Skill>) -> Self {
        self.skills = skills;
        self
    }

    /// Set an initial system prompt (optional).
    ///
    /// This is prepended to the transcript as the first message.
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Set the maximum number of LLM calls per turn (optional, default 0 (unlimited)).
    pub fn max_steps(mut self, n: usize) -> Self {
        self.kernel_builder = self.kernel_builder.max_steps(n);
        self
    }

    /// Goal 399: set the session wall-clock budget in seconds, forwarded to
    /// the kernel builder (optional, default 0 = unlimited, same contract as
    /// `max_steps`). When > 0, a turn that exceeds the budget finishes with
    /// `FinishReason::WallClockExceeded` — data, not an error (invariant #7).
    pub fn wall_timeout_secs(mut self, secs: u64) -> Self {
        self.kernel_builder = self.kernel_builder.wall_timeout_secs(secs);
        self
    }

    /// Issue #100: override the cross-step retry policy for transient
    /// provider failures (429 / 5xx / network). Forwarded to the kernel
    /// builder; when unset, `RetryPolicy::for_step_loop_from_env` applies.
    pub fn step_retry(mut self, policy: crate::llm::RetryPolicy) -> Self {
        self.kernel_builder = self.kernel_builder.step_retry(policy);
        self
    }

    /// Issue #94: cap this session's API spend per turn, forwarded to the
    /// kernel builder. `max_budget_usd` is the ceiling (`None` / non-positive
    /// = no cap) and `pricing` is the configured model's rate card
    /// (`None` = unpriced model → the guard degrades to a token ceiling).
    /// Reaching the ceiling finishes the turn with
    /// `FinishReason::BudgetExceeded` — data, not an error (invariant #7).
    pub fn cost_budget(
        mut self,
        max_budget_usd: Option<f64>,
        pricing: Option<crate::llm::ModelPricing>,
    ) -> Self {
        self.kernel_builder = self.kernel_builder.cost_budget(max_budget_usd, pricing);
        self
    }

    /// Set a transcript character limit (optional, default unlimited).
    pub fn max_transcript_chars(mut self, n: usize) -> Self {
        self.kernel_builder = self.kernel_builder.max_transcript_chars(n);
        self
    }

    /// Set an optional compactor for summarising old messages.
    pub fn compactor(mut self, compactor: Compactor) -> Self {
        // Also pass the compactor to the kernel so `RunCore` can perform
        // intra-turn compaction (which dispatches `PreCompact` / `PostCompact`
        // hooks). Cross-turn compaction is performed by the runtime itself.
        self.kernel_builder = self.kernel_builder.compactor(compactor.clone());
        self.compactor = Some(compactor);
        self
    }

    /// Set an optional microcompactor for no-LLM proactive pruning of old
    /// tool results by count.
    pub fn microcompactor(mut self, microcompactor: crate::compact::Microcompactor) -> Self {
        self.kernel_builder = self.kernel_builder.microcompactor(microcompactor.clone());
        self.microcompactor = Some(microcompactor);
        self
    }

    // Goal-393: test-only read access so `context_management` tests can
    // assert what a frontend assembly installed without building a runtime
    // (`AgentRuntime` has no public accessors on purpose — keep it that way
    // instead of growing `runtime.rs` for tests).
    /// Inspect the compactor installed by a builder chain (tests only).
    #[cfg(test)]
    pub(crate) fn compactor_for_test(&self) -> Option<&Compactor> {
        self.compactor.as_ref()
    }

    /// Inspect the microcompactor installed by a builder chain (tests only).
    #[cfg(test)]
    pub(crate) fn microcompactor_for_test(&self) -> Option<&crate::compact::Microcompactor> {
        self.microcompactor.as_ref()
    }

    /// Inspect the transcript cap installed by a builder chain (tests only).
    #[cfg(test)]
    pub(crate) fn max_transcript_chars_for_test(&self) -> Option<usize> {
        self.kernel_builder.max_transcript_chars_for_test()
    }

    /// Inspect the skill catalog a builder chain installed (tests only) —
    /// the kernel serves this list as the per-turn skill reminder and the
    /// Globs-mode injector.
    #[cfg(test)]
    pub(crate) fn skills_for_test(&self) -> &[crate::skills::Skill] {
        &self.skills
    }

    /// Inspect whether a builder chain asked for the plan-mode tools
    /// (tests only). The registry mutation itself happens in `build()`.
    #[cfg(test)]
    pub(crate) fn with_plan_mode_tools_for_test(&self) -> bool {
        self.with_plan_mode_tools
    }

    /// Issue #127: stamp the agent preset this session is assembled from.
    pub fn with_preset_id(mut self, id: String) -> Self {
        self.preset_id = Some(id);
        self
    }

    /// The agent preset id this builder carries, if any.
    pub fn preset_id(&self) -> Option<&str> {
        self.preset_id.as_deref()
    }

    /// Issue #127: the context management this builder chain installed, as
    /// plain data. Lets a frontend (or a test) compare what it assembled
    /// against the resolved preset without reaching into the runtime.
    pub fn context_management_facts(&self) -> crate::preset::ContextFacts {
        crate::preset::ContextFacts {
            compaction: self
                .compactor
                .as_ref()
                .map(|c| crate::preset::CompactionFacts {
                    threshold_chars: c.threshold_chars,
                    threshold_prompt_tokens: c.threshold_prompt_tokens,
                    keep_recent_n: c.keep_recent_n,
                }),
            microcompaction: self.microcompactor.as_ref().map(|mc| {
                crate::preset::MicrocompactionSpec {
                    trigger_tool_count: mc.trigger_tool_count,
                    keep_recent: mc.keep_recent,
                }
            }),
            max_transcript_chars: self.kernel_builder.max_transcript_chars_cap(),
            reinject_recent_files: self.file_reinjector.as_ref().map(|r| {
                crate::preset::FileReinjectSpec {
                    max_files: r.max_files,
                    token_budget: r.token_budget,
                    per_file_budget: r.per_file_budget,
                }
            }),
            reinject_invoked_skills: self.skill_reinjector.as_ref().map(|r| {
                crate::preset::SkillReinjectSpec {
                    token_budget: r.token_budget,
                    per_skill_budget: r.per_skill_budget,
                }
            }),
        }
    }

    /// Goal 396: inject a storage backend, forwarded to the kernel builder
    /// (same forwarding pattern as `compactor`). The HTTP host layer shares
    /// the same `Arc` and persists transcripts on session close/eviction —
    /// the kernel itself does NOT save per-turn (that would be an O(N²)
    /// full-transcript write on the hot path). `recursive http` picks the
    /// backend with [`crate::storage::http_storage_backend`] (issue #92).
    pub fn storage(mut self, storage: Arc<dyn crate::storage::StorageBackend>) -> Self {
        self.kernel_builder = self.kernel_builder.with_storage(storage);
        self
    }

    /// Goal 396: inject a session hot-state store, forwarded to the kernel
    /// builder. The local default stays [`crate::storage::NoopSessionStore`]
    /// (zero cost). `recursive http` does not inject Redis yet — the kernel
    /// owns the injection point but never checkpoints per turn, so an HTTP
    /// store would be a no-op (issue #92).
    pub fn session_store(mut self, store: Arc<dyn crate::storage::SessionStore>) -> Self {
        self.kernel_builder = self.kernel_builder.with_session_store(store);
        self
    }

    /// Enable or disable streaming of partial tokens (optional, default false).
    pub fn streaming(mut self, enabled: bool) -> Self {
        self.streaming = enabled;
        self
    }

    /// Set the hook registry (optional).
    pub fn hooks(mut self, hooks: HookRegistry) -> Self {
        self.kernel_builder = self.kernel_builder.hooks(hooks);
        self
    }

    /// Seed the transcript with messages from a previous session.
    ///
    /// These messages are placed after any system prompt, before the
    /// first user turn. Use this to resume an existing conversation.
    pub fn seed_transcript(mut self, messages: Vec<Message>) -> Self {
        self.seed = messages;
        self
    }

    /// Set the event sink for streaming events (optional, defaults to [`NullSink`]).
    pub fn event_sink(mut self, sink: Arc<dyn EventSink>) -> Self {
        self.saved_event_sink = Some(sink);
        self
    }

    /// Set the cancellation token for graceful shutdown. When the token
    /// is cancelled, the runtime's underlying kernel terminates the
    /// step loop with
    /// [`FinishReason::Cancelled`](crate::agent::FinishReason::Cancelled)
    /// at the next step boundary.
    pub fn shutdown_token(mut self, token: tokio_util::sync::CancellationToken) -> Self {
        self.kernel_builder = self.kernel_builder.shutdown_token(token);
        self
    }

    /// Set the stuck-detection sliding window size.
    pub fn stuck_window(mut self, n: usize) -> Self {
        self.kernel_builder = self.kernel_builder.stuck_window(n);
        self
    }

    /// Set the stuck-detection error rate threshold.
    pub fn stuck_error_rate(mut self, rate: f64) -> Self {
        self.kernel_builder = self.kernel_builder.stuck_error_rate(rate);
        self
    }

    /// Set the tail-window size for the goal-evaluator judge.
    ///
    /// Each turn, the goal loop calls
    /// [`GoalEvaluator::evaluate`](crate::runtime_goal::GoalEvaluator::evaluate)
    /// with the most-recent `n` transcript messages. Smaller values reduce
    /// judge cost; larger values give the judge more context for long
    /// sessions. Defaults to 12 (matching the previous hard-coded
    /// `GOAL_EVAL_TRANSCRIPT_TAIL` constant). Goal-291.
    pub fn goal_eval_transcript_tail(mut self, n: usize) -> Self {
        self.goal_eval_transcript_tail = n;
        self
    }

    /// Set an optional file re-injector for post-compaction restoration
    /// of recently-read file contents as System attachments.
    pub fn file_reinjector(mut self, r: crate::compact::FileReinjector) -> Self {
        self.file_reinjector = Some(r);
        self
    }

    /// Set an optional skill re-injector for post-compaction restoration.
    pub fn skill_reinjector(mut self, r: crate::compact::SkillReinjector) -> Self {
        self.skill_reinjector = Some(r);
        self
    }

    /// Issue #99: override the retry budget `run_loop` uses for a turn that
    /// failed with a transient error. Defaults to
    /// [`crate::runtime::LoopRetryPolicy::default`].
    pub fn loop_retry(mut self, policy: crate::runtime::LoopRetryPolicy) -> Self {
        self.loop_retry = policy;
        self
    }

    /// Issue #99: persist the pending wakeup into `dir` (normally the
    /// session directory) so a restarted process can restore it instead of
    /// silently dropping it. Callers without session recording simply leave
    /// this unset.
    pub fn wakeup_store_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.wakeup_store_dir = Some(dir.into());
        self
    }

    /// Build the [`AgentRuntime`].
    ///
    /// Returns an error if the LLM provider is missing.
    pub fn build(self) -> Result<AgentRuntime> {
        let kernel_builder = self.kernel_builder.skills(self.skills);
        let mut kernel = kernel_builder.build()?;

        let mut transcript = Vec::new();
        if let Some(sys) = self.system_prompt {
            transcript.push(Message::system(sys));
        }
        transcript.extend(self.seed);

        let event_sink: Arc<dyn EventSink> =
            self.saved_event_sink.unwrap_or_else(|| Arc::new(NullSink));

        // Goal-167: create the shared todo list and register a properly-sinked
        // TodoWriteTool, overriding the NullSink version from build_standard_tools.
        //
        // Issue #65: skip the re-register only when a surface filter
        // deliberately dropped TodoWrite — `retain_tools` marks the registry
        // (`surface_filtered`), so "placeholder present" or "never filtered"
        // both keep the legacy always-registered behavior (default
        // `AgentRuntime::builder()` runtimes carry an EMPTY local registry,
        // not a filtered one), while an operator allow-list without TodoWrite
        // stays strict through build. The `todo_list` arc is still created
        // unconditionally: compaction's PlanTodoReinjector and
        // `AgentRuntime::todo_list` share it regardless of tool presence.
        let todo_list = Arc::new(RwLock::new(Vec::<TodoItem>::new()));
        if kernel.tools().find_by_name("TodoWrite").is_some() || !kernel.tools().surface_filtered()
        {
            kernel.tools_mut().register_mut(Arc::new(TodoWriteTool::new(
                todo_list.clone(),
                event_sink.clone(),
            )));
        }

        // Goal #133: the deliverables ledger is created by the registry
        // (which knows the workspace); the runtime picks up the very same
        // `Arc` so the tools that record declarations and the per-turn
        // begin/finalize bookkeeping cannot drift apart. `None` means the
        // subsystem is off for this session — no ledger runs and no
        // `ChangeLedger` event is emitted.
        let deliverables = kernel.tools().deliverables();
        if let Some(ledger) = &deliverables {
            // Same surface-filter rule as TodoWrite: re-register the
            // NullSink placeholder with the live sink, but never resurrect a
            // tool an explicit allow-list dropped.
            let present_kept = kernel
                .tools()
                .find_by_name(crate::tools::PRESENT_TOOL_NAME)
                .is_some();
            let ledger_kept = kernel
                .tools()
                .find_by_name(crate::tools::CHANGE_LEDGER_TOOL_NAME)
                .is_some();
            let filtered = kernel.tools().surface_filtered();
            if present_kept || !filtered {
                kernel
                    .tools_mut()
                    .register_mut(Arc::new(crate::tools::PresentTool::new(
                        ledger.clone(),
                        event_sink.clone(),
                    )));
            }
            // Both tools are bound to the runtime's ledger, so a registry
            // whose ledger was swapped after registration cannot leave
            // `ChangeLedger` rendering a stale one.
            if ledger_kept || !filtered {
                kernel
                    .tools_mut()
                    .register_mut(Arc::new(crate::tools::ChangeLedgerTool::new(
                        ledger.clone(),
                    )));
            }
        }

        // Goal-165 / Goal-202: plan mode tools block waiting for human approval
        // via the gate. They must only be registered when a live interactive
        // channel (TUI or interactive CLI) is present to call confirm_plan().
        // Headless / batch callers set with_plan_mode_tools = false (the default)
        // so the model never sees these tools and cannot trigger a deadlock.
        let plan_approval_gate = Arc::new(PlanApprovalGate::new());
        let plan_mode_request_gate = Arc::new(PlanModeRequestGate::new());
        // Issue #65: `with_plan_mode_tools` may only ADD to a surface that
        // was never explicitly filtered — a `retain_tools` allow-list
        // (operator or coordinator prune) that dropped the plan tools stays
        // strict even on interactive channels.
        if self.with_plan_mode_tools && !kernel.tools().surface_filtered() {
            let permissions_arc = kernel.tools().permissions_config().map(Arc::new);
            kernel.tools_mut().register_mut({
                let mut tool = EnterPlanModeTool::new(plan_approval_gate.clone());
                if let Some(ref perms) = permissions_arc {
                    tool = tool.with_permissions(perms.clone());
                }
                Arc::new(tool)
            });
            kernel.tools_mut().register_mut({
                let mut tool =
                    ExitPlanModeTool::new(plan_approval_gate.clone(), event_sink.clone());
                if let Some(ref perms) = permissions_arc {
                    tool = tool.with_permissions(perms.clone());
                }
                Arc::new(tool)
            });
            kernel
                .tools_mut()
                .register_mut(Arc::new(RequestPlanModeTool::new(
                    plan_mode_request_gate.clone(),
                    event_sink.clone(),
                )));
        }

        // Goal-340: plan/todo re-injector shares the same todo_list and
        // plan_approval_gate arcs already constructed above.
        let plan_todo_reinjector = Some(crate::compact::PlanTodoReinjector::new(
            todo_list.clone(),
            plan_approval_gate.clone(),
        ));

        // Register ToolSearchTool only when the provider supports deferred
        // tool loading via tool_reference (Anthropic API feature).
        // OpenAI and compatible providers get all tools eagerly.
        if kernel.llm().supports_deferred_tools() {
            kernel.tools_mut().freeze_deferred_specs();
        }

        Ok(AgentRuntime {
            kernel,
            transcript: Arc::new(transcript),
            event_sink,
            streaming: self.streaming,
            compactor: self.compactor,
            microcompactor: self.microcompactor,
            consecutive_compact_failures: 0,
            checkpoints: CheckpointState::disabled(),
            todo_list,
            plan_approval_gate,
            approval_wait_timeout_secs: None,
            plan_approval_interrupt_token: None,
            plan_mode_request_gate,
            goal_state: Arc::new(RwLock::new(None)),
            message_queue: std::collections::VecDeque::new(),
            deferred_turn_finished: None,
            session: SessionLifecycle::open(),
            goal_eval_transcript_tail: self.goal_eval_transcript_tail,
            prompt_segments: self.prompt_segments,
            file_reinjector: self.file_reinjector,
            skill_reinjector: self.skill_reinjector,
            plan_todo_reinjector,
            last_compact_turn: None,
            permission_hook: None,
            loop_retry: self.loop_retry,
            wakeup_store_dir: self.wakeup_store_dir,
            preset_id: self.preset_id,
            deliverables,
            last_failed_usage: TokenUsage::default(),
            pending_compact_usage: TokenUsage::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::MockProvider;
    use crate::message::Role;

    fn mock_llm() -> Arc<dyn ChatProvider> {
        Arc::new(MockProvider::new(vec![]))
    }

    /// Goal 396: minimal fake backend that only records its own identity —
    /// the assertion is Arc pointer equality after the build, not I/O.
    #[derive(Default)]
    struct FakeStorage;

    #[async_trait::async_trait]
    impl crate::storage::StorageBackend for FakeStorage {
        async fn load_transcript(&self, _session_id: &str) -> crate::error::Result<Vec<Message>> {
            Ok(vec![])
        }
        async fn save_transcript(
            &self,
            _session_id: &str,
            _messages: &[Message],
        ) -> crate::error::Result<()> {
            Ok(())
        }
        async fn load_memory(&self, _key: &str) -> crate::error::Result<Option<String>> {
            Ok(None)
        }
        async fn save_memory(&self, _key: &str, _value: &str) -> crate::error::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn build_with_minimum_config_succeeds() {
        let rt = AgentRuntimeBuilder::new()
            .llm(mock_llm())
            .build()
            .expect("build() with just an LLM must succeed");

        // Sane defaults pinned: max_steps 0 (unlimited), no compactor,
        // streaming off, empty transcript, checkpoints disabled.
        assert_eq!(rt.kernel.max_steps, 0);
        assert_eq!(rt.kernel.max_transcript_chars, None);
        assert!(rt.compactor.is_none());
        assert!(rt.microcompactor.is_none());
        assert!(!rt.streaming);
        assert!(rt.transcript.is_empty());
        assert!(!rt.checkpoints.enabled());
        assert_eq!(rt.goal_eval_transcript_tail, 12);
    }

    /// Issue #65: `build()` must not re-add TodoWrite when the caller's
    /// registry deliberately excluded it (operator allow-list) — but the
    /// legacy always-registered behavior stays for the default path, whose
    /// registry is EMPTY (`ToolRegistry::local()`), not filtered. The
    /// `retain_tools` marker (`surface_filtered`) separates the two.
    #[test]
    fn build_does_not_reinject_a_filtered_todo_write() {
        // Default path (no explicit registry): empty local registry, never
        // filtered → TodoWrite is registered as before (P0-2 contract).
        let rt = AgentRuntimeBuilder::new()
            .llm(mock_llm())
            .build()
            .expect("build() with the default registry must succeed");
        assert!(
            rt.kernel.tools().find_by_name("TodoWrite").is_some(),
            "default-path runtimes keep TodoWrite"
        );

        // Explicit unfiltered registry: the NullSink placeholder is replaced
        // by the properly-sinked tool — name still present.
        let std_registry = crate::tools::build_standard_tools(std::path::Path::new("."), &[], 30);
        assert!(
            std_registry.find_by_name("TodoWrite").is_some(),
            "fixture needs the unfiltered registry to carry TodoWrite"
        );
        let rt = AgentRuntimeBuilder::new()
            .llm(mock_llm())
            .tools(std_registry)
            .build()
            .expect("build() with a standard registry must succeed");
        assert!(
            rt.kernel.tools().find_by_name("TodoWrite").is_some(),
            "unfiltered registries keep TodoWrite"
        );

        // Filtered (RECURSIVE_ALLOW_TOOLS without TodoWrite): build() must
        // NOT sneak it back in.
        let mut filtered = crate::tools::build_standard_tools(std::path::Path::new("."), &[], 30);
        filtered.retain_tools(&["Read".to_string()]);
        let rt = AgentRuntimeBuilder::new()
            .llm(mock_llm())
            .tools(filtered)
            .build()
            .expect("build() with a filtered registry must succeed");
        assert!(
            rt.kernel.tools().find_by_name("TodoWrite").is_none(),
            "a TodoWrite filtered out by the allow-list must not be re-added at build time"
        );
        assert!(
            rt.kernel.tools().find_by_name("Read").is_some(),
            "allow-listed tools survive the build"
        );
    }

    #[test]
    fn build_without_llm_errors() {
        let result = AgentRuntimeBuilder::new().build();
        let err = result.expect_err("build() without an LLM must fail");
        assert!(
            matches!(err, crate::error::Error::Config { .. }),
            "expected Error::Config, got {err:?}"
        );
    }

    #[test]
    fn builder_setters_round_trip() {
        let compactor = Compactor::new(1000);
        let rt = AgentRuntimeBuilder::new()
            .llm(mock_llm())
            .max_steps(7)
            .max_transcript_chars(5000)
            .compactor(compactor)
            .streaming(true)
            .stuck_window(4)
            .seed_transcript(vec![
                Message::user("seed user"),
                Message::assistant("seed assistant"),
            ])
            .build()
            .expect("build() with setters must succeed");

        assert_eq!(rt.kernel.max_steps, 7);
        assert_eq!(rt.kernel.max_transcript_chars, Some(5000));
        assert!(rt.compactor.is_some());
        assert!(rt.streaming);
        assert_eq!(rt.kernel.stuck_window, 4);
        // Seeded transcript appears verbatim (no system prompt set here).
        assert_eq!(rt.transcript.len(), 2);
        assert_eq!(rt.transcript[0].role, Role::User);
        assert_eq!(rt.transcript[1].role, Role::Assistant);
        assert_eq!(rt.transcript[0].content, "seed user");
        assert_eq!(rt.transcript[1].content, "seed assistant");
    }

    #[test]
    fn file_reinjector_and_skill_reinjector_wire_through() {
        let read_state = Arc::new(std::sync::Mutex::new(crate::tools::fs::ReadFileState::new()));
        let rt = AgentRuntimeBuilder::new()
            .llm(mock_llm())
            .file_reinjector(crate::compact::FileReinjector::new(read_state))
            .skill_reinjector(crate::compact::SkillReinjector::new(vec![]))
            .build()
            .expect("build() with reinjectors must succeed");

        assert!(rt.file_reinjector.is_some());
        assert!(rt.skill_reinjector.is_some());
        // Goal-340: build() always wires the plan/todo reinjector.
        assert!(rt.plan_todo_reinjector.is_some());
    }

    /// Goal 396: `storage(...)` / `session_store(...)` must reach the kernel
    /// — same Arc, not a copy or a default. Without this the host layer's
    /// save path and the kernel's storage would silently diverge.
    #[test]
    fn storage_and_session_store_forward_to_kernel() {
        let storage: Arc<dyn crate::storage::StorageBackend> = Arc::new(FakeStorage);
        let store: Arc<dyn crate::storage::SessionStore> =
            Arc::new(crate::storage::NoopSessionStore);

        let rt = AgentRuntimeBuilder::new()
            .llm(mock_llm())
            .storage(storage.clone())
            .session_store(store.clone())
            .build()
            .expect("build() with storage must succeed");

        assert!(
            Arc::ptr_eq(&storage, &rt.kernel().storage),
            "kernel.storage must be the exact Arc passed to the builder"
        );
        assert!(
            Arc::ptr_eq(&store, &rt.kernel().session_store),
            "kernel.session_store must be the exact Arc passed to the builder"
        );
    }
}
