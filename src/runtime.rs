//! High-level stateful agent runtime.
//!
//! Wraps the stateless [`AgentKernel`] and manages cross-turn state:
//! transcript accumulation, usage tracking, and configuration that
//! varies per turn (streaming, planning mode, permission hook, event sink).
//!
//! # Example
//!
//! ```ignore
//! use recursive::{AgentRuntime, AgentRuntimeBuilder, NullSink};
//!
//! let mut rt = AgentRuntimeBuilder::new()
//!     .llm(my_llm)
//!     .tools(my_tools)
//!     .system_prompt("You are a helpful assistant.")
//!     .build()
//!     .unwrap();
//!
//! let outcome = rt.run("What is the weather?").await.unwrap();
//! println!("{}", outcome.final_text.unwrap_or_default());
//! ```

use std::sync::Arc;

use crate::agent::{FinishReason, PermissionHook, PlanningMode};
use crate::error::Result;
use crate::event::{EventSink, NullSink};
use crate::hooks::HookRegistry;
use crate::kernel::{AgentKernel, AgentKernelBuilder, TurnContext, TurnOutcome};
use crate::llm::{LlmProvider, TokenUsage};
use crate::message::Message;
use crate::tools::ToolRegistry;
use crate::Compactor;

// ──────────────────────────────────────────────────────────────────────────
// RuntimeOutcome
// ──────────────────────────────────────────────────────────────────────────

/// The result of a single [`AgentRuntime::run()`] turn.
///
/// Contains the model's final text (if any), how the turn ended,
/// token usage for this turn, the number of LLM steps taken, and
/// the LLM latency in milliseconds.
#[derive(Debug, Clone)]
pub struct RuntimeOutcome {
    /// The final assistant text, if the model produced one.
    pub final_text: Option<String>,
    /// Why the turn stopped.
    pub finish_reason: FinishReason,
    /// Token usage for this turn only.
    pub total_usage: TokenUsage,
    /// Number of LLM calls made during this turn.
    pub steps: usize,
    /// Measured LLM latency for this turn (milliseconds).
    pub llm_latency_ms: u64,
}

impl From<TurnOutcome> for RuntimeOutcome {
    fn from(t: TurnOutcome) -> Self {
        Self {
            final_text: t.final_text,
            finish_reason: t.finish_reason,
            total_usage: t.usage,
            steps: t.steps,
            llm_latency_ms: t.llm_latency_ms,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────
// AgentRuntime
// ──────────────────────────────────────────────────────────────────────────

/// A stateful agent runtime that wraps [`AgentKernel`].
///
/// `AgentRuntime` owns the conversation transcript and all cross-turn
/// configuration. Each call to [`run`](AgentRuntime::run) appends a user
/// message to the transcript, delegates to the kernel for one turn, and
/// appends the kernel's new messages back to the transcript.
pub struct AgentRuntime {
    /// The stateless kernel that executes each turn.
    kernel: AgentKernel,
    /// Accumulated conversation transcript.
    transcript: Vec<Message>,
    /// Event sink for streaming events (Arc for sharing with forwarder task).
    event_sink: Arc<dyn EventSink>,
    /// Pending plan tool calls buffered by the kernel (plan-first mode).
    pending_plan_calls: Option<Vec<crate::llm::ToolCall>>,
    /// Whether the user confirmed the pending plan.
    plan_confirmed: bool,
    /// Whether to request streaming responses from the LLM.
    streaming: bool,
    /// Optional permission hook for tool-call interception.
    permission_hook: Option<PermissionHook>,
    /// Planning mode (immediate vs plan-first).
    planning_mode: PlanningMode,
    /// Optional compactor for cross-turn transcript summarization.
    compactor: Option<Compactor>,
}

impl std::fmt::Debug for AgentRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRuntime")
            .field("kernel", &self.kernel)
            .field("transcript", &self.transcript)
            .field("event_sink", &"<EventSink>")
            .field("streaming", &self.streaming)
            .field(
                "permission_hook",
                &self.permission_hook.as_ref().map(|_| "<hook>"),
            )
            .field("planning_mode", &self.planning_mode)
            .finish()
    }
}

impl AgentRuntime {
    /// Create a new [`AgentRuntimeBuilder`].
    pub fn builder() -> AgentRuntimeBuilder {
        AgentRuntimeBuilder::new()
    }

