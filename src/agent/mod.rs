//! Agent loop. The whole kernel.
// Internal module — suppress deprecated warnings for types defined here.
#![allow(deprecated)]
//!
//! Tiny on purpose: receive a goal, ask the model what to do, run any tool
//! calls, feed results back, repeat until the model stops requesting tools
//! or we hit the step budget. Everything interesting (which model, which
//! tools, what system prompt) is injected by the caller.
//!
//! The loop emits `StepEvent`s through a channel so a UI/CLI/log layer can
//! observe progress without coupling to the agent's internals.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::compact::Compactor;
use crate::error::{Error, Result};
use crate::hooks::{Hook, HookAction, HookEvent, HookRegistry};
use crate::llm::{LlmProvider, TokenUsage, ToolCall};
use crate::message::Message;
use crate::permissions::PermissionMode;
use crate::tools::ToolRegistry;

pub mod types;
pub use types::*;

/// Optional callback fired whenever a message is appended to the transcript.
///
/// # Deprecated
/// Use [`AgentRuntime`](crate::runtime::AgentRuntime) with an
/// [`EventSink`](crate::event::EventSink) instead.
#[deprecated(
    since = "0.5.0",
    note = "Use AgentRuntime with an EventSink instead of OnMessageFn callbacks."
)]
pub type OnMessageFn = Box<dyn Fn(&Message) + Send + Sync>;

use crate::run_core::TRIM_PLACEHOLDER;

/// Low-level agent step events.
///
/// # Deprecated
/// Use [`AgentEvent`](crate::event::AgentEvent) instead.  `StepEvent` is an
/// internal implementation detail bridged to `AgentEvent` by the kernel layer;
/// external code should subscribe to events via the
/// [`EventSink`](crate::event::EventSink) API and receive `AgentEvent` values.
#[deprecated(
    since = "0.5.0",
    note = "Use AgentEvent (crate::event::AgentEvent) instead."
)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum StepEvent {
    /// Model generated text without tool calls.
    ///
    /// Emitted when the LLM produces a response. This is typically the final
    /// answer or intermediate reasoning. The `text` field contains the complete
    /// model response, and `step` indicates which iteration this occurred on.
    AssistantText { text: String, step: usize },
    /// Model requested to execute a tool.
    ///
    /// Emitted when the LLM calls a tool. Contains the tool name, ID, and arguments
    /// that will be executed. The `call` is dispatched to the registry after this event.
    /// Tool errors are reported via `ToolResult` events, not `ToolCall` failures.
    ToolCall { call: ToolCall, step: usize },
    /// Time taken for the LLM request (excluding tool execution).
    ///
    /// Emitted after the model responds. Useful for measuring provider latency
    /// and diagnosing slow responses. `llm_ms` is in milliseconds.
    Latency { step: usize, llm_ms: u64 },
    /// Result of executing a tool call.
    ///
    /// Emitted after a tool finishes executing. Contains the tool name, call ID,
    /// the output string (or error message), and the step number. This result
    /// is added to the transcript and sent back to the model for the next iteration.
    /// In case of tool error, the output will be the error message prefixed with "ERROR: ".
    ToolResult {
        id: String,
        name: String,
        output: String,
        step: usize,
    },
    /// Token usage statistics from the LLM provider.
    ///
    /// Emitted if the provider returns usage information (input tokens, output tokens).
    /// Accumulated across all steps for total usage. Useful for cost tracking and
    /// monitoring resource consumption.
    Usage { usage: TokenUsage, step: usize },
    /// Partial token from streaming response (if streaming enabled).
    ///
    /// Only emitted if streaming is enabled. Contains a single token or partial chunk
    /// of the model's response text. Allows UI layers to display real-time incremental
    /// updates to the model's output without waiting for the entire response.
    PartialToken { text: String, step: usize },
    /// Transcript was compacted to fit size constraints.
    ///
    /// Emitted when the transcript exceeds the max size and is automatically compacted.
    /// The `removed` field shows how many messages were summarized, `kept` shows how many
    /// remain, and `summary_chars` shows the size of the compaction summary added.
    /// This allows UI layers to notify users that older context has been summarized.
    /// Compaction only occurs if a `Compactor` is configured on the Agent.
    Compacted {
        removed: usize,
        kept: usize,
        summary_chars: usize,
        step: usize,
    },
    /// Agent run completed.
    ///
    /// Emitted as the final event. Indicates the run is done and why it stopped.
    /// `reason` explains the termination (no more tool calls, budget exceeded, stuck
    /// detection, etc.). `steps` shows how many iterations were executed.
    /// After this event, no more events will be emitted for this run.
    Finished { reason: FinishReason, steps: usize },
    /// Agent has produced a plan and is waiting for confirmation.
    PlanProposed {
        /// Human-readable plan description
        plan_text: String,
        /// The buffered tool calls
        tool_calls: Vec<ToolCall>,
    },
    /// Plan was confirmed, execution will proceed.
    PlanConfirmed,
    /// Plan was rejected with a reason.
    PlanRejected { reason: String },

    // ── Goal 210: hook progress events ────────────────────────────
    /// A hook started executing.
    ///
    /// Emitted just before a hook process/request is dispatched. Allows
    /// UI layers (TUI) to show a spinner while the hook runs.
    HookStarted {
        /// Serialized event name (e.g. `"preToolCall"`).
        hook_event: String,
        /// Human-readable hook identifier (path, URL, or `"prompt"`).
        hook_name: String,
        /// Optional custom status message from `HookCommand::status_message`.
        status_message: Option<String>,
    },
    /// A hook produced incremental stdout output.
    ///
    /// Emitted periodically while a hook is running (if stdout streaming
    /// is available). Contains the most recent stdout line.
    HookProgress {
        hook_event: String,
        hook_name: String,
        /// The most recently observed stdout line.
        last_line: String,
    },
    /// A hook finished executing.
    ///
    /// Emitted immediately after a hook returns. `outcome` describes the
    /// final status: `"success"`, `"error"`, `"timeout"`, or `"skipped"`.
    HookFinished {
        hook_event: String,
        hook_name: String,
        /// Final status: `"success"` / `"error"` / `"timeout"` / `"skipped"`.
        outcome: String,
        /// Wall-clock duration of the hook in milliseconds.
        duration_ms: u64,
    },
    /// A hook produced a system message to show to the user.
    HookSystemMessage {
        /// The message text to display.
        text: String,
    },
}

// `FinishReason` lives in `crate::agent::types` and is re-exported via
// `pub use types::*;` at the top of this module. The doc comment block
// that previously accompanied the definition has moved with it.

/// The outcome of a single [`Agent::run()`] call.
///
/// # Deprecated
/// Use [`AgentRuntime`](crate::runtime::AgentRuntime) and its
/// [`RuntimeOutcome`](crate::runtime::RuntimeOutcome) instead.  `Agent` is a
/// legacy wrapper; new code should call `AgentRuntime::run()`.
#[deprecated(
    since = "0.5.0",
    note = "Use AgentRuntime::run() which returns RuntimeOutcome."
)]
#[derive(Debug, Clone)]
pub struct AgentOutcome {
    pub final_message: Option<String>,
    pub transcript: Vec<Message>,
    pub steps: usize,
    pub finish: FinishReason,
    pub total_usage: TokenUsage,
    pub total_llm_latency_ms: u64,
}

/// The ReAct loop: ask LLM, execute tools, repeat.
///
/// # Deprecated
/// Use [`AgentRuntime`](crate::runtime::AgentRuntime) instead.  `Agent` is a
/// legacy type kept for backward compatibility.  New code should build and run
/// an `AgentRuntime`:
///
/// ```ignore
/// let mut runtime = AgentRuntime::builder()
///     .llm(provider)
///     .system_prompt("You are a helpful assistant")
///     .max_steps(32)
///     .build()?;
/// let outcome = runtime.run("Help me debug this file").await?;
/// ```
#[deprecated(since = "0.5.0", note = "Use AgentRuntime instead of Agent.")]
pub struct Agent {
    llm: Arc<dyn LlmProvider>,
    tools: ToolRegistry,
    transcript: Vec<Message>,
    max_steps: usize,
    max_transcript_chars: Option<usize>,
    events: Option<mpsc::UnboundedSender<StepEvent>>,
    streaming: bool,
    total_llm_latency_ms: u64,
    compactor: Option<Compactor>,
    permission_hook: Option<PermissionHook>,
    hooks: HookRegistry,
    planning_mode: PlanningMode,
    plan_buffer: Option<Vec<ToolCall>>,
    plan_confirmed: bool,
    on_message: Option<OnMessageFn>,
    shutdown_token: Option<CancellationToken>,
}

#[allow(unused_imports)]
pub(crate) use crate::run_core::{RunCore, RunInnerOutcome};

impl Agent {
    pub fn builder() -> AgentBuilder {
        AgentBuilder::default()
    }

    /// Restore transcript for multi-turn reuse. Call after `run()` returns
    /// to re-seed the agent with the conversation history.
    pub fn set_transcript(&mut self, transcript: Vec<Message>) {
        self.transcript = transcript;
    }

    /// Replace the events channel. Dropping the old sender closes its
    /// channel (letting any spawned printer task finish). Pass `None` to
    /// disable event emission entirely.
    pub fn set_events(&mut self, tx: Option<mpsc::UnboundedSender<StepEvent>>) {
        self.events = tx;
    }

    /// Confirm a proposed plan, allowing execution to proceed.
    pub fn confirm_plan(&mut self) {
        self.plan_confirmed = true;
        self.emit(StepEvent::PlanConfirmed);
    }

