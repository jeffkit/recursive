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
use crate::llm::ChatProvider;
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

    /// Set the maximum number of LLM calls per turn (optional, default 32).
    pub fn max_steps(mut self, n: usize) -> Self {
        self.kernel_builder = self.kernel_builder.max_steps(n);
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
        let todo_list = Arc::new(RwLock::new(Vec::<TodoItem>::new()));
        kernel.tools_mut().register_mut(Arc::new(TodoWriteTool::new(
            todo_list.clone(),
            event_sink.clone(),
        )));

        // Goal-165 / Goal-202: plan mode tools block waiting for human approval
        // via the gate. They must only be registered when a live interactive
        // channel (TUI or interactive CLI) is present to call confirm_plan().
        // Headless / batch callers set with_plan_mode_tools = false (the default)
        // so the model never sees these tools and cannot trigger a deadlock.
        let plan_approval_gate = Arc::new(PlanApprovalGate::new());
        let plan_mode_request_gate = Arc::new(PlanModeRequestGate::new());
        if self.with_plan_mode_tools {
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
        })
    }
}
