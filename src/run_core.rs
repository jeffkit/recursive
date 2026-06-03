//! Stateless execution kernel — `RunCore<'a>` and `RunInnerOutcome`.
//!
//! Extracted from `src/agent.rs` (Goal 213).  `RunCore` owns all the state
//! needed for one ReAct loop iteration and is consumed by `run_inner()`.
//! The containing `AgentKernel` constructs a `RunCore`, calls `run_inner()`,
//! and integrates the returned `RunInnerOutcome`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::compact::Compactor;
use crate::error::{Error, Result};
use crate::hooks::{HookAction, HookEvent, HookRegistry};
use crate::llm::{Completion, LlmProvider, StreamSender, TokenUsage, ToolCall};
use crate::message::Message;
use crate::permissions::PermissionMode;
use crate::tools::ToolRegistry;

use crate::agent::{FinishReason, PermissionDecision, PermissionHook, PlanningMode};
use crate::event::AgentEvent;

/// Placeholder text used when trimming old tool results to fit the transcript budget.
pub(crate) const TRIM_PLACEHOLDER: &str = "[older tool output trimmed to fit budget]";

/// Threshold for consecutive identical failing tool calls before declaring stuck.
const STUCK_THRESHOLD: usize = 3;

/// Render a [`FinishReason`] as the `reason` field of [`AgentEvent::TurnFinished`].
///
/// The old `From<StepEvent>` bridge used a hard-coded `"finished"` placeholder
/// (see `event::From<StepEvent>` prior to Goal 219), which made the reason
/// unobservable downstream. This helper restores per-variant text so SDK and
/// TUI consumers can distinguish termination causes.
fn finish_reason_str(reason: &FinishReason) -> String {
    match reason {
        FinishReason::NoMoreToolCalls => "no_more_tool_calls".to_string(),
        FinishReason::BudgetExceeded => "budget_exceeded".to_string(),
        FinishReason::ProviderStop(s) => format!("provider_stop({s})"),
        FinishReason::Stuck { .. } => "stuck".to_string(),
        FinishReason::TranscriptLimit { .. } => "transcript_limit".to_string(),
        FinishReason::PlanPending => "plan_pending".to_string(),
        FinishReason::Cancelled => "cancelled".to_string(),
        FinishReason::PermissionDenialLimit => "permission_denial_limit".to_string(),
    }
}

/// Outcome returned by the stateless [`run_inner`] loop.
pub(crate) struct RunInnerOutcome {
    pub(crate) messages: Vec<Message>,
    pub(crate) final_message: Option<String>,
    pub(crate) finish_reason: FinishReason,
    pub(crate) total_usage: TokenUsage,
    pub(crate) total_llm_latency_ms: u64,
    pub(crate) steps: usize,
    pub(crate) plan_buffer: Option<Vec<ToolCall>>,
    pub(crate) plan_confirmed: bool,
    /// Goal-153: audit metadata for tool results, keyed by `tool_call_id`.
    /// Only entries for successfully-dispatched tool calls are present;
    /// the key matches `Message.tool_call_id` on `Role::Tool` messages.
    pub(crate) tool_audits: std::collections::HashMap<String, crate::tools::AuditMeta>,
}