    /// Reject a proposed plan with a reason.
    pub fn reject_plan(&mut self, reason: &str) {
        // Feed the rejection back to the model as tool errors so it can revise.
        if let Some(calls) = self.plan_buffer.take() {
            for call in &calls {
                let result = format!(
                    "ERROR: Plan rejected. Reason: {reason}. \
                     The tool call {}({}) was not executed. \
                     Please revise your approach.",
                    call.name,
                    serde_json::to_string(&call.arguments).unwrap_or_default()
                );
                self.push_message(Message::tool_result(call.id.clone(), result));
            }
        }
        self.emit(StepEvent::PlanRejected {
            reason: reason.into(),
        });
    }

    /// Set a cancellation token that will be checked between steps.
    /// When the token is cancelled, the agent finishes with `FinishReason::Cancelled`.
    pub fn set_shutdown_token(&mut self, token: CancellationToken) {
        self.shutdown_token = Some(token);
    }

    /// Drive the loop until the model stops calling tools, or the budget is exhausted.
    /// Execute a set of tool calls, returning (id, name, output, args) for each.
    /// Read-only calls are batched and executed in parallel; write calls run
    /// sequentially to preserve ordering guarantees.
    #[allow(dead_code)]
    async fn execute_tool_calls(
        &mut self,
        calls: &[ToolCall],
        _step: usize,
    ) -> Vec<(String, String, String, serde_json::Value)> {
        let mut results: Vec<(String, String, String, serde_json::Value)> = Vec::new();

        // First pass: handle denied/skipped calls immediately, collect the rest.
        struct PendingCall {
            id: String,
            name: String,
            args: serde_json::Value,
        }
        let mut pending: Vec<PendingCall> = Vec::new();

        for call in calls {
            let effective_args = if let Some(ref hook) = self.permission_hook {
                match hook(&call.name, &call.arguments) {
                    PermissionDecision::Allow => call.arguments.clone(),
                    PermissionDecision::Deny(reason) => {
                        let result = format!("ERROR: {reason}");
                        results.push((
                            call.id.clone(),
                            call.name.clone(),
                            result,
                            call.arguments.clone(),
                        ));
                        continue;
                    }
                    PermissionDecision::Transform(new_args) => new_args,
                }
            } else {
                call.arguments.clone()
            };

            // Apply lifecycle hooks before executing the tool
            let hook_action = self.hooks.dispatch(HookEvent::PreToolCall {
                name: &call.name,
                args: &effective_args,
            });
            match hook_action {
                HookAction::Skip => {
                    let result = "ERROR: tool call skipped by hook".to_string();
                    results.push((
                        call.id.clone(),
                        call.name.clone(),
                        result,
                        call.arguments.clone(),
                    ));
                    continue;
                }
                HookAction::Error(msg) => {
                    let result = format!("ERROR: {msg}");
                    results.push((
                        call.id.clone(),
                        call.name.clone(),
                        result,
                        call.arguments.clone(),
                    ));
                    continue;
                }
                HookAction::Continue => {}
            }

            pending.push(PendingCall {
                id: call.id.clone(),
                name: call.name.clone(),
                args: effective_args,
            });
        }

        // Second pass: execute read-only calls in parallel, write calls sequentially.
        let mut i = 0;
        while i < pending.len() {
            if self
                .tools
                .is_readonly_for_call(&pending[i].name, &pending[i].args)
            {
                // Batch consecutive read-only calls (including explore sub-agents)
                let batch_start = i;
                while i < pending.len()
                    && self
                        .tools
                        .is_readonly_for_call(&pending[i].name, &pending[i].args)
                {
                    i += 1;
                }
                let batch: Vec<PendingCall> = pending.drain(batch_start..i).collect();
                i = batch_start;

                let mut join_set = tokio::task::JoinSet::new();
                for pc in &batch {
                    let name = pc.name.clone();
                    let args = pc.args.clone();
                    let tools = self.tools.clone();
                    join_set.spawn(async move {
                        let tool_start = std::time::Instant::now();
                        let span = tracing::info_span!("tool.exec", tool = %name);
                        let result = span.in_scope(|| tools.invoke(&name, args)).await;
                        let result = match result {
                            Ok(output) => output,
                            Err(err) => format!("ERROR: {err}"),
                        };
                        let duration_ms = tool_start.elapsed().as_millis() as u64;
                        (name, result, duration_ms)
                    });
                }

                let mut batch_results: Vec<(String, String, u64)> = Vec::new();
                while let Some(res) = join_set.join_next().await {
                    let (name, result, duration_ms) = res.unwrap();
                    batch_results.push((name, result, duration_ms));
                }

                for pc in &batch {
                    let (_, result, duration_ms) = batch_results
                        .iter()
                        .find(|(n, _, _)| n == &pc.name)
                        .unwrap();
                    results.push((
                        pc.id.clone(),
                        pc.name.clone(),
                        result.clone(),
                        pc.args.clone(),
                    ));
                    self.hooks.dispatch(HookEvent::PostToolCall {
                        name: &pc.name,
                        args: &pc.args,
                        result,
                        duration_ms: *duration_ms,
                    });
                }
            } else {
                let pc = pending.remove(i);
                let tool_start = std::time::Instant::now();
                let span = tracing::info_span!("tool.exec", tool = %pc.name);
                let result = span
                    .in_scope(|| self.tools.invoke(&pc.name, pc.args.clone()))
                    .await;
                let result = match result {
                    Ok(output) => output,
                    Err(err) => format!("ERROR: {err}"),
                };
                let duration_ms = tool_start.elapsed().as_millis() as u64;
                results.push((
                    pc.id.clone(),
                    pc.name.clone(),
                    result.clone(),
                    pc.args.clone(),
                ));
                self.hooks.dispatch(HookEvent::PostToolCall {
                    name: &pc.name,
                    args: &pc.args,
                    result: &result,
                    duration_ms,
                });
            }
        }

        results
    }

    #[tracing::instrument(skip(self), fields(goal))]
    pub async fn run(&mut self, goal: impl Into<String>) -> Result<AgentOutcome> {
        let goal = goal.into();
        info!(target: "recursive::agent", goal = %truncate(&goal, 200), "agent run starting");
        self.push_message(Message::user(goal.clone()));
        self.hooks.dispatch(HookEvent::SessionStart { goal: &goal });

        // Legacy bridge: `RunCore` now emits `AgentEvent` directly.  If the
        // caller wired up a legacy `StepEvent` channel on this `Agent`, spawn
        // a converter task that forwards `AgentEvent` → `StepEvent`.  This
        // bridge is transient — it disappears with the `Agent` legacy
        // wrapper in Commit 2 of Goal 219.
        let (core_events_tx, bridge_handle) = match self.events.clone() {
            Some(step_tx) => {
                let (ae_tx, mut ae_rx) = mpsc::unbounded_channel::<crate::event::AgentEvent>();
                let handle = tokio::spawn(async move {
                    while let Some(ae) = ae_rx.recv().await {
                        let _ = step_tx.send(ae.into());
                    }
                });
                (Some(ae_tx), Some(handle))
            }
            None => (None, None),
        };

        let core = RunCore {
            messages: std::mem::take(&mut self.transcript),
            llm: self.llm.clone(),
            tools: self.tools.clone(),
            max_steps: self.max_steps,
            max_transcript_chars: self.max_transcript_chars,
            events: core_events_tx,
            streaming: self.streaming,
            compactor: self.compactor.clone(),
            permission_hook: self.permission_hook.clone(),
            hooks: &self.hooks,
            planning_mode: self.planning_mode.clone(),
            on_message: &self.on_message,
            total_llm_latency_ms: self.total_llm_latency_ms,
            plan_buffer: self.plan_buffer.take(),
            plan_confirmed: self.plan_confirmed,
            // Legacy Agent path: plan mode 2.0 not wired up, default to off.
            exploring_plan_mode: Arc::new(AtomicBool::new(false)),
            permission_mode: PermissionMode::Default,
            shutdown_token: self.shutdown_token.clone(),
            // Legacy Agent path: mailbox not wired up, default to None.
            mailbox: None,
        };

        let inner = core.run_inner().await?;

        // Wait for the bridge to flush any remaining events.
        if let Some(handle) = bridge_handle {
            handle.await.ok();
        }

        // Restore mutable state from the stateless run.
        self.transcript = inner.messages;
        self.total_llm_latency_ms = inner.total_llm_latency_ms;
        self.plan_buffer = inner.plan_buffer;
        self.plan_confirmed = inner.plan_confirmed;

        let outcome = AgentOutcome {
            final_message: inner.final_message,
            transcript: self.transcript.clone(),
            steps: inner.steps,
            finish: inner.finish_reason,
            total_usage: inner.total_usage,
            total_llm_latency_ms: inner.total_llm_latency_ms,
        };

        // Dispatch SessionEnd for terminal outcomes.
        match &outcome.finish {
            FinishReason::NoMoreToolCalls
            | FinishReason::Stuck { .. }
            | FinishReason::BudgetExceeded => {
                self.hooks
                    .dispatch(HookEvent::SessionEnd { outcome: &outcome });
            }
            _ => {}
        }

        tracing::info!(
            target: "recursive::agent",
            steps = outcome.steps,
            tokens_in = outcome.total_usage.prompt_tokens,
            tokens_out = outcome.total_usage.completion_tokens,
            finish = ?outcome.finish,
            llm_latency_ms = outcome.total_llm_latency_ms,
            "agent.run.complete"
        );

        Ok(outcome)
    }

