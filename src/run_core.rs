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

/// Sentinel returned in the tool-result string when the permission denial
/// limit is exceeded.  Matches the check in `run_inner` that breaks the
/// ReAct loop.  Defined as a constant to avoid scattered string literals.
pub(crate) const DENIAL_LIMIT_SENTINEL: &str = "ERROR_DENIAL_LIMIT:";

use crate::compact::Compactor;
use crate::error::Result;
use crate::hooks::{HookAction, HookEvent, HookRegistry};
use crate::llm::{ChatProvider, Completion, StreamChunk, StreamSender, TokenUsage, ToolCall};
use crate::message::Message;

use crate::tools::plan_mode::{ENTER_PLAN_MODE_TOOL_NAME, EXIT_PLAN_MODE_TOOL_NAME};
use crate::tools::ToolRegistry;

use crate::agent::{FinishReason, PermissionDecision};
use crate::event::AgentEvent;
use crate::skills::Skill;
use crate::skills_injector::SkillInjector;
use crate::tools::PermissionHook;

/// Placeholder text used when trimming old tool results to fit the transcript budget.
pub(crate) const TRIM_PLACEHOLDER: &str = "[older tool output trimmed to fit budget]";

/// Minimum tool-result size (bytes) worth trimming; shorter results are kept verbatim.
const MIN_TRIM_LENGTH: usize = 200;

/// Render a [`FinishReason`] as the `reason` field of [`AgentEvent::TurnFinished`].
///
/// Delegates to the `Display` implementation on `FinishReason` to ensure
/// consistency between event payloads and HTTP API responses.
fn finish_reason_str(reason: &FinishReason) -> String {
    reason.to_string()
}

/// Outcome returned by the stateless [`run_inner`] loop.
pub(crate) struct RunInnerOutcome {
    pub(crate) messages: Arc<Vec<Message>>,
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
    pub(crate) messages: Arc<Vec<Message>>,
    pub(crate) llm: Arc<dyn ChatProvider>,
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
    /// Goal-318: `Globs`-mode skills for path-triggered injection.
    /// Injected as system messages after tool calls match a skill's glob patterns.
    pub(crate) globs_skills: Vec<Skill>,
}

impl<'a> RunCore<'a> {
    fn emit(&self, event: AgentEvent) {
        if let Some(ref tx) = self.events {
            let _ = tx.send(event);
        }
    }

    fn push_message(&mut self, msg: Message) {
        Arc::make_mut(&mut self.messages).push(msg);
    }

    /// Attach `reasoning_content` to the last message in the transcript, if present.
    ///
    /// Called immediately after `push_message` on any path that produces reasoning
    /// tokens (both the no-tool-calls path and the tool-calls path share this logic).
    fn attach_reasoning_content(&mut self, reasoning: Option<String>) {
        if reasoning.is_some() {
            if let Some(msg) = Arc::make_mut(&mut self.messages).last_mut() {
                msg.reasoning_content = reasoning;
            }
        }
    }