/// Private core holding all state needed for one run of the ReAct loop.
///
/// Borrows immutable config from the parent [`Agent`]; owns the mutable
/// transcript and plan state.  `run_inner()` consumes `self` so the
/// loop cannot accidentally leave stale state behind.
pub(crate) struct RunCore<'a> {
    pub(crate) messages: Vec<Message>,
    pub(crate) llm: Arc<dyn LlmProvider>,
    pub(crate) tools: ToolRegistry,
    pub(crate) max_steps: usize,
    pub(crate) max_transcript_chars: Option<usize>,
    pub(crate) events: Option<mpsc::UnboundedSender<AgentEvent>>,
    pub(crate) streaming: bool,
    pub(crate) compactor: Option<Compactor>,
    pub(crate) permission_hook: Option<PermissionHook>,
    pub(crate) hooks: &'a HookRegistry,
    pub(crate) planning_mode: PlanningMode,
    pub(crate) total_llm_latency_ms: u64,
    pub(crate) plan_buffer: Option<Vec<ToolCall>>,
    pub(crate) plan_confirmed: bool,
    /// Goal-165: shared flag set by `EnterPlanModeTool`; blocks write tools
    /// while the agent is in read-only exploring / planning mode.
    pub(crate) exploring_plan_mode: Arc<AtomicBool>,
    #[allow(dead_code)]
    /// Goal-190: default permission mode for tools not covered by explicit
    /// config lists. Used to check if a tool requires plan mode.
    pub(crate) permission_mode: PermissionMode,
    /// Optional cancellation token. When cancelled, the step loop
    /// terminates at the next step boundary with
    /// [`FinishReason::Cancelled`].
    pub(crate) shutdown_token: Option<CancellationToken>,

    /// Optional mailbox for receiving mid-run messages from a coordinator.
    ///
    /// Drained at the start of every step so the agent sees coordinator
    /// instructions as user-role messages injected into the conversation.
    pub(crate) mailbox: Option<crate::tools::send_message::WorkerMailbox>,
}

impl<'a> RunCore<'a> {
    fn emit(&self, event: AgentEvent) {
        if let Some(ref tx) = self.events {
            let _ = tx.send(event);
        }
    }

    fn push_message(&mut self, msg: Message) {
        self.messages.push(msg);
    }