    /// Try to trim old tool results to bring the transcript under the character limit.
    ///
    /// Walks the transcript from index 1 (skipping the system prompt at 0) forward,
    /// and for any `Role::Tool` message whose content is longer than 200 characters,
    /// replaces the content with [`TRIM_PLACEHOLDER`]. Stops as soon as the total
    /// character count is below `limit`. Emits an `AssistantText` event (reusing the
    /// existing variant) to surface that trimming happened.
    #[allow(dead_code)]
    async fn maybe_compact(&mut self, step: usize) -> Result<()> {
        let compactor = match &self.compactor {
            Some(c) => c,
            None => return Ok(()),
        };

        let chars = Compactor::estimate_chars(&self.transcript);
        if chars < compactor.threshold_chars {
            return Ok(());
        }

        // Need at least keep_recent_n + 2 messages to have something to compact.
        let min_messages = compactor.keep_recent_n + 2;
        if self.transcript.len() < min_messages {
            return Ok(());
        }

        let summary_msg = compactor
            .compact(self.llm.as_ref(), &self.transcript)
            .await?;
        let summary_chars = summary_msg.content.len();

        // Replace the older portion with the summary message.
        // Keep the last keep_recent_n messages verbatim.
        let keep = compactor.keep_recent_n;
        let mut split = self.transcript.len().saturating_sub(keep);

        // Invariant: every `Role::Tool` message must be immediately preceded by
        // an `Role::Assistant` message containing the matching `tool_calls`.
        // OpenAI/DeepSeek/Anthropic all enforce this on the request side. If
        // the kept window starts at a `Role::Tool` message, the parent
        // assistant has just been drained — the next LLM request fails with
        // HTTP 400 ("Messages with role 'tool' must be a response to a
        // preceding message with 'tool_calls'"). Retreat the split until the
        // window starts at a non-Tool message.
        while split > 0 && matches!(self.transcript[split].role, crate::message::Role::Tool) {
            split -= 1;
        }

        let removed = split;
        let kept = self.transcript.len() - split;

        // Drain the older messages and insert the summary at the front.
        self.transcript.drain(..split);
        self.transcript.insert(0, summary_msg);

        self.hooks.dispatch(HookEvent::PostCompact {
            removed,
            summary_chars,
        });

        self.emit(StepEvent::Compacted {
            removed,
            kept,
            summary_chars,
            step,
        });

        Ok(())
    }

    #[allow(dead_code)]
    fn maybe_trim_transcript(&mut self, limit: usize, step: usize) {
        let mut chars: usize = self.transcript.iter().map(|m| m.content.len()).sum();
        if chars < limit {
            return;
        }

        let mut trimmed_count: usize = 0;
        let placeholder_len = TRIM_PLACEHOLDER.len();

        // Walk from index 1 (skip system prompt at 0) forward, trimming old tool results.
        // Track the running total ourselves to avoid re-borrowing self.transcript.
        for msg in self.transcript.iter_mut().skip(1) {
            if msg.role == crate::message::Role::Tool && msg.content.len() > 200 {
                let old_len = msg.content.len();
                msg.content = TRIM_PLACEHOLDER.to_string();
                trimmed_count += 1;
                // Adjust the running total: we removed old_len and added placeholder_len.
                chars = chars
                    .saturating_sub(old_len)
                    .saturating_add(placeholder_len);
                if chars < limit {
                    break;
                }
            }
        }

        if trimmed_count > 0 {
            let note = format!(
                "[trimmed {} old tool result{} to fit budget]",
                trimmed_count,
                if trimmed_count == 1 { "" } else { "s" }
            );
            self.emit(StepEvent::AssistantText { text: note, step });
        }
    }

    pub fn transcript(&self) -> &[Message] {
        &self.transcript
    }

    fn emit(&self, event: StepEvent) {
        if let Some(tx) = &self.events {
            let _ = tx.send(event);
        }
    }

    /// Push a message to the transcript and fire the `on_message` callback if set.
    fn push_message(&mut self, msg: Message) {
        if let Some(ref cb) = self.on_message {
            cb(&msg);
        }
        self.transcript.push(msg);
    }
}

/// Builder for configuring and creating an agent.
///
/// Use `Agent::builder()` to start building. All methods are optional except `llm()`.
///
/// # Example
///
/// ```ignore
/// let agent = Agent::builder()
///     .llm(Arc::new(provider))
///     .tools(registry)
///     .system_prompt("You are a helpful assistant")
///     .max_steps(50)
///     .build()?;
/// ```
#[derive(Default)]
pub struct AgentBuilder {
    llm: Option<Arc<dyn LlmProvider>>,
    tools: ToolRegistry,
    system: Option<String>,
    max_steps: Option<usize>,
    max_transcript_chars: Option<usize>,
    events: Option<mpsc::UnboundedSender<StepEvent>>,
    seed: Vec<Message>,
    streaming: bool,
    compactor: Option<Compactor>,
    permission_hook: Option<PermissionHook>,
    hooks: HookRegistry,
    planning_mode: PlanningMode,
    on_message: Option<OnMessageFn>,
    shutdown_token: Option<CancellationToken>,
}

