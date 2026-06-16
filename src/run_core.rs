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

/// Output of a single tool call execution, replacing the anonymous 5-tuple.
pub(crate) struct ToolCallOutcome {
    pub id: String,
    pub name: String,
    pub result: String,
    pub audit: Option<crate::tools::AuditMeta>,
}

/// Maximum number of automatic retries for retryable LLM errors (rate limits, timeouts).
const LLM_MAX_RETRIES: u32 = 3;
/// Base delay for exponential back-off on LLM retries (milliseconds).
const LLM_RETRY_BASE_MS: u64 = 1_000;
/// Sentinel returned in the tool-result string when the permission denial
/// limit is exceeded.  Matches the check in `run_inner` that breaks the
/// ReAct loop.  Defined as a constant to avoid scattered string literals.
pub(crate) const DENIAL_LIMIT_SENTINEL: &str = "ERROR_DENIAL_LIMIT:";

use crate::compact::Compactor;
use crate::error::Result;
use crate::hooks::{HookAction, HookEvent, HookRegistry};
use crate::llm::{Completion, LlmProvider, StreamSender, TokenUsage, ToolCall};
use crate::message::Message;
use crate::permissions::PermissionMode;
use crate::tools::plan_mode::{ENTER_PLAN_MODE_TOOL_NAME, EXIT_PLAN_MODE_TOOL_NAME};
use crate::tools::ToolRegistry;

use crate::agent::{FinishReason, PermissionDecision};
use crate::event::AgentEvent;
use crate::tools::PermissionHook;

/// Placeholder text used when trimming old tool results to fit the transcript budget.
pub(crate) const TRIM_PLACEHOLDER: &str = "[older tool output trimmed to fit budget]";

/// Render a [`FinishReason`] as the `reason` field of [`AgentEvent::TurnFinished`].
///
/// Delegates to the `Display` implementation on `FinishReason` to ensure
/// consistency between event payloads and HTTP API responses.
fn finish_reason_str(reason: &FinishReason) -> String {
    reason.to_string()
}

/// Outcome returned by the stateless [`run_inner`] loop.
pub(crate) struct RunInnerOutcome {
    pub(crate) messages: Vec<Message>,
    pub(crate) final_message: Option<String>,
    pub(crate) finish_reason: FinishReason,
    pub(crate) total_usage: TokenUsage,
    pub(crate) total_llm_latency_ms: u64,
    pub(crate) steps: usize,
    /// Goal-153: audit metadata for tool results, keyed by `(turn, tool_call_id)`.
    /// Only entries for successfully-dispatched tool calls are present;
    /// the key matches `Message.tool_call_id` on `Role::Tool` messages scoped
    /// by turn to prevent cross-turn id collisions.
    pub(crate) tool_audits:
        std::collections::HashMap<crate::tools::AuditKey, crate::tools::AuditMeta>,
}