    /// Process the result of a tool batch: emit per-call `ToolResult`
    /// events, push paired tool_result messages, run sliding-window
    /// stuck detection, then optionally terminate the run.
    ///
    /// Returns `Some(finish)` when the run should end after this batch
    /// (sentinel pre-pass hits `PermissionDenialLimit`, or stuck
    /// detection triggers `Stuck`); the caller routes the finish through
    /// [`make_outcome`]. Returns `None` when the loop should continue.
    ///
    /// **Invariant #8**: every tool_call in the batch is paired with a
    /// tool_result message before any early return. The sentinel pre-pass
    /// scans the whole batch first and flushes all tool_results
    /// atomically; the stuck-detection loop likewise pushes each
    /// tool_result before recording the (deferred) finish verdict. We
    /// never mid-batch return.
    ///
    /// When returning `None`, also runs the Globs-mode skill injector
    /// against the result strings so the next step can pick up
    /// path-triggered skills.
    #[allow(clippy::too_many_arguments)]
    fn process_tool_results(
        &mut self,
        results: &[ToolCallOutcome],
        step: usize,
        recent_errors: &mut std::collections::VecDeque<(bool, String)>,
        tool_audits: &mut std::collections::HashMap<
            crate::tools::AuditKey,
            crate::tools::AuditMeta,
        >,
        skill_injector: &mut SkillInjector,
    ) -> Option<FinishReason> {
        // Goal-285: sentinel pre-pass — detect before any push_message calls.
        // When the sentinel appears at index N > 0, flushing incrementally
        // would duplicate results[0..N] in the transcript; scan first, flush
        // once atomically, return immediately.
        if results.iter().any(|o| o.result == DENIAL_LIMIT_SENTINEL) {
            for o in results {
                if let Some(a) = &o.audit {
                    tool_audits.insert((self.turn, o.id.clone()), a.clone());
                }
                let is_error = o.result.starts_with("ERROR: ") || o.result == DENIAL_LIMIT_SENTINEL;
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
            return Some(finish);
        }

        // Stuck detection may fire while iterating the results of a
        // multi-call step. Returning mid-loop (as this code used to do)
        // left the assistant message's remaining tool_calls without
        // matching tool_result messages — orphaned `tool_use` blocks
        // that the provider rejects on the *next* turn with HTTP 400.
        // Record the stuck verdict here but defer the actual return
        // until the loop has pushed a tool_result for every call.
        let mut stuck_finish: Option<FinishReason> = None;
        for ToolCallOutcome {
            id,
            name,
            result,
            audit,
        } in results
        {
            let is_error = result.starts_with("ERROR: ");
            self.emit(AgentEvent::ToolResult {
                id: id.clone(),
                name: name.clone(),
                output: result.clone(),
                step,
                is_error,
            });
            if let Some(a) = audit {
                tool_audits.insert((self.turn, id.clone()), a.clone());
            }
            self.push_message(Message::tool_result(id.clone(), result.clone()));

            // Sliding-window stuck detection: track whether each tool call
            // was an error. Triggers when the error rate in the last
            // stuck_window steps exceeds stuck_error_rate, catching loops
            // that cycle across different tools (e.g. A→B→A→B).
            if recent_errors.len() == self.stuck_window {
                recent_errors.pop_front();
            }
            recent_errors.push_back((is_error, name.clone()));

            if stuck_finish.is_none() && recent_errors.len() == self.stuck_window {
                let error_entries: Vec<&str> = recent_errors
                    .iter()
                    .filter(|(is_err, _)| *is_err)
                    .map(|(_, n)| n.as_str())
                    .collect();
                let error_count = error_entries.len();
                let rate = error_count as f64 / self.stuck_window as f64;
                if rate >= self.stuck_error_rate {
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
                    stuck_finish = Some(FinishReason::Stuck {
                        repeated_call: top_tool.to_string(),
                        repeats: error_count,
                    });
                }
            }
        }

        // Deferred stuck termination: every tool_call now has its matching
        // tool_result in the transcript, so ending the turn here cannot
        // leave orphaned `tool_use` blocks behind.
        if let Some(finish) = stuck_finish {
            self.emit(AgentEvent::TurnFinished {
                reason: finish_reason_str(&finish),
                steps: step,
            });
            return Some(finish);
        }

        // Goal-318: after every tool-result batch, check whether any result
        // references a path matching a Globs-mode skill. Inject once per skill.
        let result_strings: Vec<String> = results.iter().map(|r| r.result.clone()).collect();
        for (skill_name, skill_body) in skill_injector.check(&result_strings) {
            self.push_message(Message::system(format!(
                "<!-- skill:{skill_name} injected by globs match -->\n{skill_body}"
            )));
        }
        None
    }

    /// Finalise a step whose LLM completion carried no tool calls. Pushes
    /// the assistant message + reasoning onto the transcript, classifies
    /// the finish reason (`ProviderStop` for non-`stop`/`end_turn`
    /// provider reasons, else `NoMoreToolCalls`), emits `TurnFinished`,
    /// and returns `Some(finish)` so the caller routes through
    /// [`make_outcome`]. Returns `None` when the caller should continue
    /// with tool dispatch.
    fn handle_no_tool_calls(
        &mut self,
        completion: &Completion,
        step: usize,
    ) -> Option<FinishReason> {
        if !completion.tool_calls.is_empty() {
            return None;
        }
        self.push_message(Message::assistant(completion.content.clone()));
        self.attach_reasoning_content(completion.reasoning_content.clone());
        let finish = match completion.finish_reason.as_deref() {
            Some(r) if r != "stop" && r != "end_turn" => FinishReason::ProviderStop(r.to_string()),
            _ => FinishReason::NoMoreToolCalls,
        };
        self.emit(AgentEvent::TurnFinished {
            reason: finish_reason_str(&finish),
            steps: step,
        });
        Some(finish)
    }

    /// Drain any messages that a coordinator pushed via `send_message`
    /// since the last step, appending each as a user-role turn so the
    /// LLM sees coordinator instructions on the next reasoning step.
    /// No-op when no mailbox is configured.
    async fn drain_mailbox(&mut self) {
        let Some(mailbox) = self.mailbox.as_ref() else {
            return;
        };
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

    /// Apply transcript-budget enforcement for this step. When configured
    /// (`max_transcript_chars`) the transcript is trimmed and re-measured;
    /// if it still exceeds the limit, returns `Some((finish, step))` for
    /// the caller to route through [`make_outcome`].
    fn enforce_transcript_budget(
        &mut self,
        step: usize,
        total_usage: &TokenUsage,
    ) -> Option<(FinishReason, usize)> {
        let limit = self.max_transcript_chars?;
        self.maybe_trim_transcript(limit, step);
        let chars: usize = self.messages.iter().map(|m| m.content.len()).sum();
        if chars < limit {
            return None;
        }
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
        Some((finish, step))
    }

    /// Check the shutdown token at the top of a step. Returns `Some((
    /// finish, finished_steps))` when the run should terminate with
    /// [`FinishReason::Cancelled`]; the caller routes the pair through
    /// [`make_outcome`]. `finished_steps` is `step - 1` because the
    /// current step's LLM call has not started yet.
    ///
    /// `total_usage` is read for the structured log line only — the
    /// outcome assembly happens at the call site.
    fn check_shutdown(
        &self,
        step: usize,
        total_usage: &TokenUsage,
    ) -> Option<(FinishReason, usize)> {
        let token = self.shutdown_token.as_ref()?;
        if !token.is_cancelled() {
            return None;
        }
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
        Some((finish, finished_steps))
    }

    /// Assemble the final outcome for a run that terminates with `finish`
    /// at step `steps`. All early-return paths in [`run_inner`] route here
    /// so the seven-field struct cannot drift out of sync across sites.
    fn make_outcome(
        self,
        finish: FinishReason,
        steps: usize,
        final_message: Option<String>,
        total_usage: TokenUsage,
        tool_audits: std::collections::HashMap<crate::tools::AuditKey, crate::tools::AuditMeta>,
    ) -> RunInnerOutcome {
        RunInnerOutcome {
            messages: self.messages,
            final_message,
            finish_reason: finish,
            total_usage,
            total_llm_latency_ms: self.total_llm_latency_ms,
            steps,
            tool_audits,
        }
    }

    /// Call the LLM once, delegating retry handling to the provider's
    /// internal `RetryPolicy`. Goal-288 removed the outer retry loop so
    /// there is exactly one retry layer.
    ///
    /// When the registry has deferred tools, their names are injected as an
    /// `<available-deferred-tools>` user message prepended to the transcript,
    /// and only eager tool specs are passed to the provider. ToolSearchTool
    /// (registered by `freeze_deferred_specs`) appears in the eager list and
    /// its results in the message history are serialized as `tool_reference`
    /// blocks by `serialize_messages_anthropic`.
    async fn call_llm(
        &self,
        specs: &[crate::llm::ToolSpec],
        stream_sender: Option<crate::llm::StreamSender>,
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

        if let Some(ref tx) = stream_sender {
            self.llm
                .stream(messages, call_specs, Some(tx.clone()))
                .await
        } else {
            self.llm.complete(messages, call_specs).await
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

        for msg in Arc::make_mut(&mut self.messages).iter_mut().skip(1) {
            if msg.role == crate::message::Role::Tool && msg.content.len() > MIN_TRIM_LENGTH {
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
            .apply_to_transcript(self.llm.as_ref(), Arc::make_mut(&mut self.messages), step)
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
                        let dispatch = tools.invoke_with_audit(&name, args).await;
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
                    std::collections::HashMap::with_capacity(batch.len());
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
        // Goal-318: one injector per run; tracks already-fired globs skills.
        let mut skill_injector = SkillInjector::new(&self.globs_skills);
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
            // FinishReason::Cancelled.
            if let Some((finish, finished_steps)) = self.check_shutdown(step, &total_usage) {
                return Ok(self.make_outcome(
                    finish,
                    finished_steps,
                    final_message,
                    total_usage,
                    tool_audits,
                ));
            }

            // ---- mailbox drain (coordinator → worker mid-run messages) -----------
            self.drain_mailbox().await;

            // ---- transcript budget ------------------------------------------------
            if let Some((finish, finish_step)) = self.enforce_transcript_budget(step, &total_usage)
            {
                return Ok(self.make_outcome(
                    finish,
                    finish_step,
                    final_message,
                    total_usage,
                    tool_audits,
                ));
            }

            // ---- compaction -------------------------------------------------------
            self.maybe_compact(step).await?;

            // ---- LLM call (with retry) --------------------------------------------
            debug!(target: "recursive::agent", step, "calling llm");
            let start = std::time::Instant::now();
            let mut forward_handle = None;
            let stream_tx: Option<StreamSender> = if self.streaming {
                let (delta_tx, mut delta_rx) = mpsc::unbounded_channel::<StreamChunk>();
                let events_tx = self.events.clone();
                forward_handle = Some(tokio::spawn(async move {
                    while let Some(chunk) = delta_rx.recv().await {
                        if let Some(ref tx) = events_tx {
                            let event = match chunk {
                                StreamChunk::Text(text) => AgentEvent::PartialToken { text, step },
                                StreamChunk::Reasoning(text) => {
                                    AgentEvent::PartialReasoning { text, step }
                                }
                            };
                            let _ = tx.send(event);
                        }
                    }
                }));
                Some(delta_tx)
            } else {
                None
            };
            let mut completion: Completion = self.call_llm(&specs, stream_tx).await?;
            // Drain the partial-token forwarder before emitting any further
            // events. `call_llm` drops `stream_tx` on return, which closes
            // `delta_rx` and lets the spawned task finish; awaiting it
            // guarantees every `PartialToken` has been pushed to the event
            // sink *before* the finalising `AssistantText`. Without this the
            // two tasks race on the shared sink and a late token can arrive
            // after the assistant block was finalised, spawning a duplicate
            // streaming block in the UI.
            if let Some(handle) = forward_handle.take() {
                let _ = handle.await;
            }
            // Normalise chain-of-thought that the model emitted inline as
            // `<think>…</think>` in `content` (common for OpenAI-compatible
            // DeepSeek-R1 deployments that don't use the dedicated
            // `reasoning_content` SSE field) into the reasoning channel, so
            // the thinking block renders instead of being dropped by the
            // markdown HTML-block parser.
            completion.extract_inline_reasoning();
            let llm_ms = start.elapsed().as_millis() as u64;
            self.total_llm_latency_ms = self.total_llm_latency_ms.saturating_add(llm_ms);
            self.emit(AgentEvent::Latency { step, llm_ms });

            if let Some(u) = completion.usage {
                total_usage = total_usage.accumulate(u);
                self.emit(AgentEvent::Usage {
                    input_tokens: u.prompt_tokens,
                    output_tokens: u.completion_tokens,
                    cache_hit_tokens: u.cache_hit_tokens,
                    cache_miss_tokens: u.cache_miss_tokens,
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
            if let Some(finish) = self.handle_no_tool_calls(&completion, step) {
                return Ok(self.make_outcome(
                    finish,
                    step,
                    final_message,
                    total_usage,
                    tool_audits,
                ));
            }

            self.push_message(Message::assistant_with_tool_calls(
                completion.content.clone(),
                completion.tool_calls.clone(),
            ));
            self.attach_reasoning_content(completion.reasoning_content.clone());

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

            if let Some(finish) = self.process_tool_results(
                &results,
                step,
                &mut recent_errors,
                &mut tool_audits,
                &mut skill_injector,
            ) {
                return Ok(self.make_outcome(
                    finish,
                    step,
                    final_message,
                    total_usage,
                    tool_audits,
                ));
            }
        }

        warn!(target: "recursive::agent", "step budget exceeded");
        let finish = FinishReason::BudgetExceeded;
        let steps = self.max_steps;
        self.emit(AgentEvent::TurnFinished {
            reason: finish_reason_str(&finish),
            steps,
        });
        Ok(self.make_outcome(finish, steps, final_message, total_usage, tool_audits))
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
    use super::{effective_step_limit, RunCore};
    use crate::message::Message;

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

    // Helper: build a minimal RunCore suitable for testing methods that only
    // touch `messages` (e.g. `attach_reasoning_content`).
    fn make_test_core<'a>(
        messages: Vec<Message>,
        hooks: &'a crate::hooks::HookRegistry,
    ) -> RunCore<'a> {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        RunCore {
            messages: Arc::new(messages),
            llm: Arc::new(crate::llm::MockProvider::new(vec![])),
            tools: Arc::new(crate::tools::ToolRegistry::default()),
            max_steps: 1,
            max_transcript_chars: None,
            events: None,
            streaming: false,
            compactor: None,
            permission_hook: None,
            hooks,
            total_llm_latency_ms: 0,
            exploring_plan_mode: Arc::new(AtomicBool::new(false)),
            shutdown_token: None,
            mailbox: None,
            stuck_window: 3,
            stuck_error_rate: 1.0,
            turn: 0,
            globs_skills: vec![],
        }
    }

    #[test]
    fn attach_reasoning_content_sets_last_message_when_some() {
        let hooks = crate::hooks::HookRegistry::new();
        let mut core = make_test_core(vec![Message::assistant("response".to_string())], &hooks);

        core.attach_reasoning_content(Some("I thought carefully about this.".to_string()));

        assert_eq!(
            core.messages.last().unwrap().reasoning_content.as_deref(),
            Some("I thought carefully about this."),
            "reasoning_content should be set on the last message"
        );
    }

    #[test]
    fn attach_reasoning_content_does_not_modify_when_none() {
        let hooks = crate::hooks::HookRegistry::new();
        let mut core = make_test_core(vec![Message::assistant("response".to_string())], &hooks);

        core.attach_reasoning_content(None);

        assert!(
            core.messages.last().unwrap().reasoning_content.is_none(),
            "reasoning_content should remain None when called with None"
        );
    }

    #[test]
    fn attach_reasoning_content_preserves_existing_content_when_none() {
        let hooks = crate::hooks::HookRegistry::new();
        let mut msg = Message::assistant("response".to_string());
        msg.reasoning_content = Some("prior thinking".to_string());
        let mut core = make_test_core(vec![msg], &hooks);

        core.attach_reasoning_content(None);

        assert_eq!(
            core.messages.last().unwrap().reasoning_content.as_deref(),
            Some("prior thinking"),
            "calling with None should not overwrite existing reasoning_content"
        );
    }
}