impl AgentBuilder {
    /// Set the LLM provider (required).
    ///
    /// The provider handles all requests to the model. Must be provided before
    /// calling `build()`, otherwise `build()` will return an error.
    pub fn llm(mut self, llm: Arc<dyn LlmProvider>) -> Self {
        self.llm = Some(llm);
        self
    }
    /// Set the tool registry (optional, defaults to empty registry).
    ///
    /// Tools are available to the model for execution during the run. If not set,
    /// the agent will only generate text. The registry is shared via Arc, so tool
    /// implementations must be thread-safe.
    pub fn tools(mut self, tools: ToolRegistry) -> Self {
        self.tools = tools;
        self
    }
    /// Set the system prompt (optional).
    ///
    /// Prepended to the transcript before the first goal. Typically describes
    /// the agent's role (e.g., "You are a code assistant"). If not set, no
    /// system message is added.
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system = Some(prompt.into());
        self
    }
    /// Set the maximum number of agent iterations (optional, defaults to 32).
    ///
    /// Each iteration is one LLM call + tool execution round. Higher values
    /// allow more complex reasoning but increase cost and latency. If exceeded,
    /// the run finishes with `FinishReason::BudgetExceeded`.
    pub fn max_steps(mut self, n: usize) -> Self {
        self.max_steps = Some(n);
        self
    }
    /// Set the maximum transcript character size (optional).
    ///
    /// Prevents the transcript from growing unbounded. When this limit is approached,
    /// `Compactor` (if configured) will summarize and trim old messages. If the
    /// transcript cannot fit within the limit, the run stops with
    /// `FinishReason::TranscriptLimit`.
    ///
    /// # Note
    ///
    /// This is a hard limit; if compaction cannot reduce the size enough, the
    /// run will terminate rather than add the next message.
    pub fn max_transcript_chars(mut self, n: usize) -> Self {
        self.max_transcript_chars = Some(n);
        self
    }
    /// Attach an event channel to observe agent progress (optional).
    ///
    /// Events are sent through this channel as the agent runs. Non-blocking:
    /// if the receiver is dropped or the channel is full, sends are silently
    /// ignored. Allows UI layers to display real-time progress without coupling
    /// to the agent's internals.
    pub fn events(mut self, tx: mpsc::UnboundedSender<StepEvent>) -> Self {
        self.events = Some(tx);
        self
    }
    /// Seed the agent with a pre-existing transcript. Seeded messages are
    /// placed in the transcript *after* the system prompt but *before* the
    /// new goal. No `StepEvent`s are emitted for seeded messages; only events
    /// produced during the new run are streamed.
    pub fn seed_transcript(mut self, messages: Vec<Message>) -> Self {
        self.seed = messages;
        self
    }
    /// Enable token-by-token streaming from the provider (optional, defaults to false).
    ///
    /// If enabled, the agent creates a channel and asks the provider to emit
    /// `PartialToken` events as they arrive. Requires provider support; some
    /// providers ignore this and emit the complete response at once.
    pub fn streaming(mut self, enabled: bool) -> Self {
        self.streaming = enabled;
        self
    }
    /// Attach a compactor to automatically trim old transcript content (optional).
    ///
    /// When the transcript reaches `max_transcript_chars`, the compactor
    /// summarizes and removes old messages. Only active if both `max_transcript_chars`
    /// and `compactor` are set. Without a compactor, `TranscriptLimit` errors occur.
    pub fn compactor(mut self, compactor: Compactor) -> Self {
        self.compactor = Some(compactor);
        self
    }
    /// Attach a permission hook that is invoked before each tool execution (optional).
    ///
    /// The hook can allow, deny, or transform tool arguments. If denied, the tool
    /// is not executed and the reason is returned as a tool error. If transformed,
    /// the new arguments are passed to the tool instead.
    pub fn permission_hook<F>(mut self, hook: F) -> Self
    where
        F: Fn(&str, &serde_json::Value) -> PermissionDecision + Send + Sync + 'static,
    {
        self.permission_hook = Some(Arc::new(hook));
        self
    }

    /// Attach a pre-built permission hook (e.g. cloned from a parent agent).
    ///
    /// This is useful when inheriting a permission hook from a parent agent
    /// into a sub-agent. Unlike `permission_hook()`, this accepts an
    /// `Option<PermissionHook>` directly, avoiding the need to re-wrap.
    pub fn permission_hook_opt(mut self, hook: Option<PermissionHook>) -> Self {
        self.permission_hook = hook;
        self
    }
    /// Register a lifecycle hook (optional).
    ///
    /// Hooks are invoked at well-defined lifecycle points during the agent run.
    /// Multiple hooks are supported; they fire in registration order.
    pub fn hook(mut self, hook: Arc<dyn Hook>) -> Self {
        self.hooks.register(hook);
        self
    }
    /// Set the planning mode (optional, defaults to Immediate).
    ///
    /// When set to `PlanFirst`, the agent will buffer tool calls and emit a
    /// `PlanProposed` event instead of executing them immediately. The caller
    /// must then call `confirm_plan()` or `reject_plan()` to proceed.
    pub fn planning_mode(mut self, mode: PlanningMode) -> Self {
        self.planning_mode = mode;
        self
    }
    /// Attach a callback that fires whenever a message is appended to the transcript.
    ///
    /// The callback receives a reference to the newly appended `Message`.
    /// It is called for every message pushed during `run()` (user goal,
    /// assistant responses, tool results). It is **not** called for messages
    /// added via `seed_transcript()`, `set_transcript()`, or compaction
    /// (which replaces messages rather than appending).
    ///
    /// The callback must be `Send + Sync`. Panicking inside the callback
    /// will propagate and abort the agent run — the caller is responsible
    /// for catching panics if needed.
    pub fn on_message(mut self, f: OnMessageFn) -> Self {
        self.on_message = Some(f);
        self
    }
    /// Attach a cancellation token for graceful shutdown on SIGINT/SIGTERM.
    pub fn shutdown_token(mut self, token: CancellationToken) -> Self {
        self.shutdown_token = Some(token);
        self
    }
    pub fn build(self) -> Result<Agent> {
        let llm = self.llm.ok_or_else(|| Error::Config {
            message: "agent: missing llm provider".into(),
        })?;
        let mut transcript = Vec::new();
        if let Some(sys) = self.system {
            transcript.push(Message::system(sys));
        }
        transcript.extend(self.seed);
        Ok(Agent {
            llm,
            tools: self.tools,
            transcript,
            max_steps: self.max_steps.unwrap_or(32),
            max_transcript_chars: self.max_transcript_chars,
            events: self.events,
            streaming: self.streaming,
            total_llm_latency_ms: 0,
            compactor: self.compactor,
            permission_hook: self.permission_hook,
            hooks: self.hooks,
            planning_mode: self.planning_mode,
            plan_buffer: None,
            plan_confirmed: false,
            on_message: self.on_message,
            shutdown_token: self.shutdown_token,
        })
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n).collect();
        out.push_str("...");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Completion, MockProvider, TokenUsage, ToolCall};
    use crate::tools::Tool;
    use async_trait::async_trait;
    use serde_json::{json, Value};

    struct Adder;

    #[async_trait]
    impl Tool for Adder {
        fn spec(&self) -> crate::llm::ToolSpec {
            crate::llm::ToolSpec {
                name: "add".into(),
                description: "add a and b".into(),
                parameters: json!({"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}}}),
            }
        }
        async fn execute(&self, args: Value) -> Result<String> {
            let a = args["a"].as_i64().unwrap_or(0);
            let b = args["b"].as_i64().unwrap_or(0);
            Ok((a + b).to_string())
        }
    }

    #[tokio::test]
    async fn terminates_when_model_emits_no_tool_calls() {
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "done".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let mut agent = Agent::builder().llm(llm).build().unwrap();
        let out = agent.run("hi").await.unwrap();
        assert_eq!(out.final_message.as_deref(), Some("done"));
        assert_eq!(out.steps, 1);
        assert_eq!(out.finish, FinishReason::NoMoreToolCalls);
    }

    #[tokio::test]
    async fn runs_a_tool_then_completes() {
        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "let me add".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "add".into(),
                    arguments: json!({"a":2,"b":3}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "5".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));
        let tools = ToolRegistry::local().register(Arc::new(Adder));
        let mut agent = Agent::builder().llm(llm).tools(tools).build().unwrap();
        let out = agent.run("what is 2+3?").await.unwrap();
        assert_eq!(out.final_message.as_deref(), Some("5"));
        assert_eq!(out.steps, 2);
        // transcript should be: user, assistant(tool_call), tool, assistant("5")
        assert_eq!(out.transcript.len(), 4);
    }

    #[tokio::test]
    async fn reports_step_budget_exceeded() {
        let mut script = Vec::new();
        for _ in 0..10 {
            script.push(Completion {
                content: "".into(),
                tool_calls: vec![ToolCall {
                    id: "x".into(),
                    name: "add".into(),
                    arguments: json!({"a":1,"b":1}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            });
        }
        let llm = Arc::new(MockProvider::new(script));
        let tools = ToolRegistry::local().register(Arc::new(Adder));
        let mut agent = Agent::builder()
            .llm(llm)
            .tools(tools)
            .max_steps(3)
            .build()
            .unwrap();
        let out = agent.run("loop").await.unwrap();
        assert!(matches!(out.finish, FinishReason::BudgetExceeded));
        assert_eq!(out.steps, 3);
        // Transcript MUST be populated even on budget-exceeded — this is
        // what unlocks auto-resume in self-improve.sh.
        assert!(!out.transcript.is_empty());
    }

    #[tokio::test]
    async fn unknown_tool_returns_error_to_model_not_abort() {
        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "nope".into(),
                    arguments: json!({}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "ok i give up".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));
        let mut agent = Agent::builder().llm(llm).build().unwrap();
        let out = agent.run("call a missing tool").await.unwrap();
        // tool message must contain the error so the model can recover
        let tool_msg = out
            .transcript
            .iter()
            .find(|m| m.role == crate::message::Role::Tool)
            .unwrap();
        assert!(tool_msg.content.contains("ERROR"));
        assert_eq!(out.final_message.as_deref(), Some("ok i give up"));
    }

    #[tokio::test]
    async fn emits_events_in_order() {
        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "thinking".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "add".into(),
                    arguments: json!({"a":1,"b":1}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "two".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));
        let tools = ToolRegistry::local().register(Arc::new(Adder));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut agent = Agent::builder()
            .llm(llm)
            .tools(tools)
            .events(tx)
            .build()
            .unwrap();
        agent.run("add").await.unwrap();
        let mut kinds = Vec::new();
        while let Ok(e) = rx.try_recv() {
            kinds.push(match e {
                StepEvent::AssistantText { .. } => "text",
                StepEvent::ToolCall { .. } => "call",
                StepEvent::ToolResult { .. } => "result",
                StepEvent::Finished { .. } => "done",
                StepEvent::Usage { .. } => "usage",
                StepEvent::Latency { .. } => "latency",
                StepEvent::PartialToken { .. } => "partial",
                StepEvent::Compacted { .. } => "compacted",
                StepEvent::PlanProposed { .. } => "plan_proposed",
                StepEvent::PlanConfirmed => "plan_confirmed",
                StepEvent::PlanRejected { .. } => "plan_rejected",
                StepEvent::HookStarted { .. } => "hook_started",
                StepEvent::HookProgress { .. } => "hook_progress",
                StepEvent::HookFinished { .. } => "hook_finished",
                StepEvent::HookSystemMessage { .. } => "hook_system_message",
            });
        }
        assert_eq!(
            kinds,
            vec!["latency", "text", "call", "result", "latency", "text", "done"]
        );
    }

    #[tokio::test]
    async fn stops_when_repeated_call_keeps_erroring() {
        // MockProvider scripted to call a non-existent tool 4 times
        let mut script = Vec::new();
        for i in 0..4 {
            script.push(Completion {
                content: "".into(),
                tool_calls: vec![ToolCall {
                    id: format!("c{}", i),
                    name: "UnknownTool".into(),
                    arguments: json!({"arg": "value"}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            });
        }
        let llm = Arc::new(MockProvider::new(script));
        let mut agent = Agent::builder().llm(llm).max_steps(10).build().unwrap();
        let out = agent.run("call unknown tool").await.unwrap();

        // Should be stuck after 3 consecutive errors
        assert!(matches!(out.finish, FinishReason::Stuck { .. }));
        if let FinishReason::Stuck {
            repeated_call,
            repeats,
        } = &out.finish
        {
            assert_eq!(repeated_call, "UnknownTool");
            assert_eq!(*repeats, 3);
        }
    }

    #[tokio::test]
    async fn truncated_response_surfaces_as_error() {
        // Provider says finish_reason = "length": the response was cut off by
        // the server, not a deliberate stop. The agent must treat this as
        // failure rather than pretend the assistant ended its turn.
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "I was going to say more but ran out of".into(),
            tool_calls: vec![],
            finish_reason: Some("length".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let mut agent = Agent::builder().llm(llm).build().unwrap();
        let err = agent.run("hi").await.unwrap_err();
        assert!(matches!(err, Error::ProviderTruncated(ref s) if s == "length"));
    }

    #[tokio::test]
    async fn does_not_trigger_when_args_differ() {
        // MockProvider scripted to call same tool with different args each time
        let mut script = Vec::new();
        for i in 0..3 {
            script.push(Completion {
                content: "".into(),
                tool_calls: vec![ToolCall {
                    id: format!("c{}", i),
                    name: "add".into(),
                    arguments: json!({"a": i, "b": i}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            });
        }
        let llm = Arc::new(MockProvider::new(script));
        let tools = ToolRegistry::local().register(Arc::new(Adder));
        // Set max_steps low so test terminates with budget
        let mut agent = Agent::builder()
            .llm(llm)
            .tools(tools)
            .max_steps(3)
            .build()
            .unwrap();
        let out = agent.run("add with different args").await.unwrap();

        // Should hit budget, not stuck (args differ each time)
        assert!(matches!(out.finish, FinishReason::BudgetExceeded));
        assert_eq!(out.steps, 3);
    }

    /// Regression test for self-improve.sh auto-resume.
    ///
    /// Before the fix, `Agent::run` returned `Err(StepBudgetExceeded)` on
    /// budget overrun, and `run_once` propagated the `?` before the
    /// transcript-save block. Result: `--transcript-out` produced no file,
    /// and self-improve.sh's auto-resume gate `[[ -f $TRANSCRIPT_OUT ]]`
    /// always failed → no resume ever ran.
    ///
    /// Now `Agent::run` returns `Ok(outcome)` with `finish: BudgetExceeded`
    /// and the full transcript populated. The CLI (`main.rs`) is then
    /// expected to save the transcript first and only then bail with a
    /// non-zero exit code via `exit_for_finish`.
    ///
    /// This test pins the agent half of that contract: on budget overrun,
    /// `outcome.transcript` is non-empty AND round-trips through
    /// `TranscriptFile::{write_to,read_from}` cleanly.
    #[tokio::test]
    async fn budget_exceeded_yields_writable_transcript() {
        use crate::TranscriptFile;

        let mut script = Vec::new();
        for i in 0..10 {
            script.push(Completion {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: format!("t{i}"),
                    name: "adder".into(),
                    arguments: json!({"a": i, "b": i + 1}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            });
        }
        let llm = Arc::new(MockProvider::new(script));
        let tools = ToolRegistry::local().register(Arc::new(Adder));
        let mut agent = Agent::builder()
            .llm(llm)
            .tools(tools)
            .max_steps(3)
            .build()
            .unwrap();
        let out = agent.run("loop").await.unwrap();

        assert!(matches!(out.finish, FinishReason::BudgetExceeded));
        assert!(
            !out.transcript.is_empty(),
            "transcript must survive BudgetExceeded for auto-resume"
        );

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let file =
            TranscriptFile::new(out.transcript.clone(), out.steps, Some("mock-model".into()));
        file.write_to(tmp.path()).unwrap();

        let restored = TranscriptFile::read_from(tmp.path()).unwrap();
        assert_eq!(
            restored.messages().len(),
            out.transcript.len(),
            "round-trip transcript length must match"
        );
    }

    #[tokio::test]
    async fn accumulates_usage_across_llm_calls() {
        let u1 = TokenUsage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            cache_hit_tokens: 0,
            cache_miss_tokens: 0,
        };
        let u2 = TokenUsage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            cache_hit_tokens: 0,
            cache_miss_tokens: 0,
        };
        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "step 1".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "add".into(),
                    arguments: json!({"a":1,"b":1}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: Some(u1),
                reasoning_content: None,
            },
            Completion {
                content: "step 2".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: Some(u2),
                reasoning_content: None,
            },
        ]));
        let tools = ToolRegistry::local().register(Arc::new(Adder));
        let mut agent = Agent::builder().llm(llm).tools(tools).build().unwrap();
        let out = agent.run("add twice").await.unwrap();

        assert_eq!(out.total_usage.prompt_tokens, 20);
        assert_eq!(out.total_usage.completion_tokens, 10);
        assert_eq!(out.total_usage.total_tokens, 30);
    }

    #[tokio::test]
    async fn outcome_has_zero_usage_when_provider_never_reports() {
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "done".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let mut agent = Agent::builder().llm(llm).build().unwrap();
        let out = agent.run("hi").await.unwrap();

        assert_eq!(out.total_usage, TokenUsage::default());
    }

    #[tokio::test]
    async fn step_event_usage_emitted_per_llm_call() {
        let u = TokenUsage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            cache_hit_tokens: 0,
            cache_miss_tokens: 0,
        };
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "first".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: Some(u),
            reasoning_content: None,
        }]));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut agent = Agent::builder().llm(llm).events(tx).build().unwrap();
        agent.run("hi").await.unwrap();

        let mut usage_events = 0;
        while let Ok(e) = rx.try_recv() {
            if matches!(e, StepEvent::Usage { .. }) {
                usage_events += 1;
            }
        }
        assert_eq!(usage_events, 1);
    }

    #[tokio::test]
    async fn transcript_limit_stops_loop() {
        // Script many small tool calls so the transcript grows past 50 chars.
        // Each iteration adds: assistant "x" (1 char) + tool result "2" (1 char).
        // User "hi" adds 2 chars. So after N iterations: 2 + 2N chars.
        // To reach 50: N >= 24. Script 30 completions to be safe.
        let mut script = Vec::new();
        for _ in 0..30 {
            script.push(Completion {
                content: "x".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "add".into(),
                    arguments: json!({"a":1,"b":1}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            });
        }
        let llm = Arc::new(MockProvider::new(script));
        let tools = ToolRegistry::local().register(Arc::new(Adder));
        let mut agent = Agent::builder()
            .llm(llm)
            .tools(tools)
            .max_transcript_chars(50)
            .max_steps(100)
            .build()
            .unwrap();
        let out = agent.run("hi").await.unwrap();
        assert!(matches!(out.finish, FinishReason::TranscriptLimit { .. }));
        if let FinishReason::TranscriptLimit { chars, limit } = &out.finish {
            assert!(*chars >= 50);
            assert_eq!(*limit, 50);
        }
    }

    #[tokio::test]
    async fn transcript_limit_unset_runs_to_completion() {
        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "let me add".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "add".into(),
                    arguments: json!({"a":2,"b":3}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "5".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));
        let tools = ToolRegistry::local().register(Arc::new(Adder));
        let mut agent = Agent::builder().llm(llm).tools(tools).build().unwrap();
        let out = agent.run("what is 2+3?").await.unwrap();
        assert_eq!(out.finish, FinishReason::NoMoreToolCalls);
    }

    #[tokio::test]
    async fn transcript_limit_is_checked_before_llm_call() {
        // A massive user goal that already exceeds the limit.
        // Use an empty MockProvider so any actual call would panic.
        let llm = Arc::new(MockProvider::new(vec![]));
        let mut agent = Agent::builder()
            .llm(llm)
            .max_transcript_chars(10)
            .build()
            .unwrap();
        let out = agent
            .run("a very long goal that exceeds the limit")
            .await
            .unwrap();
        assert!(matches!(out.finish, FinishReason::TranscriptLimit { .. }));
        if let FinishReason::TranscriptLimit { chars, limit } = &out.finish {
            assert!(*chars >= 10);
            assert_eq!(*limit, 10);
        }
        // Should have stopped at step 1 without making any LLM call.
        assert_eq!(out.steps, 1);
    }

    #[tokio::test]
    async fn compaction_triggers_with_low_threshold() {
        // Set up an agent with a very low compaction threshold (10 chars)
        // and a script that produces enough messages to exceed it.
        // The MockProvider's first two calls return tool calls to build up
        // the transcript, the third call is consumed by the compactor,
        // and the fourth call returns "done" to finish.
        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "first call".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "add".into(),
                    arguments: json!({"a":1,"b":1}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "second call".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "add".into(),
                    arguments: json!({"a":1,"b":1}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
            // This completion is consumed by the compactor
            Completion {
                content: "Summary: added numbers, tests pass.".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
            // This completion is the agent's next step after compaction
            Completion {
                content: "done".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));
        let tools = ToolRegistry::local().register(Arc::new(Adder));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let compactor = crate::compact::Compactor::new(10).keep_recent_n(2);
        let mut agent = Agent::builder()
            .llm(llm)
            .tools(tools)
            .events(tx)
            .compactor(compactor)
            .max_steps(10)
            .build()
            .unwrap();
        let out = agent.run("hi").await.unwrap();
        assert_eq!(out.finish, FinishReason::NoMoreToolCalls);

        // Check that a Compacted event was emitted
        let mut compacted_count = 0;
        while let Ok(e) = rx.try_recv() {
            if matches!(e, StepEvent::Compacted { .. }) {
                compacted_count += 1;
            }
        }
        assert_eq!(compacted_count, 1, "expected exactly one Compacted event");

        // The transcript should contain the summary message (system role)
        let summary_msgs: Vec<&Message> = out
            .transcript
            .iter()
            .filter(|m| m.role == crate::message::Role::System)
            .collect();
        assert!(
            !summary_msgs.is_empty(),
            "expected at least one system message (the summary)"
        );
        assert!(
            summary_msgs
                .iter()
                .any(|m| m.content.contains("[compacted:")),
            "expected a system message with compacted header"
        );
    }

    #[tokio::test]
    async fn compaction_disabled_by_default() {
        // Without setting a compactor, no compaction should happen.
        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "let me add".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "add".into(),
                    arguments: json!({"a":1,"b":1}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "done".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));
        let tools = ToolRegistry::local().register(Arc::new(Adder));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut agent = Agent::builder()
            .llm(llm)
            .tools(tools)
            .events(tx)
            .max_steps(10)
            .build()
            .unwrap();
        let out = agent.run("hi").await.unwrap();
        assert_eq!(out.finish, FinishReason::NoMoreToolCalls);

        // No Compacted events should be emitted
        let mut compacted_count = 0;
        while let Ok(e) = rx.try_recv() {
            if matches!(e, StepEvent::Compacted { .. }) {
                compacted_count += 1;
            }
        }
        assert_eq!(
            compacted_count, 0,
            "expected no Compacted events by default"
        );
    }

    /// Regression: a compaction whose `keep_recent_n` window started with a
    /// `Role::Tool` message orphaned that tool from its parent assistant's
    /// `tool_calls`. Subsequent LLM requests failed with HTTP 400 (DeepSeek)
    /// or 422 (OpenAI). Fix retreats the split point until the kept window
    /// begins at a non-Tool message.
    ///
    /// Discovered during batch 15 dogfooding: g43 + g47 both rolled back
    /// the moment Compactor fired with the new 200 KB threshold + AGENTS.md
    /// + skill_index inflation.
    ///
    /// Scenario: transcript after step 1 looks like
    ///   [0] System (prompt)
    ///   [1] User (goal)
    ///   [2] Assistant + tool_calls("adder")
    ///   [3] Tool result
    /// With `keep_recent_n=1`, naive split = len-1 = 3 lands on the Tool
    /// message. Without the fix, kept window = [Tool] — orphan. With the
    /// fix, split retreats to 2 (the parent Assistant).
    #[tokio::test]
    async fn compaction_keeps_tool_calls_paired_with_results() {
        use crate::message::Role;
        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "looking...".to_string(),
                tool_calls: vec![ToolCall {
                    id: "call-1".to_string(),
                    name: "adder".to_string(),
                    arguments: json!({"a": 1, "b": 2}),
                }],
                finish_reason: None,
                usage: None,
                reasoning_content: None,
            },
            // Summary returned by the compactor's call to provider.complete()
            Completion {
                content: "Summary of older messages.".to_string(),
                tool_calls: vec![],
                finish_reason: Some("stop".to_string()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "done".to_string(),
                tool_calls: vec![],
                finish_reason: Some("stop".to_string()),
                usage: None,
                reasoning_content: None,
            },
        ]));
        let tools = ToolRegistry::local().register(Arc::new(Adder));
        // keep_recent_n=1 forces the naive split onto the Tool message.
        // min_messages check is keep_recent_n + 2 = 3, satisfied at step 2.
        let compactor = crate::compact::Compactor::new(10).keep_recent_n(1);
        let mut agent = Agent::builder()
            .llm(llm)
            .tools(tools)
            .max_steps(5)
            .compactor(compactor)
            .build()
            .unwrap();

        let out = agent.run("test").await.unwrap();
        assert_eq!(out.finish, FinishReason::NoMoreToolCalls);

        // Sanity: at least one Compacted event must have fired, otherwise
        // the test isn't exercising the code path we care about.
        // (We can't introspect easily here — instead assert via transcript
        // shape: the first message after compaction should be a synthetic
        // System summary with the "[compacted:" header.)
        let first = &out.transcript[0];
        assert!(
            matches!(first.role, Role::System) && first.content.contains("[compacted:"),
            "compaction never fired — transcript[0] = {:?}, content={:?}",
            first.role,
            &first.content.chars().take(60).collect::<String>()
        );

        // The kept transcript must NOT have an orphaned Tool message at
        // position 1 (right after the summary system message at position 0).
        for (i, m) in out.transcript.iter().enumerate() {
            if m.role == Role::Tool {
                assert!(
                    i > 0,
                    "Tool message at index 0 (right after summary) is impossible"
                );
                let prev = &out.transcript[i - 1];
                assert!(
                    matches!(prev.role, Role::Assistant) && !prev.tool_calls.is_empty(),
                    "tool message at index {i} is orphaned — previous message \
                     has role={:?}, tool_calls={}",
                    prev.role,
                    prev.tool_calls.len()
                );
            }
        }
    }

    #[test]
    fn step_event_serializes_with_kind_tag() {
        let ev = StepEvent::AssistantText {
            text: "hello".into(),
            step: 1,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""kind":"assistant_text""#));
        let back: StepEvent = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(back, StepEvent::AssistantText { text, step } if text == "hello" && step == 1)
        );
    }

    #[test]
    fn step_event_tool_call_uses_snake_case() {
        let ev = StepEvent::ToolCall {
            call: ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                arguments: json!({"path": "foo.txt"}),
            },
            step: 2,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""kind":"tool_call""#));
    }

    #[test]
    fn step_event_latency_serializes_with_kind_tag() {
        let ev = StepEvent::Latency {
            step: 3,
            llm_ms: 42,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""kind":"latency""#));
        assert!(json.contains(r#""step":3"#));
        assert!(json.contains(r#""llm_ms":42"#));
        let back: StepEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, StepEvent::Latency { step, llm_ms } if step == 3 && llm_ms == 42));
    }

    #[test]
    fn finish_reason_serializes_with_kind_tag() {
        let fr = FinishReason::Stuck {
            repeated_call: "read_file".into(),
            repeats: 3,
        };
        let json = serde_json::to_string(&fr).unwrap();
        assert!(json.contains(r#""kind":"stuck""#));
        let back: FinishReason = serde_json::from_str(&json).unwrap();
        assert_eq!(back, fr);
    }

    #[test]
    fn finish_reason_transcript_limit_roundtrips() {
        let fr = FinishReason::TranscriptLimit {
            chars: 4096,
            limit: 2048,
        };
        let json = serde_json::to_string(&fr).unwrap();
        let back: FinishReason = serde_json::from_str(&json).unwrap();
        assert_eq!(back, fr);
    }

    #[tokio::test]
    async fn seeded_transcript_lands_before_new_goal() {
        let seed = vec![
            Message::system("sys".to_string()),
            Message::user("old goal".to_string()),
            Message::assistant("old reply".to_string()),
        ];
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "fresh reply".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let mut agent = Agent::builder()
            .llm(llm)
            .seed_transcript(seed)
            .build()
            .unwrap();
        let out = agent.run("new goal").await.unwrap();
        // seed (3) + new user goal + new assistant reply = 5
        assert_eq!(out.transcript.len(), 5);
        assert_eq!(out.transcript[0].content, "sys");
        assert_eq!(out.transcript[1].content, "old goal");
        assert_eq!(out.transcript[2].content, "old reply");
        assert_eq!(out.transcript[3].content, "new goal");
        assert_eq!(out.transcript[4].content, "fresh reply");
    }

    #[tokio::test]
    async fn seed_transcript_does_not_emit_events_for_seed() {
        let seed = vec![
            Message::user("old goal".to_string()),
            Message::assistant("old reply".to_string()),
        ];
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "fresh".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut agent = Agent::builder()
            .llm(llm)
            .seed_transcript(seed)
            .events(tx)
            .build()
            .unwrap();
        agent.run("new goal").await.unwrap();
        let mut kinds: Vec<&'static str> = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            kinds.push(match ev {
                StepEvent::AssistantText { .. } => "text",
                StepEvent::Finished { .. } => "done",
                StepEvent::Latency { .. } => "latency",
                _ => "other",
            });
        }
        // Only events from the new run; nothing fired for seeded messages.
        assert_eq!(kinds, vec!["latency", "text", "done"]);
    }

    #[tokio::test]
    async fn emits_latency_event_per_llm_call() {
        // Run the agent through 2 steps and verify Latency events are emitted.
        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "step 1".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "add".into(),
                    arguments: json!({"a":1,"b":1}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "step 2".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));
        let tools = ToolRegistry::local().register(Arc::new(Adder));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut agent = Agent::builder()
            .llm(llm)
            .tools(tools)
            .events(tx)
            .build()
            .unwrap();
        let out = agent.run("add twice").await.unwrap();

        // total_llm_latency_ms should exist and be u64 (compile check)
        let _: u64 = out.total_llm_latency_ms;

        // Count Latency events
        let mut latency_count = 0;
        while let Ok(e) = rx.try_recv() {
            if matches!(e, StepEvent::Latency { .. }) {
                latency_count += 1;
            }
        }
        // Should have one Latency event per LLM call (2 steps)
        assert_eq!(latency_count, 2);
    }

    // --- Transcript trimming tests ---

    #[tokio::test]
    async fn trims_old_tool_result_to_fit_budget() {
        // Build a transcript with a large tool result followed by a small
        // assistant message. Set max_transcript_chars just under the big
        // result's size so trimming is triggered.
        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "let me run a tool".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "big".into(),
                    arguments: json!({}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "done".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));
        // Use a custom tool that returns a big result
        struct BigResultTool;
        #[async_trait]
        impl Tool for BigResultTool {
            fn spec(&self) -> crate::llm::ToolSpec {
                crate::llm::ToolSpec {
                    name: "big".into(),
                    description: "returns a big result".into(),
                    parameters: json!({"type":"object"}),
                }
            }
            async fn execute(&self, _args: Value) -> Result<String> {
                Ok("x".repeat(500))
            }
        }
        let tools = ToolRegistry::local().register(Arc::new(BigResultTool));
        // Set limit so the big result (500 chars) plus user goal (2 chars)
        // plus assistant text (~17 chars) would exceed, but trimming the
        // tool result to the placeholder (~50 chars) brings it under.
        // User "hi" = 2, assistant "let me run a tool" = 17, tool result
        // placeholder = 50, assistant "done" = 4. Total ~73.
        // Set limit to 100: the big result alone (500) would blow it,
        // but after trimming we're under.
        let mut agent = Agent::builder()
            .llm(llm)
            .tools(tools)
            .max_transcript_chars(100)
            .max_steps(10)
            .build()
            .unwrap();
        let out = agent.run("hi").await.unwrap();
        // Should complete normally, not hit TranscriptLimit
        assert_eq!(out.finish, FinishReason::NoMoreToolCalls);
        // The tool result should have been trimmed
        let tool_msgs: Vec<&Message> = out
            .transcript
            .iter()
            .filter(|m| m.role == crate::message::Role::Tool)
            .collect();
        assert!(!tool_msgs.is_empty());
        for msg in &tool_msgs {
            assert_eq!(msg.content, TRIM_PLACEHOLDER);
        }
    }

    #[tokio::test]
    async fn transcript_limit_fires_when_trimming_not_enough() {
        // Build a transcript where even after trimming all tool results,
        // the non-tool messages alone exceed the budget.
        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "x".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "add".into(),
                    arguments: json!({"a":1,"b":1}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "y".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));
        let tools = ToolRegistry::local().register(Arc::new(Adder));
        // Set a very tight limit that even the user goal alone exceeds.
        let mut agent = Agent::builder()
            .llm(llm)
            .tools(tools)
            .max_transcript_chars(1)
            .max_steps(10)
            .build()
            .unwrap();
        let out = agent.run("hi").await.unwrap();
        assert!(matches!(out.finish, FinishReason::TranscriptLimit { .. }));
    }

    // ========================================================================
    // Permission hook tests
    // ========================================================================

    #[tokio::test]
    async fn permission_hook_allow_passes_args_unchanged() {
        // Hook returns Allow; tool should receive the original args.
        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "let me add".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "add".into(),
                    arguments: json!({"a": 2, "b": 3}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "5".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));
        let tools = ToolRegistry::local().register(Arc::new(Adder));
        let mut agent = Agent::builder()
            .llm(llm)
            .tools(tools)
            .permission_hook(|_name, _args| PermissionDecision::Allow)
            .build()
            .unwrap();
        let out = agent.run("what is 2+3?").await.unwrap();
        assert_eq!(out.final_message.as_deref(), Some("5"));
        assert_eq!(out.steps, 2);
    }

    #[tokio::test]
    async fn permission_hook_deny_returns_error_to_model() {
        // Hook returns Deny; tool should NOT be executed, and the model
        // should receive an error result.
        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "let me add".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "add".into(),
                    arguments: json!({"a": 2, "b": 3}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "i see the error".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));
        let tools = ToolRegistry::local().register(Arc::new(Adder));
        let mut agent = Agent::builder()
            .llm(llm)
            .tools(tools)
            .permission_hook(|_name, _args| PermissionDecision::Deny("not allowed".into()))
            .build()
            .unwrap();
        let out = agent.run("add numbers").await.unwrap();
        // The tool result should contain the denial reason
        let tool_msgs: Vec<&Message> = out
            .transcript
            .iter()
            .filter(|m| m.role == crate::message::Role::Tool)
            .collect();
        assert_eq!(tool_msgs.len(), 1);
        assert!(tool_msgs[0].content.contains("not allowed"));
        // The model should have received the error and responded
        assert_eq!(out.final_message.as_deref(), Some("i see the error"));
    }

    #[tokio::test]
    async fn permission_hook_transform_replaces_args() {
        // Hook returns Transform with different args; tool should receive
        // the transformed args, not the original ones.
        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "let me add".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "add".into(),
                    arguments: json!({"a": 100, "b": 200}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "done".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));
        let tools = ToolRegistry::local().register(Arc::new(Adder));
        // Transform args to add 1+2 instead of 100+200
        let mut agent = Agent::builder()
            .llm(llm)
            .tools(tools)
            .permission_hook(|_name, _args| PermissionDecision::Transform(json!({"a": 1, "b": 2})))
            .build()
            .unwrap();
        let out = agent.run("add numbers").await.unwrap();
        // The tool result should be 3 (1+2), not 300 (100+200)
        let tool_msgs: Vec<&Message> = out
            .transcript
            .iter()
            .filter(|m| m.role == crate::message::Role::Tool)
            .collect();
        assert_eq!(tool_msgs.len(), 1);
        assert_eq!(tool_msgs[0].content, "3");
    }

    #[tokio::test]
    async fn default_no_hook_is_unchanged() {
        // Without permission_hook(), existing behavior is preserved.
        // This is the same as terminates_when_model_emits_no_tool_calls.
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "done".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let mut agent = Agent::builder().llm(llm).build().unwrap();
        let out = agent.run("hi").await.unwrap();
        assert_eq!(out.final_message.as_deref(), Some("done"));
        assert_eq!(out.steps, 1);
        assert_eq!(out.finish, FinishReason::NoMoreToolCalls);
    }
    // --- PlanningMode tests ---

    #[test]
    fn planning_mode_default_is_immediate() {
        assert_eq!(PlanningMode::default(), PlanningMode::Immediate);
    }

    #[test]
    fn planning_mode_variants_are_distinct() {
        assert_ne!(PlanningMode::Immediate, PlanningMode::PlanFirst);
    }

    #[tokio::test]
    async fn builder_with_planfirst_succeeds() {
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "done".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let agent = Agent::builder()
            .llm(llm)
            .planning_mode(PlanningMode::PlanFirst)
            .build()
            .unwrap();
        assert_eq!(agent.planning_mode, PlanningMode::PlanFirst);
        assert!(agent.plan_buffer.is_none());
    }

    #[tokio::test]
    async fn immediate_mode_runs_normally() {
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "done".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let mut agent = Agent::builder().llm(llm).build().unwrap();
        let outcome = agent.run("test").await.unwrap();
        assert_eq!(outcome.finish, FinishReason::NoMoreToolCalls);
    }

    #[test]
    fn plan_proposed_can_be_constructed() {
        let event = StepEvent::PlanProposed {
            plan_text: "Step 1: read file, Step 2: edit file".into(),
            tool_calls: vec![ToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                arguments: json!({"path": "test.txt"}),
            }],
        };
        match &event {
            StepEvent::PlanProposed {
                plan_text,
                tool_calls,
            } => {
                assert!(plan_text.contains("read file"));
                assert_eq!(tool_calls.len(), 1);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn plan_confirmed_can_be_constructed() {
        let event = StepEvent::PlanConfirmed;
        assert!(matches!(event, StepEvent::PlanConfirmed));
    }

    #[test]
    fn plan_rejected_can_be_constructed() {
        let event = StepEvent::PlanRejected {
            reason: "too many steps".into(),
        };
        match &event {
            StepEvent::PlanRejected { reason } => {
                assert_eq!(reason, "too many steps");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn confirm_plan_does_not_panic() {
        let mut agent = Agent {
            llm: Arc::new(MockProvider::new(vec![])),
            tools: ToolRegistry::local(),
            transcript: vec![],
            max_steps: 32,
            max_transcript_chars: None,
            events: None,
            streaming: false,
            total_llm_latency_ms: 0,
            compactor: None,
            permission_hook: None,
            hooks: HookRegistry::new(),
            planning_mode: PlanningMode::Immediate,
            plan_buffer: Some(vec![]),
            plan_confirmed: false,
            on_message: None,
            shutdown_token: None,
        };
        agent.confirm_plan();
        assert!(agent.plan_confirmed);
    }

    #[test]
    fn reject_plan_does_not_panic() {
        let mut agent = Agent {
            llm: Arc::new(MockProvider::new(vec![])),
            tools: ToolRegistry::local(),
            transcript: vec![],
            max_steps: 32,
            max_transcript_chars: None,
            events: None,
            streaming: false,
            total_llm_latency_ms: 0,
            compactor: None,
            permission_hook: None,
            hooks: HookRegistry::new(),
            planning_mode: PlanningMode::Immediate,
            plan_buffer: Some(vec![]),
            plan_confirmed: false,
            on_message: None,
            shutdown_token: None,
        };
        agent.reject_plan("bad plan");
        assert!(agent.plan_buffer.is_none());
        assert!(!agent.plan_confirmed);
    }
}
// ============================================================================

// Tracing tests - require tracing-test
// ============================================================================
#[cfg(test)]
mod tracing_tests {
    use crate::llm::Completion;
    use crate::llm::MockProvider;
    use crate::Agent;
    use std::sync::Arc;
    use tracing_test::traced_test;

    #[traced_test]
    #[tokio::test]
    async fn agent_run_creates_span() {
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "done".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let mut agent = Agent::builder().llm(llm).build().unwrap();
        agent.run("test goal").await.unwrap();

        // The span should have been created (check for the span name in the log prefix)
        assert!(logs_contain("run:"));
    }

    #[traced_test]
    #[tokio::test]
    async fn agent_step_spans_nested_under_run() {
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "step 1".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let mut agent = Agent::builder().llm(llm).build().unwrap();
        agent.run("test").await.unwrap();

        // Should have both run and step spans
        assert!(logs_contain("run:"));
        assert!(logs_contain("step="));
    }
}

// --- PlanningMode tests ---

// ============================================================================
// Parallel execution tests
// ============================================================================
#[cfg(test)]
mod parallel_tests {
    use super::*;
    use crate::llm::{Completion, MockProvider, ToolCall};
    use crate::tools::Tool;
    use crate::ToolSpec;
    use async_trait::async_trait;
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// A read-only tool that records its execution order and simulates latency.
    struct SlowReadOnly {
        counter: Arc<AtomicUsize>,
        delay_ms: u64,
    }

    #[async_trait]
    impl Tool for SlowReadOnly {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "slow_read".into(),
                description: "a slow read-only tool".into(),
                parameters: json!({"type": "object"}),
            }
        }
        fn is_readonly(&self) -> bool {
            true
        }
        async fn execute(&self, _args: Value) -> Result<String> {
            let order = self.counter.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
            Ok(format!("read-{}", order))
        }
    }

    /// A write tool that records its execution order.
    struct TrackingWrite {
        counter: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for TrackingWrite {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "track_write".into(),
                description: "a tracked write tool".into(),
                parameters: json!({"type": "object"}),
            }
        }
        async fn execute(&self, _args: Value) -> Result<String> {
            let order = self.counter.fetch_add(1, Ordering::SeqCst);
            Ok(format!("write-{}", order))
        }
    }

    #[tokio::test]
    async fn read_only_tools_execute_in_parallel() {
        // Two read-only tools with delays. If executed in parallel, total
        // time should be ~100ms (not ~200ms).
        let counter = Arc::new(AtomicUsize::new(0));
        let tool1 = Arc::new(SlowReadOnly {
            counter: counter.clone(),
            delay_ms: 100,
        });
        let tool2 = Arc::new(SlowReadOnly {
            counter: counter.clone(),
            delay_ms: 100,
        });

        let tools = ToolRegistry::local().register(tool1).register(tool2);

        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "reading...".into(),
                tool_calls: vec![
                    ToolCall {
                        id: "c1".into(),
                        name: "slow_read".into(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        id: "c2".into(),
                        name: "slow_read".into(),
                        arguments: json!({}),
                    },
                ],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "done".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));

        let start = std::time::Instant::now();
        let mut agent = Agent::builder()
            .llm(llm)
            .tools(tools)
            .max_steps(5)
            .build()
            .unwrap();
        let out = agent.run("read in parallel").await.unwrap();
        let elapsed = start.elapsed();

        // Should complete in ~100ms (parallel), not ~200ms (sequential)
        assert!(
            elapsed.as_millis() < 150,
            "parallel execution took {}ms, expected <150ms",
            elapsed.as_millis()
        );
        assert_eq!(out.finish, FinishReason::NoMoreToolCalls);

        // Both results should be present in the transcript
        let tool_msgs: Vec<&Message> = out
            .transcript
            .iter()
            .filter(|m| m.role == crate::message::Role::Tool)
            .collect();
        assert_eq!(tool_msgs.len(), 2);
    }

    #[tokio::test]
    async fn write_tools_execute_sequentially() {
        // Two write tools. If executed sequentially, the order counter
        // should increment predictably.
        let counter = Arc::new(AtomicUsize::new(0));
        let tool1 = Arc::new(TrackingWrite {
            counter: counter.clone(),
        });
        let tool2 = Arc::new(TrackingWrite {
            counter: counter.clone(),
        });

        let tools = ToolRegistry::local().register(tool1).register(tool2);

        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "writing...".into(),
                tool_calls: vec![
                    ToolCall {
                        id: "c1".into(),
                        name: "track_write".into(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        id: "c2".into(),
                        name: "track_write".into(),
                        arguments: json!({}),
                    },
                ],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "done".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));

        let mut agent = Agent::builder()
            .llm(llm)
            .tools(tools)
            .max_steps(5)
            .build()
            .unwrap();
        let out = agent.run("write sequentially").await.unwrap();

        // Both results should be present
        let tool_msgs: Vec<&Message> = out
            .transcript
            .iter()
            .filter(|m| m.role == crate::message::Role::Tool)
            .collect();
        assert_eq!(tool_msgs.len(), 2);
        assert_eq!(out.finish, FinishReason::NoMoreToolCalls);
    }

    #[tokio::test]
    async fn mixed_read_write_preserves_order() {
        // Mix of read-only and write tools. Read-only tools should run in
        // parallel, write tools sequentially, but results should appear in
        // the original call order in the transcript.
        let counter = Arc::new(AtomicUsize::new(0));
        let read_tool = Arc::new(SlowReadOnly {
            counter: counter.clone(),
            delay_ms: 10,
        });
        let write_tool = Arc::new(TrackingWrite {
            counter: counter.clone(),
        });

        let tools = ToolRegistry::local()
            // Adder not needed for this test
            .register(read_tool)
            .register(write_tool);

        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "mixed...".into(),
                tool_calls: vec![
                    ToolCall {
                        id: "c1".into(),
                        name: "slow_read".into(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        id: "c2".into(),
                        name: "track_write".into(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        id: "c3".into(),
                        name: "slow_read".into(),
                        arguments: json!({}),
                    },
                ],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "done".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));

        let mut agent = Agent::builder()
            .llm(llm)
            .tools(tools)
            .max_steps(5)
            .build()
            .unwrap();
        let out = agent.run("mixed").await.unwrap();

        // Results should be in original order: read, write, read
        let tool_msgs: Vec<&Message> = out
            .transcript
            .iter()
            .filter(|m| m.role == crate::message::Role::Tool)
            .collect();
        assert_eq!(tool_msgs.len(), 3);
        assert_eq!(out.finish, FinishReason::NoMoreToolCalls);
    }

    #[tokio::test]
    async fn parallel_read_only_with_unknown_tool() {
        // Mix of read-only and unknown tools. Unknown tools should error
        // but not prevent parallel execution of read-only tools.
        let counter = Arc::new(AtomicUsize::new(0));
        let read_tool = Arc::new(SlowReadOnly {
            counter: counter.clone(),
            delay_ms: 10,
        });

        let tools = ToolRegistry::local().register(read_tool);

        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "mixed...".into(),
                tool_calls: vec![
                    ToolCall {
                        id: "c1".into(),
                        name: "slow_read".into(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        id: "c2".into(),
                        name: "unknown_tool".into(),
                        arguments: json!({}),
                    },
                ],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "done".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));

        let mut agent = Agent::builder()
            .llm(llm)
            .tools(tools)
            .max_steps(5)
            .build()
            .unwrap();
        let out = agent.run("mixed with unknown").await.unwrap();

        // Should complete with both results (one error)
        let tool_msgs: Vec<&Message> = out
            .transcript
            .iter()
            .filter(|m| m.role == crate::message::Role::Tool)
            .collect();
        assert_eq!(tool_msgs.len(), 2);
        // The unknown tool should have an error message
        assert!(tool_msgs[1].content.contains("ERROR"));
        assert_eq!(out.finish, FinishReason::NoMoreToolCalls);
    }

    #[tokio::test]
    async fn mixed_read_write_executes_sequentially() {
        // A write tool between read-only tools breaks parallel batching.
        // Calls: [read, write, read] — the write separates the reads into
        // two sequential batches, so total time >= 200ms (not ~100ms).
        let counter = Arc::new(AtomicUsize::new(0));
        let read_tool = Arc::new(SlowReadOnly {
            counter: counter.clone(),
            delay_ms: 100,
        });
        let write_tool = Arc::new(TrackingWrite {
            counter: counter.clone(),
        });

        let tools = ToolRegistry::local()
            .register(read_tool)
            .register(write_tool);

        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "mixed rw...".into(),
                tool_calls: vec![
                    ToolCall {
                        id: "c1".into(),
                        name: "slow_read".into(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        id: "c2".into(),
                        name: "track_write".into(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        id: "c3".into(),
                        name: "slow_read".into(),
                        arguments: json!({}),
                    },
                ],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "done".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));

        let start = std::time::Instant::now();
        let mut agent = Agent::builder()
            .llm(llm)
            .tools(tools)
            .max_steps(5)
            .build()
            .unwrap();
        let out = agent.run("mixed read write").await.unwrap();
        let elapsed = start.elapsed();

        // read(100ms) + write(~0ms) + read(100ms) = >=200ms
        assert!(
            elapsed.as_millis() >= 200,
            "mixed read/write took {}ms, expected >=200ms (write breaks parallel batching)",
            elapsed.as_millis()
        );

        let tool_msgs: Vec<&Message> = out
            .transcript
            .iter()
            .filter(|m| m.role == crate::message::Role::Tool)
            .collect();
        assert_eq!(tool_msgs.len(), 3);
        assert_eq!(out.finish, FinishReason::NoMoreToolCalls);
    }

    #[tokio::test]
    async fn single_tool_call_unaffected_by_parallel_mode() {
        // A single read-only tool call should execute normally regardless
        // of parallel execution logic. Regression test.
        let counter = Arc::new(AtomicUsize::new(0));
        let read_tool = Arc::new(SlowReadOnly {
            counter: counter.clone(),
            delay_ms: 10,
        });

        let tools = ToolRegistry::local().register(read_tool);

        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "single call...".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "slow_read".into(),
                    arguments: json!({}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "done".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));

        let mut agent = Agent::builder()
            .llm(llm)
            .tools(tools)
            .max_steps(5)
            .build()
            .unwrap();
        let out = agent.run("single tool call").await.unwrap();

        let tool_msgs: Vec<&Message> = out
            .transcript
            .iter()
            .filter(|m| m.role == crate::message::Role::Tool)
            .collect();
        assert_eq!(tool_msgs.len(), 1);
        assert!(tool_msgs[0].content.contains("read-0"));
        assert_eq!(out.finish, FinishReason::NoMoreToolCalls);
    }

    #[tokio::test]
    async fn multiple_write_tools_preserve_order() {
        // 3 write tool calls must execute in strict order: 0, 1, 2.
        let counter = Arc::new(AtomicUsize::new(0));
        let write_tool = Arc::new(TrackingWrite {
            counter: counter.clone(),
        });

        let tools = ToolRegistry::local().register(write_tool);

        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "writing three...".into(),
                tool_calls: vec![
                    ToolCall {
                        id: "c1".into(),
                        name: "track_write".into(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        id: "c2".into(),
                        name: "track_write".into(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        id: "c3".into(),
                        name: "track_write".into(),
                        arguments: json!({}),
                    },
                ],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "done".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));

        let mut agent = Agent::builder()
            .llm(llm)
            .tools(tools)
            .max_steps(5)
            .build()
            .unwrap();
        let out = agent.run("write three times").await.unwrap();

        let tool_msgs: Vec<&Message> = out
            .transcript
            .iter()
            .filter(|m| m.role == crate::message::Role::Tool)
            .collect();
        assert_eq!(tool_msgs.len(), 3);
        // Verify strict sequential order via atomic counter
        assert!(tool_msgs[0].content.contains("write-0"));
        assert!(tool_msgs[1].content.contains("write-1"));
        assert!(tool_msgs[2].content.contains("write-2"));
        assert_eq!(out.finish, FinishReason::NoMoreToolCalls);
    }

    #[tokio::test]
    async fn no_tool_calls_completes_immediately() {
        // When the provider returns a completion with empty tool_calls and
        // finish_reason "stop", the agent should complete with NoMoreToolCalls.
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "nothing to do".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));

        let mut agent = Agent::builder().llm(llm).max_steps(5).build().unwrap();
        let out = agent.run("no tools needed").await.unwrap();

        assert_eq!(out.finish, FinishReason::NoMoreToolCalls);
        assert_eq!(out.final_message.as_deref(), Some("nothing to do"));
        assert_eq!(out.steps, 1);
        // No tool messages in transcript
        let tool_msgs: Vec<&Message> = out
            .transcript
            .iter()
            .filter(|m| m.role == crate::message::Role::Tool)
            .collect();
        assert_eq!(tool_msgs.len(), 0);
    }
}