/// Private core holding all state needed for one run of the ReAct loop.
///
/// Borrows immutable config from the parent [`Agent`]; owns the mutable
/// transcript and plan state.  `run_inner()` consumes `self` so the
/// loop cannot accidentally leave stale state behind.
pub(crate) struct RunCore<'a> {
    pub(crate) messages: Vec<Message>,
    pub(crate) llm: Arc<dyn LlmProvider>,
    pub(crate) tools: Arc<ToolRegistry>,
    pub(crate) max_steps: usize,
    pub(crate) max_transcript_chars: Option<usize>,
    pub(crate) events: Option<mpsc::UnboundedSender<AgentEvent>>,
    pub(crate) streaming: bool,
    pub(crate) compactor: Option<Compactor>,
    pub(crate) permission_hook: Option<Arc<dyn PermissionHook>>,
    pub(crate) hooks: &'a HookRegistry,
    pub(crate) total_llm_latency_ms: u64,
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
    /// Sliding window size for stuck detection (from Config).
    pub(crate) stuck_window: usize,
    /// Error rate threshold to declare stuck (from Config).
    pub(crate) stuck_error_rate: f64,
    /// Turn index (0-based), scopes [`crate::tools::AuditKey`] prefixes so
    /// that cross-turn tool_call_id reuse cannot corrupt audit metadata.
    pub(crate) turn: u32,
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

    /// Call the LLM with exponential back-off retry for retryable errors.
    ///
    /// When the registry has deferred tools, their names are injected as an
    /// `<available-deferred-tools>` user message prepended to the transcript,
    /// and only eager tool specs are passed to the provider. ToolSearchTool
    /// (registered by `freeze_deferred_specs`) appears in the eager list and
    /// its results in the message history are serialized as `tool_reference`
    /// blocks by `serialize_messages_anthropic`.
    async fn call_llm_with_retry(
        &self,
        specs: &[crate::llm::ToolSpec],
        stream_sender: Option<crate::llm::StreamSender>,
        step: usize,
    ) -> crate::error::Result<Completion> {
        // Deferred-tool partition: only when the provider supports tool_reference
        // (Anthropic). Other providers (OpenAI-compatible) receive all tools eagerly
        // — ToolSearchTool is not registered for them (see AgentRuntimeBuilder::build).
        let eager_specs_owned: Vec<crate::llm::ToolSpec>;
        let messages_with_deferred: Vec<crate::message::Message>;

        let (call_specs, messages): (&[crate::llm::ToolSpec], &[crate::message::Message]) =
            if self.llm.supports_deferred_tools() {
                let (eager, deferred): (Vec<_>, Vec<_>) = specs
                    .iter()
                    .cloned()
                    .partition(|s| !self.tools.is_deferred_spec(s));

                if deferred.is_empty() {
                    (specs, &self.messages)
                } else {
                    let names: Vec<&str> = deferred.iter().map(|s| s.name.as_str()).collect();
                    let block = format!(
                        "<available-deferred-tools>\n{}\n</available-deferred-tools>",
                        names.join("\n")
                    );
                    messages_with_deferred = std::iter::once(crate::message::Message::user(block))
                        .chain(self.messages.iter().cloned())
                        .collect();
                    eager_specs_owned = eager;
                    (&eager_specs_owned, &messages_with_deferred)
                }
            } else {
                (specs, &self.messages)
            };

        let mut attempt = 0u32;
        loop {
            let result = if let Some(ref tx) = stream_sender {
                self.llm
                    .stream(messages, call_specs, Some(tx.clone()))
                    .await
            } else {
                self.llm.complete(messages, call_specs).await
            };
            match result {
                Ok(c) => return Ok(c),
                Err(e) if e.is_retryable() && attempt < LLM_MAX_RETRIES => {
                    let wait_ms =
                        if let crate::error::Error::RateLimited { retry_after_ms, .. } = &e {
                            *retry_after_ms
                        } else {
                            LLM_RETRY_BASE_MS << attempt
                        };
                    warn!(
                        step,
                        attempt,
                        wait_ms,
                        error = %e,
                        "llm retryable error — backing off"
                    );
                    // Goal-287: surface retry to event consumers (TUI, SDK).
                    self.emit(AgentEvent::LlmRetry {
                        step,
                        attempt: attempt + 1,
                        wait_ms,
                        reason: match &e {
                            crate::error::Error::RateLimited { .. } => "rate_limited".to_string(),
                            _ => "timeout".to_string(),
                        },
                    });
                    tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
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
    ///
    /// `PreCompact` and `PostCompact` hooks are dispatched only when
    /// compaction actually runs (threshold is exceeded), not on every step.
    async fn maybe_compact(&mut self, step: usize) -> Result<()> {
        let compactor = match &self.compactor {
            Some(c) => c,
            None => return Ok(()),
        };

        let chars = Compactor::estimate_chars(&self.messages);
        if chars < compactor.threshold_chars {
            return Ok(());
        }

        // Only dispatch PreCompact when we're actually about to compact.
        self.hooks.dispatch(HookEvent::PreCompact {
            transcript_len: chars,
        });

        let kept_before = self.messages.len();
        if let Some((removed, summary_chars)) = compactor
            .apply_to_transcript(self.llm.as_ref(), &mut self.messages, step)
            .await?
        {
            let kept = kept_before - removed;
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
        }

        Ok(())
    }

    /// Execute a set of tool calls, returning `(id, name, output, args)` for each.
    /// Read-only calls are batched and executed in parallel; write calls run
    /// sequentially to preserve ordering guarantees.
    async fn execute_tool_calls(&self, calls: &[ToolCall]) -> Vec<ToolCallOutcome> {
        let mut results: Vec<ToolCallOutcome> = Vec::new();

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
                && call.name != EXIT_PLAN_MODE_TOOL_NAME
            {
                results.push(ToolCallOutcome {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    result: format!(
                        "ERROR: Cannot execute '{}' in plan mode. \
                         You are in read-only planning mode. \
                         Explore freely with read tools, then call \
                         exit_plan_mode with your plan.",
                        call.name
                    ),
                    audit: None,
                });
                continue;
            }

            // Goal-190: if a tool is in Plan mode and we're not in plan mode,
            // tell the agent to enter plan mode first.
            if !self.exploring_plan_mode.load(Ordering::Relaxed)
                && self.tools.is_plan_mode(&call.name)
                && call.name != ENTER_PLAN_MODE_TOOL_NAME
                && call.name != EXIT_PLAN_MODE_TOOL_NAME
            {
                results.push(ToolCallOutcome {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    result: format!(
                        "ERROR: Tool '{}' requires plan mode. \
                         Call enter_plan_mode first to explore and plan, \
                         then call exit_plan_mode with your plan before using this tool.",
                        call.name
                    ),
                    audit: None,
                });
                continue;
            }

            let effective_args = if let Some(ref hook) = self.permission_hook {
                match hook.check(&call.name, &call.arguments).await {
                    PermissionDecision::Allow => call.arguments.clone(),
                    PermissionDecision::Deny(reason) => {
                        let result = format!("ERROR: {reason}");
                        results.push(ToolCallOutcome {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            result,
                            audit: None,
                        });
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
                    results.push(ToolCallOutcome {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        result,
                        audit: None,
                    });
                    continue;
                }
                HookAction::Error(msg) => {
                    let result = format!("ERROR: {msg}");
                    results.push(ToolCallOutcome {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        result,
                        audit: None,
                    });
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
                    let tools = Arc::clone(&self.tools);
                    join_set.spawn(async move {
                        let tool_start = std::time::Instant::now();
                        let dispatch = tools.invoke_with_audit(&name, args.clone()).await;
                        let result = match dispatch.result {
                            Ok(output) => output,
                            Err(crate::error::Error::PermissionDeniedLimit { .. }) => {
                                DENIAL_LIMIT_SENTINEL.to_string()
                            }
                            Err(err) => format!("ERROR: {err}"),
                        };
                        let duration_ms = tool_start.elapsed().as_millis() as u64;
                        (id, name, result, dispatch.audit, duration_ms)
                    });
                }

                type BatchRow = (String, String, String, crate::tools::AuditMeta, u64);
                // Use a HashMap keyed by tool_call_id so that O(1) lookup
                // preserves the correct audit metadata when multiple parallel
                // calls complete in arbitrary order (linear find would always
                // hit the first matching id, causing silent audit mis-attribution).
                let mut batch_map: std::collections::HashMap<String, BatchRow> =
                    std::collections::HashMap::new();
                while let Some(res) = join_set.join_next().await {
                    match res {
                        Ok(row) => {
                            batch_map.insert(row.0.clone(), row);
                        }
                        Err(e) => {
                            tracing::error!(target: "recursive::agent", "parallel tool task panicked: {e}");
                        }
                    }
                }

                for pc in &batch {
                    let Some((_, _, result, audit, duration_ms)) = batch_map.get(&pc.id) else {
                        // Task panicked — push a placeholder error result so
                        // the tool-call ↔ tool-result pairing invariant (#8)
                        // is preserved. Without this, the next LLM request
                        // would include an orphaned tool_call with no matching
                        // tool result and be rejected with HTTP 400.
                        results.push(ToolCallOutcome {
                            id: pc.id.clone(),
                            name: pc.name.clone(),
                            result: "ERROR: tool task panicked during parallel execution"
                                .to_string(),
                            audit: None,
                        });
                        continue;
                    };
                    results.push(ToolCallOutcome {
                        id: pc.id.clone(),
                        name: pc.name.clone(),
                        result: result.clone(),
                        audit: Some(audit.clone()),
                    });
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
                    Err(crate::error::Error::PermissionDeniedLimit { .. }) => {
                        DENIAL_LIMIT_SENTINEL.to_string()
                    }
                    Err(err) => format!("ERROR: {err}"),
                };
                let duration_ms = tool_start.elapsed().as_millis() as u64;
                results.push(ToolCallOutcome {
                    id: pc.id.clone(),
                    name: pc.name.clone(),
                    result: result.clone(),
                    audit: Some(dispatch.audit),
                });
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
        let mut recent_errors: std::collections::VecDeque<(bool, String)> =
            std::collections::VecDeque::with_capacity(self.stuck_window);
        let mut total_usage = TokenUsage::default();
        // Goal-153: audit metadata for tool calls, keyed by (turn, tool_call_id).
        let mut tool_audits: std::collections::HashMap<
            crate::tools::AuditKey,
            crate::tools::AuditMeta,
        > = std::collections::HashMap::new();
        self.total_llm_latency_ms = 0;

        let step_cap = effective_step_limit(self.max_steps);
        for step in 1..=step_cap {
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
                        is_compaction_summary: false,
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
                        tool_audits,
                    });
                }
            }

            // ---- compaction -------------------------------------------------------
            self.maybe_compact(step).await?;

            // ---- LLM call (with retry) --------------------------------------------
            debug!(target: "recursive::agent", step, "calling llm");
            let start = std::time::Instant::now();
            let stream_tx: Option<StreamSender> = if self.streaming {
                let (delta_tx, mut delta_rx) = mpsc::unbounded_channel::<String>();
                let events_tx = self.events.clone();
                tokio::spawn(async move {
                    while let Some(text) = delta_rx.recv().await {
                        if let Some(ref tx) = events_tx {
                            let _ = tx.send(AgentEvent::PartialToken { text, step });
                        }
                    }
                });
                Some(delta_tx)
            } else {
                None
            };
            let completion: Completion = self.call_llm_with_retry(&specs, stream_tx, step).await?;
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

            // Surface reasoning / thinking content to UI consumers
            // (TUI) as a separate event so it can be rendered as a
            // `thinking…` block. Providers that stream reasoning
            // tokens accumulate them into the final string before
            // this point; we emit exactly once per step.
            //
            // Emit this BEFORE AssistantText so the TUI's
            // `Reasoning { text }` block lands above the matching
            // `Assistant { text }` block in the transcript — the
            // model thinks first, then speaks, and the visual order
            // should match.
            if let Some(reasoning) = &completion.reasoning_content {
                if !reasoning.is_empty() {
                    self.emit(AgentEvent::Reasoning {
                        text: reasoning.clone(),
                        step,
                    });
                }
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

            // ---- tool execution ---------------------------------------------------
            let results = self.execute_tool_calls(&completion.tool_calls).await;

            // ── Goal-285: sentinel pre-pass — detect before any push_message calls ──────
            // The old code checked for DENIAL_LIMIT_SENTINEL inside the outer loop.
            // When sentinel appeared at index N > 0, results[0..N] had already been
            // pushed by the outer loop's push_message; the nested inner loop then
            // pushed ALL results again, duplicating results[0..N] in the transcript.
            //
            // Fix: scan first, flush once atomically, return immediately.
            if results.iter().any(|o| o.result == DENIAL_LIMIT_SENTINEL) {
                for o in &results {
                    if let Some(a) = &o.audit {
                        tool_audits.insert((self.turn, o.id.clone()), a.clone());
                    }
                    let is_error =
                        o.result.starts_with("ERROR: ") || o.result == DENIAL_LIMIT_SENTINEL;
                    self.emit(AgentEvent::ToolResult {
                        id: o.id.clone(),
                        name: o.name.clone(),
                        output: o.result.clone(),
                        step,
                        is_error,
                    });
                    self.push_message(Message::tool_result(o.id.clone(), o.result.clone()));
                }
                let finish = FinishReason::PermissionDenialLimit;
                self.emit(AgentEvent::TurnFinished {
                    reason: finish_reason_str(&finish),
                    steps: step,
                });
                return Ok(RunInnerOutcome {
                    messages: self.messages,
                    final_message,
                    finish_reason: finish,
                    total_usage,
                    total_llm_latency_ms: self.total_llm_latency_ms,
                    steps: step,
                    tool_audits,
                });
            }

            for ToolCallOutcome {
                id,
                name,
                result,
                audit,
            } in &results
            {
                let is_error = result.starts_with("ERROR: ");
                self.emit(AgentEvent::ToolResult {
                    id: id.clone(),
                    name: name.clone(),
                    output: result.clone(),
                    step,
                    is_error,
                });
                // Goal-153: accumulate audit keyed by (turn, tool_call_id).
                if let Some(a) = audit {
                    tool_audits.insert((self.turn, id.clone()), a.clone());
                }

                // Sliding-window stuck detection: track whether each tool call
                // was an error. Triggers when the error rate in the last
                // stuck_window steps exceeds stuck_error_rate, catching loops
                // that cycle across different tools (e.g. A→B→A→B).
                if recent_errors.len() == self.stuck_window {
                    recent_errors.pop_front();
                }
                recent_errors.push_back((is_error, name.clone()));

                if recent_errors.len() == self.stuck_window {
                    let error_entries: Vec<&str> = recent_errors
                        .iter()
                        .filter(|(is_err, _)| *is_err)
                        .map(|(_, n)| n.as_str())
                        .collect();
                    let error_count = error_entries.len();
                    let rate = error_count as f64 / self.stuck_window as f64;
                    if rate >= self.stuck_error_rate {
                        // Find the most frequently appearing tool name in the error window.
                        let mut counts: std::collections::HashMap<&str, usize> =
                            std::collections::HashMap::new();
                        for n in &error_entries {
                            *counts.entry(n).or_default() += 1;
                        }
                        let top_tool = counts
                            .into_iter()
                            .max_by_key(|&(_, c)| c)
                            .map(|(n, _)| n)
                            .unwrap_or(name.as_str());
                        let finish = FinishReason::Stuck {
                            repeated_call: top_tool.to_string(),
                            repeats: error_count,
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
                            tool_audits,
                        };
                        return Ok(outcome);
                    }
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
            tool_audits,
        };
        Ok(outcome)
    }
}

/// Step cap for the agent loop. `0` means unlimited (no `BudgetExceeded`).
fn effective_step_limit(max_steps: usize) -> usize {
    if max_steps == 0 {
        usize::MAX
    } else {
        max_steps
    }
}

#[cfg(test)]
mod tests {
    use super::effective_step_limit;

    #[test]
    fn effective_step_limit_zero_means_unbounded() {
        assert_eq!(effective_step_limit(0), usize::MAX);
        assert_eq!(effective_step_limit(32), 32);
    }

    /// Verify the stuck-detection window/rate math that RunCore uses.
    /// This tests the same logic as the sliding-window check in run_inner().
    #[test]
    fn stuck_detection_window_and_rate() {
        // Simulate: stuck_window=3, stuck_error_rate=1.0 → 3 consecutive errors triggers stuck
        let stuck_window = 3usize;
        let stuck_error_rate = 1.0f64;

        let mut recent_errors: std::collections::VecDeque<(bool, String)> =
            std::collections::VecDeque::with_capacity(stuck_window);

        // Push 2 errors — should not trigger yet
        for i in 0..2 {
            if recent_errors.len() == stuck_window {
                recent_errors.pop_front();
            }
            let name = format!("tool_{i}");
            recent_errors.push_back((true, name));
            let rate = recent_errors.iter().filter(|(e, _)| *e).count() as f64
                / recent_errors.len() as f64;
            assert!(
                recent_errors.len() < stuck_window || rate < stuck_error_rate,
                "should not trigger before window is full"
            );
        }

        // Push 3rd error — window is now full and rate = 1.0 ≥ threshold
        if recent_errors.len() == stuck_window {
            recent_errors.pop_front();
        }
        recent_errors.push_back((true, "tool_2".to_string()));
        assert_eq!(recent_errors.len(), stuck_window);
        let error_count = recent_errors.iter().filter(|(e, _)| *e).count();
        let rate = error_count as f64 / stuck_window as f64;
        assert!(
            rate >= stuck_error_rate,
            "rate {rate} should trigger stuck at threshold {stuck_error_rate}"
        );

        // Verify most-repeated tool reporting logic
        let error_entries: Vec<&str> = recent_errors
            .iter()
            .filter(|(e, _)| *e)
            .map(|(_, n)| n.as_str())
            .collect();
        let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for n in &error_entries {
            *counts.entry(n).or_default() += 1;
        }
        let top_tool = counts.into_iter().max_by_key(|&(_, c)| c).map(|(n, _)| n);
        // All three are distinct so any one could be "top"; just verify it exists
        assert!(top_tool.is_some());
    }

    #[test]
    fn stuck_detection_partial_errors_below_threshold() {
        // window=4, rate=0.8 → need 4 errors out of 4 (or round up).
        // With 3 errors + 1 success the rate is 0.75 < 0.8, so no trigger.
        let stuck_window = 4usize;
        let stuck_error_rate = 0.8f64;

        let mut recent_errors: std::collections::VecDeque<(bool, String)> =
            std::collections::VecDeque::with_capacity(stuck_window);

        let pattern = [
            (true, "tool_a"),
            (true, "tool_b"),
            (true, "tool_a"),
            (false, "tool_c"),
        ]; // 3/4 errors = 0.75
        for (is_error, name) in &pattern {
            if recent_errors.len() == stuck_window {
                recent_errors.pop_front();
            }
            recent_errors.push_back((*is_error, name.to_string()));
        }

        assert_eq!(recent_errors.len(), stuck_window);
        let rate = recent_errors.iter().filter(|(e, _)| *e).count() as f64 / stuck_window as f64;
        assert!(
            rate < stuck_error_rate,
            "rate {rate} should be below threshold {stuck_error_rate}"
        );
    }

    #[test]
    fn stuck_detection_reports_most_repeated_tool() {
        // window=4, rate=0.75, pattern: tool_a err, tool_b err, tool_a err, tool_b ok
        // error_count=3, rate=0.75, most repeated should be tool_a (appears 2× in errors)
        let stuck_window = 4usize;
        let stuck_error_rate = 0.75f64;

        let mut recent_errors: std::collections::VecDeque<(bool, String)> =
            std::collections::VecDeque::with_capacity(stuck_window);

        let pattern = [
            (true, "tool_a"),
            (true, "tool_b"),
            (true, "tool_a"),
            (false, "tool_b"),
        ];
        for (is_error, name) in &pattern {
            if recent_errors.len() == stuck_window {
                recent_errors.pop_front();
            }
            recent_errors.push_back((*is_error, name.to_string()));
        }

        assert_eq!(recent_errors.len(), stuck_window);

        let error_entries: Vec<&str> = recent_errors
            .iter()
            .filter(|(is_err, _)| *is_err)
            .map(|(_, n)| n.as_str())
            .collect();
        let error_count = error_entries.len();
        let rate = error_count as f64 / stuck_window as f64;
        assert!(
            rate >= stuck_error_rate,
            "rate {rate} should trigger stuck at threshold {stuck_error_rate}"
        );

        // Find the most frequently appearing tool name in the error window.
        let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for n in &error_entries {
            *counts.entry(n).or_default() += 1;
        }
        let top_tool = counts
            .into_iter()
            .max_by_key(|&(_, c)| c)
            .map(|(n, _)| n)
            .unwrap();
        assert_eq!(
            top_tool, "tool_a",
            "most repeated tool should be tool_a (2 errors), not tool_b (1 error)"
        );
    }
}