    /// Run one turn with the given user text.
    ///
    /// Appends `Message::user(text)` to the transcript, builds a
    /// [`TurnContext`], delegates to the kernel, appends the kernel's
    /// new messages to the transcript, and returns a [`RuntimeOutcome`].
    pub async fn run(&mut self, user_text: impl Into<String>) -> Result<RuntimeOutcome> {
        // Append user message
        self.transcript.push(Message::user(user_text.into()));

        // Cross-turn compaction: summarize old messages if transcript is too large.
        // This is the Wrapper's responsibility — the kernel only does intra-turn trim.
        if let Some(ref compactor) = self.compactor {
            let chars = Compactor::estimate_chars(&self.transcript);
            if chars >= compactor.threshold_chars
                && self.transcript.len() >= compactor.keep_recent_n + 2
            {
                let summary_msg = compactor
                    .compact(self.kernel.llm().as_ref(), &self.transcript)
                    .await?;
                let keep = compactor.keep_recent_n;
                let mut split = self.transcript.len().saturating_sub(keep);
                while split > 0 && matches!(self.transcript[split].role, crate::message::Role::Tool)
                {
                    split -= 1;
                }
                self.transcript.drain(..split);
                self.transcript.insert(0, summary_msg);
            }
        }

        // Create AgentEvent channel; kernel converts StepEvent → AgentEvent internally.
        let (event_tx, mut event_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::event::AgentEvent>();
        let sink = self.event_sink.clone();
        let forwarder = tokio::spawn(async move {
            while let Some(ev) = event_rx.recv().await {
                sink.emit(ev).await;
            }
        });

        // Build turn context with the AgentEvent channel.
        let ctx = TurnContext {
            messages: self.transcript.clone(),
            tool_specs: self.kernel.tools().specs(),
            event_sink: None,
            step_events_tx: Some(event_tx.clone()),
            plan_confirmed: self.plan_confirmed,
            plan_buffer: self.pending_plan_calls.clone(),
            streaming: self.streaming,
            permission_hook: self.permission_hook.clone(),
            planning_mode: self.planning_mode.clone(),
        };

        // Execute turn
        let turn_outcome = self.kernel.run(ctx).await?;

        // Drop the sender to signal the forwarder to stop, then wait for it.
        drop(event_tx);
        forwarder.await.ok();

        // Handle plan confirmation state
        if turn_outcome.finish_reason == crate::agent::FinishReason::PlanPending {
            // Store pending plan calls from the kernel's response
            self.pending_plan_calls = turn_outcome.plan_buffer.clone();
        }

        // Reset plan confirmation state after the turn
        let was_confirmed = self.plan_confirmed;
        self.plan_confirmed = false;
        if was_confirmed {
            // If plan was confirmed, clear the pending calls
            self.pending_plan_calls = None;
        }

        // Append new messages to transcript
        self.transcript.extend(turn_outcome.new_messages.clone());

        Ok(turn_outcome.into())
    }

    /// Return a reference to the accumulated transcript.
    pub fn transcript(&self) -> &[Message] {
        &self.transcript
    }

    /// Replace the current transcript (useful for restoring from a saved session).
    pub fn set_transcript(&mut self, transcript: Vec<Message>) {
        self.transcript = transcript;
    }

    /// Return a reference to the inner kernel.
    pub fn kernel(&self) -> &AgentKernel {
        &self.kernel
    }

    /// Return the event sink currently in use.
    pub fn event_sink(&self) -> &dyn EventSink {
        self.event_sink.as_ref()
    }

    /// Set a new event sink (useful for REPL mode between turns).
    pub fn set_event_sink(&mut self, sink: Arc<dyn EventSink>) {
        self.event_sink = sink;
    }

    /// Confirm the pending plan, allowing execution to proceed on the next run.
    pub fn confirm_plan(&mut self) {
        self.plan_confirmed = true;
    }

    /// Reject the pending plan with a reason.
    ///
    /// This injects a tool error message into the transcript to inform the agent
    /// that the plan was rejected.
    pub fn reject_plan(&mut self, reason: &str) {
        // Clear pending plan calls
        self.pending_plan_calls = None;
        self.plan_confirmed = false;

        // Inject a user message with the rejection into the transcript
        let rejection_msg = Message::user(format!("Plan rejected: {}", reason));
        self.transcript.push(rejection_msg);
    }

    /// Run a loop: execute turns until the agent stops scheduling wakeups.
    ///
    /// Between turns, sleeps for the requested `delay`. If the agent doesn't
    /// call `schedule_wakeup` during a turn, the loop ends.
    ///
    /// The `wakeup_slot` should be the same slot registered with the
    /// `ScheduleWakeup` tool in the agent's tool registry.
    pub async fn run_loop(
        &mut self,
        initial_goal: impl Into<String>,
        wakeup_slot: &crate::tools::WakeupSlot,
    ) -> Result<Vec<RuntimeOutcome>> {
        let mut outcomes = Vec::new();
        let mut next_goal = initial_goal.into();

        loop {
            let outcome = self.run(&next_goal).await?;
            outcomes.push(outcome);

            // Check if the agent scheduled a wakeup
            let wakeup = wakeup_slot.lock().ok().and_then(|mut slot| slot.take());

            match wakeup {
                Some(req) => {
                    tokio::time::sleep(req.delay).await;
                    next_goal = req.prompt;
                }
                None => break,
            }
        }
        Ok(outcomes)
    }

    /// Run a loop with background job awareness.
    ///
    /// After each turn, checks both:
    /// 1. The `WakeupSlot` for an explicit wakeup request
    /// 2. The `BackgroundJobManager` for completed jobs
    ///
    /// If a background job completed, its output is injected as the next turn's
    /// goal. If a wakeup was scheduled, the runtime sleeps for the requested
    /// delay then continues. If neither is present, the loop ends.
    pub async fn run_event_loop(
        &mut self,
        initial_goal: impl Into<String>,
        wakeup_slot: &crate::tools::WakeupSlot,
        bg_manager: Option<&tokio::sync::Mutex<crate::tools::BackgroundJobManager>>,
    ) -> Result<Vec<RuntimeOutcome>> {
        let mut outcomes = Vec::new();
        let mut next_goal = initial_goal.into();

        loop {
            let outcome = self.run(&next_goal).await?;
            outcomes.push(outcome);

            // Priority 1: explicit wakeup
            let wakeup = wakeup_slot.lock().ok().and_then(|mut slot| slot.take());
            if let Some(req) = wakeup {
                tokio::time::sleep(req.delay).await;
                next_goal = req.prompt;
                continue;
            }

            // Priority 2: background job completed
            if let Some(mgr) = bg_manager {
                if let Ok(mut mgr) = mgr.try_lock() {
                    if let Some((id, output)) = mgr.take_completed() {
                        next_goal = format!("Background job '{}' completed:\n{}", id, output);
                        continue;
                    }
                }
            }

            // Nothing to do → loop ends
            break;
        }
        Ok(outcomes)
    }
}

// ──────────────────────────────────────────────────────────────────────────
// AgentRuntimeBuilder
// ──────────────────────────────────────────────────────────────────────────

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
    permission_hook: Option<PermissionHook>,
    planning_mode: PlanningMode,
    saved_event_sink: Option<Arc<dyn EventSink>>,
    compactor: Option<Compactor>,
}

impl std::fmt::Debug for AgentRuntimeBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRuntimeBuilder")
            .field("kernel_builder", &self.kernel_builder)
            .field("system_prompt", &self.system_prompt)
            .field("seed", &self.seed)
            .field("streaming", &self.streaming)
            .field(
                "permission_hook",
                &self.permission_hook.as_ref().map(|_| "<hook>"),
            )
            .field("planning_mode", &self.planning_mode)
            .field(
                "event_sink",
                &self.saved_event_sink.as_ref().map(|_| "<EventSink>"),
            )
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
            permission_hook: None,
            planning_mode: PlanningMode::Immediate,
            saved_event_sink: None,
            compactor: None,
        }
    }

    /// Set the LLM provider (required).
    pub fn llm(mut self, llm: Arc<dyn LlmProvider>) -> Self {
        self.kernel_builder = self.kernel_builder.llm(llm);
        self
    }

    /// Set the tool registry (optional, defaults to a local empty registry).
    pub fn tools(mut self, tools: ToolRegistry) -> Self {
        self.kernel_builder = self.kernel_builder.tools(tools);
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
        self.compactor = Some(compactor);
        self
    }

    /// Enable or disable streaming of partial tokens (optional, default false).
    pub fn streaming(mut self, enabled: bool) -> Self {
        self.streaming = enabled;
        self
    }

    /// Set the planning mode (optional, defaults to [`PlanningMode::Immediate`]).
    pub fn planning_mode(mut self, mode: PlanningMode) -> Self {
        self.planning_mode = mode;
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

    /// Set an optional permission hook for tool-call interception.
    pub fn permission_hook(mut self, hook: PermissionHook) -> Self {
        self.permission_hook = Some(hook);
        self
    }

    /// Set the event sink for streaming events (optional, defaults to [`NullSink`]).
    pub fn event_sink(mut self, sink: Arc<dyn EventSink>) -> Self {
        self.saved_event_sink = Some(sink);
        self
    }

    /// Build the [`AgentRuntime`].
    ///
    /// Returns an error if the LLM provider is missing.
    pub fn build(self) -> Result<AgentRuntime> {
        let kernel = self.kernel_builder.build()?;

        let mut transcript = Vec::new();
        if let Some(sys) = self.system_prompt {
            transcript.push(Message::system(sys));
        }
        transcript.extend(self.seed);

        Ok(AgentRuntime {
            kernel,
            transcript,
            event_sink: self.saved_event_sink.unwrap_or_else(|| Arc::new(NullSink)),
            pending_plan_calls: None,
            plan_confirmed: false,
            streaming: self.streaming,
            permission_hook: self.permission_hook,
            planning_mode: self.planning_mode,
            compactor: self.compactor,
        })
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Completion, MockProvider};
    use crate::tools::Tool;
    use async_trait::async_trait;
    use serde_json::{json, Value};

    struct Adder;

    #[async_trait]
    impl Tool for Adder {
        fn spec(&self) -> crate::llm::ToolSpec {
            crate::llm::ToolSpec {
                name: "add".into(),
                description: "add two numbers".into(),
                parameters: json!({"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}}}),
            }
        }
        async fn execute(&self, args: Value) -> crate::error::Result<String> {
            let a = args["a"].as_i64().unwrap_or(0);
            let b = args["b"].as_i64().unwrap_or(0);
            Ok((a + b).to_string())
        }
    }

    // ── basic turn execution ──────────────────────────────────────────

    #[tokio::test]
    async fn single_turn_no_tools() {
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "Hello!".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();
        let out = rt.run("hi").await.unwrap();
        assert_eq!(out.final_text.as_deref(), Some("Hello!"));
        assert_eq!(out.steps, 1);
        assert_eq!(rt.transcript().len(), 2); // user + assistant
    }

    #[tokio::test]
    async fn turn_with_tool() {
        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "Let me check...".into(),
                tool_calls: vec![crate::llm::ToolCall {
                    id: "c1".into(),
                    name: "add".into(),
                    arguments: json!({"a": 3, "b": 4}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "7".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));
        let tools = ToolRegistry::local().register(Arc::new(Adder));
        let mut rt = AgentRuntime::builder()
            .llm(llm)
            .tools(tools)
            .build()
            .unwrap();
        let out = rt.run("3+4?").await.unwrap();
        assert_eq!(out.final_text.as_deref(), Some("7"));
        assert_eq!(out.steps, 2);
        assert_eq!(rt.transcript().len(), 4); // user, assistant, tool, assistant
    }

    // ── transcript accumulation across turns ──────────────────────────

    #[tokio::test]
    async fn multi_turn_transcript_grows() {
        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "First reply".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "Second reply".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));
        let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();

        let o1 = rt.run("turn 1").await.unwrap();
        assert_eq!(o1.final_text.as_deref(), Some("First reply"));
        assert_eq!(rt.transcript().len(), 2);

        let o2 = rt.run("turn 2").await.unwrap();
        assert_eq!(o2.final_text.as_deref(), Some("Second reply"));
        assert_eq!(rt.transcript().len(), 4);
    }

    // ── builder options ───────────────────────────────────────────────

    #[tokio::test]
    async fn system_prompt_is_prepended() {
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "ok".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let mut rt = AgentRuntime::builder()
            .llm(llm)
            .system_prompt("Be helpful.")
            .build()
            .unwrap();
        rt.run("hello").await.unwrap();
        assert_eq!(rt.transcript()[0].role, crate::message::Role::System);
        assert_eq!(rt.transcript()[0].content, "Be helpful.");
    }

    #[tokio::test]
    async fn seed_transcript_is_included() {
        let seed = vec![
            Message::user("old Q".to_string()),
            Message::assistant("old A".to_string()),
        ];
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "fresh".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let mut rt = AgentRuntime::builder()
            .llm(llm)
            .seed_transcript(seed)
            .build()
            .unwrap();
        rt.run("new Q").await.unwrap();
        // seed(2) + new user + new assistant = 4
        assert_eq!(rt.transcript().len(), 4);
        assert_eq!(rt.transcript()[0].content, "old Q");
        assert_eq!(rt.transcript()[1].content, "old A");
        assert_eq!(rt.transcript()[2].content, "new Q");
        assert_eq!(rt.transcript()[3].content, "fresh");
    }

    #[tokio::test]
    async fn system_and_seed_ordering() {
        let seed = vec![Message::user("seeded user".to_string())];
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "r".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let mut rt = AgentRuntime::builder()
            .llm(llm)
            .system_prompt("sys prompt")
            .seed_transcript(seed)
            .build()
            .unwrap();
        rt.run("real").await.unwrap();
        assert_eq!(rt.transcript()[0].role, crate::message::Role::System);
        assert_eq!(rt.transcript()[0].content, "sys prompt");
        assert_eq!(rt.transcript()[1].content, "seeded user");
        assert_eq!(rt.transcript()[2].content, "real");
    }

    // ── state inspection / mutation ───────────────────────────────────

    #[tokio::test]
    async fn set_transcript_replaces() {
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "ok".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();
        rt.set_transcript(vec![Message::user("custom".to_string())]);
        assert_eq!(rt.transcript().len(), 1);
        assert_eq!(rt.transcript()[0].content, "custom");
    }

    #[tokio::test]
    async fn kernel_accessor_works() {
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "ok".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let rt = AgentRuntime::builder().llm(llm).build().unwrap();
        let _kernel = rt.kernel(); // should compile and return a reference
    }

    // ── default values ────────────────────────────────────────────────

    #[tokio::test]
    async fn defaults_are_sensible() {
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "done".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();
        let out = rt.run("test").await.unwrap();
        assert_eq!(out.finish_reason, FinishReason::NoMoreToolCalls);
        assert_eq!(rt.transcript().len(), 2);
    }
}