    /// Trim old tool results to fit the transcript under `limit` chars.
    fn maybe_trim_transcript(&mut self, limit: usize, step: usize) {
        let mut chars: usize = self.messages.iter().map(|m| m.content.len()).sum();
        if chars < limit {
            return;
        }

        let mut trimmed_count: usize = 0;
        let placeholder_len = TRIM_PLACEHOLDER.len();

        for msg in self.messages.iter_mut().skip(1) {
            if msg.role == crate::message::Role::Tool && msg.content.len() > 200 {
                let old_len = msg.content.len();
                msg.content = TRIM_PLACEHOLDER.to_string();
                trimmed_count += 1;
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
            self.emit(AgentEvent::AssistantText { text: note, step });
        }
    }

    /// Compact the transcript if a [`Compactor`] is configured and the
    /// character budget is exceeded.
    async fn maybe_compact(&mut self, step: usize) -> Result<()> {
        let compactor = match &self.compactor {
            Some(c) => c,
            None => return Ok(()),
        };

        let chars = Compactor::estimate_chars(&self.messages);
        if chars < compactor.threshold_chars {
            return Ok(());
        }

        let min_messages = compactor.keep_recent_n + 2;
        if self.messages.len() < min_messages {
            return Ok(());
        }

        let summary_msg = compactor.compact(self.llm.as_ref(), &self.messages).await?;
        let summary_chars = summary_msg.content.len();

        let keep = compactor.keep_recent_n;
        let mut split = self.messages.len().saturating_sub(keep);

        // Invariant: every `Role::Tool` message must be immediately preceded by
        // an `Role::Assistant` message containing the matching `tool_calls`.
        while split > 0 && matches!(self.messages[split].role, crate::message::Role::Tool) {
            split -= 1;
        }

        let removed = split;
        let kept = self.messages.len() - split;

        self.messages.drain(..split);
        self.messages.insert(0, summary_msg);

        self.hooks.dispatch(HookEvent::PostCompact {
            removed,
            summary_chars,
        });

        self.emit(AgentEvent::Compacted {
            removed,
            kept,
            summary_chars,
            step,
        });

        Ok(())
    }

    /// Execute a set of tool calls, returning `(id, name, output, args)` for each.
    /// Read-only calls are batched and executed in parallel; write calls run
    /// sequentially to preserve ordering guarantees.
    async fn execute_tool_calls(
        &self,
        calls: &[ToolCall],
    ) -> Vec<(
        String,
        String,
        String,
        serde_json::Value,
        Option<crate::tools::AuditMeta>,
    )> {
        type ResultRow = (
            String,
            String,
            String,
            serde_json::Value,
            Option<crate::tools::AuditMeta>,
        );
        let mut results: Vec<ResultRow> = Vec::new();

        struct PendingCall {
            id: String,
            name: String,
            args: serde_json::Value,
        }
        let mut pending: Vec<PendingCall> = Vec::new();

        for call in calls {
            // Goal-165: while in agent-driven plan mode, block any write tool
            // that is not `exit_plan_mode` itself.
            if self.exploring_plan_mode.load(Ordering::Relaxed)
                && !self.tools.is_readonly(&call.name)
                && call.name != "exit_plan_mode"
            {
                results.push((
                    call.id.clone(),
                    call.name.clone(),
                    format!(
                        "ERROR: Cannot execute '{}' in plan mode. \
                         You are in read-only planning mode. \
                         Explore freely with read tools, then call \
                         exit_plan_mode with your plan.",
                        call.name
                    ),
                    call.arguments.clone(),
                    None,
                ));
                continue;
            }

            // Goal-190: if a tool is in Plan mode and we're not in plan mode,
            // tell the agent to enter plan mode first.
            if !self.exploring_plan_mode.load(Ordering::Relaxed)
                && self.tools.is_plan_mode(&call.name)
                && call.name != "enter_plan_mode"
                && call.name != "exit_plan_mode"
            {
                results.push((
                    call.id.clone(),
                    call.name.clone(),
                    format!(
                        "ERROR: Tool '{}' requires plan mode. \
                         Call enter_plan_mode first to explore and plan, \
                         then call exit_plan_mode with your plan before using this tool.",
                        call.name
                    ),
                    call.arguments.clone(),
                    None,
                ));
                continue;
            }

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
                            None,
                        ));
                        continue;
                    }
                    PermissionDecision::Transform(new_args) => new_args,
                }
            } else {
                call.arguments.clone()
            };

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
                        None,
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
                        None,
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

        let mut i = 0;
        while i < pending.len() {
            if self
                .tools
                .is_readonly_for_call(&pending[i].name, &pending[i].args)
            {
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
                    let id = pc.id.clone();
                    let args = pc.args.clone();
                    let tools = self.tools.clone();
                    join_set.spawn(async move {
                        let tool_start = std::time::Instant::now();
                        let dispatch = tools.invoke_with_audit(&name, args.clone()).await;
                        let result = match dispatch.result {
                            Ok(output) => output,
                            Err(err) => format!("ERROR: {err}"),
                        };
                        let duration_ms = tool_start.elapsed().as_millis() as u64;
                        (id, name, result, args, dispatch.audit, duration_ms)
                    });
                }

                type BatchRow = (
                    String,
                    String,
                    String,
                    serde_json::Value,
                    crate::tools::AuditMeta,
                    u64,
                );
                let mut batch_results: Vec<BatchRow> = Vec::new();
                while let Some(res) = join_set.join_next().await {
                    batch_results.push(res.unwrap());
                }

                for pc in &batch {
                    let (_, _, result, _, audit, duration_ms) = batch_results
                        .iter()
                        .find(|(id, _, _, _, _, _)| id == &pc.id)
                        .unwrap();
                    results.push((
                        pc.id.clone(),
                        pc.name.clone(),
                        result.clone(),
                        pc.args.clone(),
                        Some(audit.clone()),
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
                let dispatch = self
                    .tools
                    .invoke_with_audit(&pc.name, pc.args.clone())
                    .await;
                let result = match dispatch.result {
                    Ok(output) => output,
                    Err(err) => format!("ERROR: {err}"),
                };
                let duration_ms = tool_start.elapsed().as_millis() as u64;
                results.push((
                    pc.id.clone(),
                    pc.name.clone(),
                    result.clone(),
                    pc.args.clone(),
                    Some(dispatch.audit),
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

    /// Stateless core ReAct loop.  Consumes `self` and returns a
    /// [`RunInnerOutcome`] that the wrapper integrates into the parent agent.
    pub(crate) async fn run_inner(mut self) -> Result<RunInnerOutcome> {
        let specs = self.tools.specs();

        let mut final_message: Option<String> = None;
        let mut last_call_key: Option<(String, String)> = None;
        let mut consecutive_errors: usize = 0;
        let mut total_usage = TokenUsage::default();
        // Goal-153: audit metadata for tool calls, keyed by tool_call_id.
        let mut tool_audits: std::collections::HashMap<String, crate::tools::AuditMeta> =
            std::collections::HashMap::new();
        self.total_llm_latency_ms = 0;

        for step in 1..=self.max_steps {
            let step_span = tracing::info_span!("agent.step", step);
            let _guard = step_span.enter();

            // ---- shutdown cancellation -------------------------------------------
            // Termination check, parallel to the BudgetExceeded /
            // TranscriptLimit blocks below. If a CancellationToken was
            // configured (via AgentRuntimeBuilder::shutdown_token / etc.)
            // and it fired between steps, finish cleanly with
            // FinishReason::Cancelled. Reports `step - 1` because the
            // current step's LLM call has not been started yet — the
            // last fully-completed step is the previous one.
            if let Some(ref token) = self.shutdown_token {
                if token.is_cancelled() {
                    let finished_steps = step.saturating_sub(1);
                    let finish = FinishReason::Cancelled;
                    self.emit(AgentEvent::TurnFinished {
                        reason: finish_reason_str(&finish),
                        steps: finished_steps,
                    });
                    tracing::info!(
                        target: "recursive::agent",
                        steps = finished_steps,
                        tokens_in = total_usage.prompt_tokens,
                        tokens_out = total_usage.completion_tokens,
                        finish = ?finish,
                        llm_latency_ms = self.total_llm_latency_ms,
                        "agent.run.complete"
                    );
                    return Ok(RunInnerOutcome {
                        messages: self.messages,
                        final_message,
                        finish_reason: finish,
                        total_usage,
                        total_llm_latency_ms: self.total_llm_latency_ms,
                        steps: finished_steps,
                        plan_buffer: self.plan_buffer,
                        plan_confirmed: self.plan_confirmed,
                        tool_audits,
                    });
                }
            }

            // ---- mailbox drain (coordinator → worker mid-run messages) -----------
            // Drain any messages that a coordinator pushed via `send_message`.
            // Each pending message is appended as a user-role turn so the LLM
            // sees coordinator instructions on the next reasoning step.
            if let Some(ref mailbox) = self.mailbox {
                let pending = mailbox.drain_all().await;
                for msg_text in pending {
                    self.push_message(Message {
                        role: crate::message::Role::User,
                        content: format!("[coordinator]: {msg_text}"),
                        tool_calls: vec![],
                        tool_call_id: None,
                        reasoning_content: None,
                    });
                }
            }

            // ---- transcript budget ------------------------------------------------
            if let Some(limit) = self.max_transcript_chars {
                self.maybe_trim_transcript(limit, step);
                let chars: usize = self.messages.iter().map(|m| m.content.len()).sum();
                if chars >= limit {
                    let finish = FinishReason::TranscriptLimit { chars, limit };
                    self.emit(AgentEvent::TurnFinished {
                        reason: finish_reason_str(&finish),
                        steps: step,
                    });
                    tracing::info!(
                        target: "recursive::agent",
                        steps = step,
                        tokens_in = total_usage.prompt_tokens,
                        tokens_out = total_usage.completion_tokens,
                        finish = ?finish,
                        llm_latency_ms = self.total_llm_latency_ms,
                        "agent.run.complete"
                    );
                    return Ok(RunInnerOutcome {
                        messages: self.messages,
                        final_message,
                        finish_reason: finish,
                        total_usage,
                        total_llm_latency_ms: self.total_llm_latency_ms,
                        steps: step,
                        plan_buffer: self.plan_buffer,
                        plan_confirmed: self.plan_confirmed,
                        tool_audits,
                    });
                }
            }

            // ---- compaction -------------------------------------------------------
            self.hooks.dispatch(HookEvent::PreCompact {
                transcript_len: self.messages.iter().map(|m| m.content.len()).sum(),
            });
            self.maybe_compact(step).await?;

            // ---- plan-confirmed execution -----------------------------------------
            if self.plan_confirmed {
                self.plan_confirmed = false;
                if let Some(calls) = self.plan_buffer.take() {
                    let results = self.execute_tool_calls(&calls).await;
                    for (id, name, output, _args, _audit) in results {
                        self.emit(AgentEvent::ToolResult {
                            id: id.clone(),
                            name: name.clone(),
                            output: output.clone(),
                            step,
                        });
                        self.push_message(Message::tool_result(id, output));
                    }
                    continue;
                }
            }

            // ---- LLM call ---------------------------------------------------------
            debug!(target: "recursive::agent", step, "calling llm");
            let start = std::time::Instant::now();
            let completion: Completion = if self.streaming {
                let (delta_tx, mut delta_rx) = mpsc::unbounded_channel::<String>();
                let stream_tx: Option<StreamSender> = Some(delta_tx);
                let events_tx = self.events.clone();
                tokio::spawn(async move {
                    while let Some(text) = delta_rx.recv().await {
                        if let Some(ref tx) = events_tx {
                            let _ = tx.send(AgentEvent::PartialToken { text, step });
                        }
                    }
                });
                self.llm.stream(&self.messages, &specs, stream_tx).await?
            } else {
                self.llm.complete(&self.messages, &specs).await?
            };
            let llm_ms = start.elapsed().as_millis() as u64;
            self.total_llm_latency_ms = self.total_llm_latency_ms.saturating_add(llm_ms);
            self.emit(AgentEvent::Latency { step, llm_ms });

            if let Some(u) = completion.usage {
                total_usage = total_usage.accumulate(u);
                self.emit(AgentEvent::Usage {
                    input_tokens: u.prompt_tokens,
                    output_tokens: u.completion_tokens,
                    step,
                });
            }

            if !completion.content.is_empty() {
                self.emit(AgentEvent::AssistantText {
                    text: completion.content.clone(),
                    step,
                });
                final_message = Some(completion.content.clone());
            }

            // ---- no tool calls → finish -------------------------------------------
            if completion.tool_calls.is_empty() {
                if matches!(completion.finish_reason.as_deref(), Some("length")) {
                    let finish = FinishReason::ProviderStop("length".into());
                    self.emit(AgentEvent::TurnFinished {
                        reason: finish_reason_str(&finish),
                        steps: step,
                    });
                    return Err(Error::ProviderTruncated("length".into()));
                }

                self.push_message(Message::assistant(completion.content.clone()));
                if completion.reasoning_content.is_some() {
                    if let Some(msg) = self.messages.last_mut() {
                        msg.reasoning_content = completion.reasoning_content.clone();
                    }
                }
                let finish = match completion.finish_reason {
                    Some(r) if r != "stop" && r != "end_turn" => FinishReason::ProviderStop(r),
                    _ => FinishReason::NoMoreToolCalls,
                };
                self.emit(AgentEvent::TurnFinished {
                    reason: finish_reason_str(&finish),
                    steps: step,
                });
                let outcome = RunInnerOutcome {
                    messages: self.messages,
                    final_message,
                    finish_reason: finish,
                    total_usage,
                    total_llm_latency_ms: self.total_llm_latency_ms,
                    steps: step,
                    plan_buffer: self.plan_buffer,
                    plan_confirmed: self.plan_confirmed,
                    tool_audits,
                };
                return Ok(outcome);
            }

            self.push_message(Message::assistant_with_tool_calls(
                completion.content.clone(),
                completion.tool_calls.clone(),
            ));
            if completion.reasoning_content.is_some() {
                if let Some(msg) = self.messages.last_mut() {
                    msg.reasoning_content = completion.reasoning_content.clone();
                }
            }

            for call in &completion.tool_calls {
                self.emit(AgentEvent::ToolCall {
                    name: call.name.clone(),
                    id: call.id.clone(),
                    arguments: call.arguments.to_string(),
                    step,
                });
            }

            // ---- planning mode ----------------------------------------------------
            if self.planning_mode == PlanningMode::PlanFirst && self.plan_buffer.is_none() {
                self.plan_buffer = Some(completion.tool_calls.clone());

                let plan_text = completion
                    .tool_calls
                    .iter()
                    .map(|tc| {
                        let args_str = serde_json::to_string(&tc.arguments).unwrap_or_default();
                        format!("  - {}({})", tc.name, args_str)
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let plan_text = format!(
                    "The agent proposes the following steps:\n{}\n\nConfirm or reject this plan.",
                    plan_text
                );

                self.emit(AgentEvent::PlanProposed {
                    plan_text: plan_text.clone(),
                    tool_calls: completion
                        .tool_calls
                        .iter()
                        .map(|tc| {
                            serde_json::json!({
                                "name": tc.name,
                                "id": tc.id,
                                "arguments": tc.arguments,
                            })
                        })
                        .collect(),
                });

                tracing::info!(
                    target: "recursive::agent",
                    steps = step,
                    tokens_in = 0usize,
                    tokens_out = 0usize,
                    finish = ?FinishReason::PlanPending,
                    llm_latency_ms = self.total_llm_latency_ms,
                    "agent.run.complete"
                );
                return Ok(RunInnerOutcome {
                    messages: self.messages,
                    final_message: Some(plan_text),
                    finish_reason: FinishReason::PlanPending,
                    total_usage: TokenUsage::default(),
                    total_llm_latency_ms: self.total_llm_latency_ms,
                    steps: step,
                    plan_buffer: self.plan_buffer,
                    plan_confirmed: self.plan_confirmed,
                    tool_audits,
                });
            }

            // ---- tool execution ---------------------------------------------------
            let results = self.execute_tool_calls(&completion.tool_calls).await;

            for (id, name, result, args, audit) in &results {
                self.emit(AgentEvent::ToolResult {
                    id: id.clone(),
                    name: name.clone(),
                    output: result.clone(),
                    step,
                });
                // Goal-153: accumulate audit keyed by tool_call_id.
                if let Some(a) = audit {
                    tool_audits.insert(id.clone(), a.clone());
                }

                let call_key = (
                    name.clone(),
                    serde_json::to_string(args).unwrap_or_default(),
                );
                let is_error = result.starts_with("ERROR:");

                if is_error {
                    if last_call_key == Some(call_key.clone()) {
                        consecutive_errors += 1;
                    } else {
                        consecutive_errors = 1;
                    }
                } else {
                    consecutive_errors = 0;
                }

                last_call_key = Some(call_key);

                if consecutive_errors >= STUCK_THRESHOLD {
                    let repeated_call = name.clone();
                    let repeats = consecutive_errors;
                    let finish = FinishReason::Stuck {
                        repeated_call,
                        repeats,
                    };
                    self.emit(AgentEvent::TurnFinished {
                        reason: finish_reason_str(&finish),
                        steps: step,
                    });
                    let outcome = RunInnerOutcome {
                        messages: self.messages,
                        final_message,
                        finish_reason: finish,
                        total_usage,
                        total_llm_latency_ms: self.total_llm_latency_ms,
                        steps: step,
                        plan_buffer: self.plan_buffer,
                        plan_confirmed: self.plan_confirmed,
                        tool_audits,
                    };
                    return Ok(outcome);
                }

                self.push_message(Message::tool_result(id.clone(), result.clone()));
            }
        }

        warn!(target: "recursive::agent", "step budget exceeded");
        let finish = FinishReason::BudgetExceeded;
        self.emit(AgentEvent::TurnFinished {
            reason: finish_reason_str(&finish),
            steps: self.max_steps,
        });
        let outcome = RunInnerOutcome {
            messages: self.messages,
            final_message,
            finish_reason: finish,
            total_usage,
            total_llm_latency_ms: self.total_llm_latency_ms,
            steps: self.max_steps,
            plan_buffer: self.plan_buffer,
            plan_confirmed: self.plan_confirmed,
            tool_audits,
        };
        Ok(outcome)
    }
}
