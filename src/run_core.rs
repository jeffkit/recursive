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

    /// Execute one LLM call for this step and surface everything the
    /// loop body needs: stream-forwarding task, latency tracking,
    /// token-usage accumulation, and the ordered `Reasoning` /
    /// `AssistantText` events.
    ///
    /// Returns the raw [`Completion`] plus `Some(text)` when the step
    /// produced non-empty assistant content (which becomes the
    /// candidate `final_message`). The caller decides what to do with
    /// the completion (no-tool-calls path vs. tool-dispatch path) and
    /// owns pushing assistant messages onto the transcript —
    /// `dispatch_llm_step` deliberately does not mutate `self.messages`.
    async fn dispatch_llm_step(
        &mut self,
        specs: &[crate::llm::ToolSpec],
        step: usize,
        total_usage: &mut TokenUsage,
    ) -> crate::error::Result<(Completion, Option<String>)> {
        debug!(target: "recursive::agent", step, "calling llm");
        let start = std::time::Instant::now();
        // Spin up the partial-token forwarder when streaming so the
        // UI gets live `PartialToken` / `PartialReasoning` events.
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
        let mut completion: Completion = self.call_llm(specs, stream_tx).await?;
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
            *total_usage = total_usage.accumulate(u);
            self.emit(AgentEvent::Usage {
                input_tokens: u.prompt_tokens,
                output_tokens: u.completion_tokens,
                cache_hit_tokens: u.cache_hit_tokens,
                cache_miss_tokens: u.cache_miss_tokens,
                step,
            });
        }

        // Surface reasoning / thinking content to UI consumers (TUI) as
        // a separate event so it can be rendered as a `thinking…` block.
        // Providers that stream reasoning tokens accumulate them into
        // the final string before this point; we emit exactly once per
        // step. Emit this BEFORE AssistantText so the TUI's
        // `Reasoning { text }` block lands above the matching
        // `Assistant { text }` block — the model thinks first, then
        // speaks, and the visual order should match.
        if let Some(reasoning) = &completion.reasoning_content {
            if !reasoning.is_empty() {
                self.emit(AgentEvent::Reasoning {
                    text: reasoning.clone(),
                    step,
                });
            }
        }

        let new_final_message = if !completion.content.is_empty() {
            self.emit(AgentEvent::AssistantText {
                text: completion.content.clone(),
                step,
            });
            Some(completion.content.clone())
        } else {
            None
        };

        Ok((completion, new_final_message))
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
                        *counts.entry(n).or_default() += 1; // cargo-mutants::skip — +=→*= near-equivalent with one repeated tool
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
            let cancel_token = self.shutdown_token.clone();
            self.llm
                .stream(messages, call_specs, Some(tx.clone()), cancel_token)
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
            let (completion, new_final_message) = self
                .dispatch_llm_step(&specs, step, &mut total_usage)
                .await?;
            if let Some(text) = new_final_message {
                final_message = Some(text);
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
///
/// If the `RECURSIVE_HARD_STEP_CAP` env var is set to a positive integer,
/// the cap is clamped to that value regardless of `max_steps`. This lets
/// operators enforce a production ceiling on `recursive loop` long-running
/// sessions (which default to `max_steps=0` / unbounded) without changing
/// the per-session contract. When unset or zero, behaviour is unchanged:
/// `max_steps=0` still means `usize::MAX`.
fn effective_step_limit(max_steps: usize) -> usize {
    let requested = if max_steps == 0 {
        usize::MAX
    } else {
        max_steps
    };
    match hard_step_cap_from_env() {
        Some(cap) if cap > 0 => requested.min(cap),
        _ => requested,
    }
}

/// Read the `RECURSIVE_HARD_STEP_CAP` env var once per call. Returns
/// `None` when unset or unparseable. A value of `0` is treated as
/// "unset" (matches the `max_steps=0` unlimited convention).
fn hard_step_cap_from_env() -> Option<usize> {
    std::env::var("RECURSIVE_HARD_STEP_CAP")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{
        effective_step_limit, finish_reason_str, RunCore, MIN_TRIM_LENGTH, TRIM_PLACEHOLDER,
    };
    use crate::message::Message;

    #[test]
    fn effective_step_limit_zero_means_unbounded() {
        // Env var must be unset for this test — its presence would clamp.
        let orig = std::env::var("RECURSIVE_HARD_STEP_CAP").ok();
        std::env::remove_var("RECURSIVE_HARD_STEP_CAP");
        assert_eq!(effective_step_limit(0), usize::MAX);
        assert_eq!(effective_step_limit(32), 32);
        if let Some(v) = orig {
            std::env::set_var("RECURSIVE_HARD_STEP_CAP", v);
        }
    }

    #[test]
    fn effective_step_limit_respects_hard_cap_when_set() {
        let orig = std::env::var("RECURSIVE_HARD_STEP_CAP").ok();
        std::env::set_var("RECURSIVE_HARD_STEP_CAP", "1000");

        // Unlimited path is clamped to the cap.
        assert_eq!(effective_step_limit(0), 1000);
        // Limited path below the cap stays as-is.
        assert_eq!(effective_step_limit(32), 32);
        // Limited path above the cap is clamped.
        assert_eq!(effective_step_limit(5000), 1000);

        if let Some(v) = orig {
            std::env::set_var("RECURSIVE_HARD_STEP_CAP", v);
        } else {
            std::env::remove_var("RECURSIVE_HARD_STEP_CAP");
        }
    }

    #[test]
    fn effective_step_limit_ignores_invalid_hard_cap() {
        let orig = std::env::var("RECURSIVE_HARD_STEP_CAP").ok();
        std::env::set_var("RECURSIVE_HARD_STEP_CAP", "not-a-number");
        assert_eq!(effective_step_limit(0), usize::MAX);
        std::env::set_var("RECURSIVE_HARD_STEP_CAP", "0");
        // Cap value of 0 is treated as "unset" → unlimited preserved.
        assert_eq!(effective_step_limit(0), usize::MAX);
        if let Some(v) = orig {
            std::env::set_var("RECURSIVE_HARD_STEP_CAP", v);
        } else {
            std::env::remove_var("RECURSIVE_HARD_STEP_CAP");
        }
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

    // ========================================================================
    // maybe_trim_transcript tests
    // ========================================================================

    // ========================================================================
    // finish_reason_str tests
    // (kills String::new() and "xyzzy" function-level mutants)
    // ========================================================================

    #[test]
    fn finish_reason_str_is_nonempty_and_not_xyzzy() {
        use crate::agent::FinishReason;
        let reason = FinishReason::NoMoreToolCalls;
        let s = finish_reason_str(&reason);
        assert!(
            !s.is_empty(),
            "finish_reason_str must not return empty string"
        );
        assert_ne!(s, "xyzzy", "finish_reason_str must not return 'xyzzy'");
    }

    #[test]
    fn finish_reason_str_matches_display() {
        use crate::agent::FinishReason;
        let reason = FinishReason::BudgetExceeded;
        let s = finish_reason_str(&reason);
        assert_eq!(
            s,
            reason.to_string(),
            "finish_reason_str must delegate to Display"
        );
    }

    // ========================================================================
    // maybe_trim_transcript tests
    // ========================================================================

    #[test]
    fn maybe_trim_does_nothing_when_under_limit() {
        let hooks = crate::hooks::HookRegistry::new();
        // Use a large tool result (> MIN_TRIM_LENGTH) but keep limit ABOVE total chars.
        // This kills the `replace < with ==` mutant: with `==`, chars != limit means
        // the early-return is skipped and the tool result would be incorrectly trimmed.
        let large_output = "x".repeat(MIN_TRIM_LENGTH + 10);
        let messages = vec![
            Message::system("sys".to_string()),
            Message::tool_result("c1", &large_output),
        ];
        let orig_content = messages[1].content.clone();
        let total_chars = "sys".len() + large_output.len();
        let mut core = make_test_core(messages, &hooks);

        // Set limit strictly ABOVE total chars (not equal), so trimming must not fire.
        let limit = total_chars + 1;
        core.maybe_trim_transcript(limit, 0);

        assert_eq!(
            core.messages[1].content, orig_content,
            "content must not be touched when total chars < limit"
        );
    }

    #[test]
    fn enforce_transcript_budget_returns_none_when_under_limit() {
        // kills `replace < with == in enforce_transcript_budget` mutation.
        // If changed to `==`, a core with chars clearly under the limit would
        // return Some (falsely claiming budget exceeded) instead of None.
        use crate::llm::TokenUsage;
        let hooks = crate::hooks::HookRegistry::new();
        // One short message: "hi" = 2 chars
        let mut core = make_test_core(vec![Message::user("hi".to_string())], &hooks);
        // Set limit to 1000 — far above 2 chars
        core.max_transcript_chars = Some(1000);
        let usage = TokenUsage::default();
        let result = core.enforce_transcript_budget(0, &usage);
        assert!(
            result.is_none(),
            "chars (2) < limit (1000) must return None; got Some(_)"
        );
    }

    #[test]
    fn enforce_transcript_budget_returns_finish_reason_when_over_limit() {
        // Validates the >-limit path: when chars >= limit, must return Some with TranscriptLimit.
        use crate::agent::FinishReason;
        use crate::llm::TokenUsage;
        let hooks = crate::hooks::HookRegistry::new();
        // Message with 100 'x' chars — well above a limit of 10
        let msg = Message::user("x".repeat(100));
        let mut core = make_test_core(vec![msg], &hooks);
        core.max_transcript_chars = Some(10);
        let usage = TokenUsage::default();
        let result = core.enforce_transcript_budget(0, &usage);
        assert!(
            result.is_some(),
            "chars (100) >= limit (10) must return Some finish reason"
        );
        let (finish, _step) = result.unwrap();
        assert!(
            matches!(finish, FinishReason::TranscriptLimit { .. }),
            "finish reason must be TranscriptLimit; got {finish:?}"
        );
    }

    #[test]
    fn maybe_trim_replaces_large_tool_result_when_over_limit() {
        let hooks = crate::hooks::HookRegistry::new();
        // A tool result with content > MIN_TRIM_LENGTH (200) bytes
        let large_output = "x".repeat(MIN_TRIM_LENGTH + 100);
        let messages = vec![
            Message::system("sys".to_string()),
            Message::tool_result("c1", &large_output),
        ];
        let mut core = make_test_core(messages, &hooks);

        // Set limit below total content so trimming fires
        core.maybe_trim_transcript(10, 0);

        assert_eq!(
            core.messages[1].content, TRIM_PLACEHOLDER,
            "large tool result must be replaced with placeholder"
        );
    }

    #[test]
    fn maybe_trim_does_not_trim_small_tool_results() {
        let hooks = crate::hooks::HookRegistry::new();
        // A tool result shorter than MIN_TRIM_LENGTH
        let small_output = "small".to_string();
        let messages = vec![
            Message::system("sys".to_string()),
            Message::tool_result("c1", &small_output),
        ];
        let mut core = make_test_core(messages, &hooks);

        // Trigger trim (limit = 0)
        core.maybe_trim_transcript(0, 0);

        assert_eq!(
            core.messages[1].content, small_output,
            "short tool result must not be trimmed (< MIN_TRIM_LENGTH)"
        );
    }

    #[test]
    fn maybe_trim_does_not_trim_exactly_min_trim_length() {
        // Kills the `replace > with >=` mutant: content.len() == MIN_TRIM_LENGTH
        // must NOT be trimmed under the strict `>` check.
        let hooks = crate::hooks::HookRegistry::new();
        let exact_output = "z".repeat(MIN_TRIM_LENGTH);
        let messages = vec![
            Message::system("s".to_string()),
            Message::tool_result("c1", &exact_output),
        ];
        let mut core = make_test_core(messages, &hooks);
        // Set limit = 0 so trimming is attempted
        core.maybe_trim_transcript(0, 0);
        assert_eq!(
            core.messages[1].content, exact_output,
            "content exactly at MIN_TRIM_LENGTH must NOT be trimmed (> is strict)"
        );
    }

    #[test]
    fn maybe_trim_trims_one_more_than_min_trim_length() {
        // Complementary to the above: content.len() == MIN_TRIM_LENGTH + 1 MUST be trimmed.
        let hooks = crate::hooks::HookRegistry::new();
        let just_over = "z".repeat(MIN_TRIM_LENGTH + 1);
        let messages = vec![
            Message::system("s".to_string()),
            Message::tool_result("c1", &just_over),
        ];
        let mut core = make_test_core(messages, &hooks);
        core.maybe_trim_transcript(0, 0);
        assert_eq!(
            core.messages[1].content, TRIM_PLACEHOLDER,
            "content one byte over MIN_TRIM_LENGTH must be trimmed"
        );
    }

    #[test]
    fn maybe_trim_stops_after_fitting_under_limit() {
        let hooks = crate::hooks::HookRegistry::new();
        // Two large tool results; only trim until we go under the limit.
        let large1 = "a".repeat(MIN_TRIM_LENGTH + 100);
        let large2 = "b".repeat(MIN_TRIM_LENGTH + 100);
        let messages = vec![
            Message::system("sys".to_string()),
            Message::tool_result("c1", &large1),
            Message::tool_result("c2", &large2),
        ];
        let mut core = make_test_core(messages, &hooks);

        // Set limit just above placeholder_len so trimming the first result suffices.
        let placeholder_len = TRIM_PLACEHOLDER.len();
        let limit = 3 + placeholder_len + large2.len() + 50; // sys + placeholder + large2 + buffer
        core.maybe_trim_transcript(limit, 0);

        assert_eq!(
            core.messages[1].content, TRIM_PLACEHOLDER,
            "first large result should be trimmed"
        );
        assert_eq!(
            core.messages[2].content, large2,
            "second result should not be touched once we're under limit"
        );
    }

    #[test]
    fn maybe_trim_trims_when_chars_exactly_equals_limit() {
        // Kills: `replace < with <=` at line 196.
        //
        // When chars == limit the original `<` guard is FALSE → trimming proceeds.
        // With `<=`, it would be TRUE → early return, nothing is trimmed.
        let hooks = crate::hooks::HookRegistry::new();
        // Build a transcript whose total content length == limit exactly.
        let tool_content = "a".repeat(MIN_TRIM_LENGTH + 50); // 250 chars
        let messages = vec![
            Message::system("s".to_string()),          // 1 char
            Message::tool_result("c1", &tool_content), // 250 chars
        ];
        // Total = 1 + 250 = 251.  Set limit = 251 so chars == limit.
        let limit = 1 + tool_content.len();
        let mut core = make_test_core(messages, &hooks);
        core.maybe_trim_transcript(limit, 0);

        assert_eq!(
            core.messages[1].content, TRIM_PLACEHOLDER,
            "tool result must be trimmed when chars == limit (not strictly less)"
        );
    }

    #[test]
    fn maybe_trim_continues_when_chars_equals_limit_after_first_trim() {
        // Kills: `replace < with <=` at line 211 (the inner break condition).
        //
        // After trimming the first tool result, if the remaining chars == limit,
        // the original `<` is FALSE → the loop continues and trims the second tool result.
        // With `<=`, it would be TRUE → break, leaving the second tool result untrimmed.
        let hooks = crate::hooks::HookRegistry::new();

        let tool_content = "a".repeat(MIN_TRIM_LENGTH + 50); // 250 chars
        let messages = vec![
            Message::system("s".to_string()),          // 1 char
            Message::tool_result("c1", &tool_content), // 250 chars (will be trimmed first)
            Message::tool_result("c2", &tool_content), // 250 chars (must ALSO be trimmed)
        ];
        // Total = 1 + 250 + 250 = 501
        // TRIM_PLACEHOLDER.len() = 40
        // After trimming tool1: 501 - 250 + 40 = 291
        // Set limit = 291 so that after trimming tool1, chars == limit (not < limit).
        // Original `<`: 291 < 291 → false → continues trimming tool2.
        // Mutant  `<=`: 291 <= 291 → true  → breaks, tool2 NOT trimmed.
        let after_first_trim = 1 + TRIM_PLACEHOLDER.len() + tool_content.len();
        let limit = after_first_trim; // exact boundary

        let mut core = make_test_core(messages, &hooks);
        core.maybe_trim_transcript(limit, 0);

        assert_eq!(
            core.messages[1].content, TRIM_PLACEHOLDER,
            "first tool result must be trimmed"
        );
        assert_eq!(
            core.messages[2].content, TRIM_PLACEHOLDER,
            "second tool result must ALSO be trimmed when chars == limit after first trim"
        );
    }

    #[test]
    fn maybe_trim_emits_event_when_trimmed() {
        // Kills: `replace += with *=` (trimmed_count stays 0 → event not emitted)
        //        `replace > with ==/</>= in if trimmed_count > 0` (wrong guard)
        use crate::event::AgentEvent;
        use tokio::sync::mpsc;

        let hooks = crate::hooks::HookRegistry::new();
        let large_output = "x".repeat(MIN_TRIM_LENGTH + 50);
        let messages = vec![
            Message::system("sys".to_string()),
            Message::tool_result("c1", &large_output),
        ];
        let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
        let mut core = make_test_core(messages, &hooks);
        core.events = Some(tx);

        // Trim fires (limit = 0 → chars > 0 = limit)
        core.maybe_trim_transcript(0, 0);

        // The emit must have sent an AssistantText event containing the trim notice
        let event = rx
            .try_recv()
            .expect("an event must have been emitted after trimming");
        match event {
            AgentEvent::AssistantText { text, .. } => {
                assert!(
                    text.contains("trimmed"),
                    "trim event text must mention 'trimmed'; got: {text}"
                );
            }
            other => panic!("expected AssistantText but got: {other:?}"),
        }
    }

    #[test]
    fn maybe_trim_no_event_when_nothing_trimmed() {
        // Complementary: no event when nothing is trimmed (trimmed_count == 0)
        use crate::event::AgentEvent;
        use tokio::sync::mpsc;

        let hooks = crate::hooks::HookRegistry::new();
        let small_output = "tiny".to_string(); // Below MIN_TRIM_LENGTH
        let messages = vec![
            Message::system("sys".to_string()),
            Message::tool_result("c1", &small_output),
        ];
        let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
        let mut core = make_test_core(messages, &hooks);
        core.events = Some(tx);

        // Limit = 0 → trimming is attempted, but nothing qualifies (< MIN_TRIM_LENGTH)
        core.maybe_trim_transcript(0, 0);

        assert!(
            rx.try_recv().is_err(),
            "no event must be emitted when nothing was trimmed"
        );
    }

    // ========================================================================
    // maybe_compact tests
    // ========================================================================

    #[tokio::test]
    async fn maybe_compact_noop_when_no_compactor() {
        let hooks = crate::hooks::HookRegistry::new();
        let messages = vec![
            Message::user("q".to_string()),
            Message::assistant("a".to_string()),
        ];
        let mut core = make_test_core(messages, &hooks);
        // compactor is None by default in make_test_core
        let result = core.maybe_compact(0).await;
        assert!(result.is_ok());
        assert_eq!(core.messages.len(), 2, "messages must not change");
    }

    #[tokio::test]
    async fn maybe_compact_noop_when_under_threshold() {
        use crate::compact::Compactor;
        let hooks = crate::hooks::HookRegistry::new();
        let messages = vec![
            Message::user("hi".to_string()),
            Message::assistant("hello".to_string()),
        ];
        let mut core = make_test_core(messages, &hooks);
        // Set a threshold much larger than the transcript
        core.compactor = Some(Compactor::new(usize::MAX));

        let result = core.maybe_compact(0).await;
        assert!(result.is_ok());
        assert_eq!(core.messages.len(), 2, "no compaction under threshold");
    }

    #[tokio::test]
    async fn maybe_compact_fires_when_over_threshold() {
        use crate::compact::Compactor;
        use crate::llm::{Completion, MockProvider};
        let hooks = crate::hooks::HookRegistry::new();
        // Build a transcript that exceeds the threshold
        let messages = vec![
            Message::system("sys".to_string()),
            Message::user("msg1".to_string()),
            Message::assistant("rep1".to_string()),
            Message::user("msg2".to_string()),
            Message::assistant("rep2".to_string()),
            Message::user("msg3".to_string()),
        ];
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "compact summary".to_string(),
            tool_calls: vec![],
            finish_reason: Some("stop".to_string()),
            usage: None,
            reasoning_content: None,
        }]));
        let mut core = make_test_core(messages, &hooks);
        core.llm = provider;
        // Threshold = 0 → always compact
        core.compactor = Some(Compactor::new(0).keep_recent_n(2));

        let result = core.maybe_compact(1).await;
        assert!(result.is_ok());
        // After compaction the transcript starts with a compaction summary
        assert!(
            core.messages[0].is_compaction_summary,
            "first message must be a compaction summary after compaction"
        );
    }

    #[tokio::test]
    async fn maybe_compact_fires_at_threshold_boundary() {
        // Kills: `replace < with <=` mutant at line 239.
        // With `<=`, when chars == threshold_chars, the function returns early instead of compacting.
        use crate::compact::Compactor;
        use crate::llm::{Completion, MockProvider};
        let hooks = crate::hooks::HookRegistry::new();
        let messages = vec![
            Message::system("sys".to_string()),
            Message::user("msg1".to_string()),
            Message::assistant("rep1".to_string()),
            Message::user("msg2".to_string()),
            Message::assistant("rep2".to_string()),
            Message::user("last".to_string()),
        ];
        // Compute exact chars
        let chars = crate::compact::Compactor::estimate_chars(&messages);
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "summary".to_string(),
            tool_calls: vec![],
            finish_reason: Some("stop".to_string()),
            usage: None,
            reasoning_content: None,
        }]));
        let mut core = make_test_core(messages, &hooks);
        core.llm = provider;
        // Set threshold = exact chars → `chars < threshold` is false → compaction fires
        // With `< → <=`: `chars <= chars` is true → returns early → no compaction
        core.compactor = Some(Compactor::new(chars).keep_recent_n(2));

        let result = core.maybe_compact(1).await;
        assert!(result.is_ok());
        assert!(
            core.messages[0].is_compaction_summary,
            "compaction must fire when chars == threshold (chars is NOT less than threshold)"
        );
    }

    /// Kills: `delete ! in RunCore<'a>::execute_tool_calls` at line 306.
    ///
    /// When `exploring_plan_mode == false` and the registry's mode is Plan,
    /// calling any tool (other than enter/exit plan mode) must return an error
    /// asking the agent to call `enter_plan_mode` first.
    ///
    /// With the mutant (`!` deleted), the condition becomes:
    ///   `if self.exploring_plan_mode.load() && …`
    /// which is FALSE when not in plan mode, so the guard is skipped and the
    /// tool call proceeds instead of returning the expected error.
    #[tokio::test]
    async fn execute_tool_calls_plan_mode_guard_when_not_exploring() {
        use crate::llm::ToolCall;
        use crate::permissions::{PermissionMode, PermissionsConfig};
        use std::sync::atomic::AtomicBool;

        let hooks = crate::hooks::HookRegistry::new();
        let tools = Arc::new(crate::tools::ToolRegistry::default().with_permissions(
            PermissionsConfig {
                mode: PermissionMode::Plan {
                    pre_plan_mode: Box::new(PermissionMode::Default),
                    bypass_available: false,
                },
                layers: vec![],
            },
        ));

        let mut core = make_test_core(vec![], &hooks);
        core.tools = tools;
        // Explicitly not in plan-mode exploration.
        core.exploring_plan_mode = Arc::new(AtomicBool::new(false));

        let call = ToolCall {
            id: "tc1".to_string(),
            name: "Write".to_string(),
            arguments: serde_json::json!({}),
        };

        let results = core.execute_tool_calls(&[call]).await;

        assert_eq!(results.len(), 1, "should produce exactly one outcome");
        assert!(
            results[0].result.contains("enter_plan_mode"),
            "must tell the agent to call enter_plan_mode first; got: {}",
            results[0].result,
        );
    }

    #[tokio::test]
    async fn maybe_compact_kept_count_is_correct() {
        // Kills: `replace - with + in maybe_compact` at line 253.
        // `kept = kept_before - removed` should equal the number of messages remaining.
        // With `- → +`, `kept = kept_before + removed` which would be wrong.
        use crate::compact::Compactor;
        use crate::event::AgentEvent;
        use crate::llm::{Completion, MockProvider};
        use tokio::sync::mpsc;

        let hooks = crate::hooks::HookRegistry::new();
        let messages = vec![
            Message::system("sys".to_string()),
            Message::user("msg1".to_string()),
            Message::assistant("rep1".to_string()),
            Message::user("msg2".to_string()),
            Message::assistant("rep2".to_string()),
            Message::user("last".to_string()),
        ];
        let kept_before = messages.len();
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "summary".to_string(),
            tool_calls: vec![],
            finish_reason: Some("stop".to_string()),
            usage: None,
            reasoning_content: None,
        }]));
        let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
        let mut core = make_test_core(messages, &hooks);
        core.llm = provider;
        core.events = Some(tx);
        core.compactor = Some(Compactor::new(0).keep_recent_n(2));

        let result = core.maybe_compact(1).await;
        assert!(result.is_ok());

        // Drain events to find Compacted
        let mut found_compacted = false;
        while let Ok(ev) = rx.try_recv() {
            if let AgentEvent::Compacted { removed, kept, .. } = ev {
                assert_eq!(
                    kept + removed,
                    kept_before,
                    "kept + removed must equal the original message count"
                );
                // kept must be the actual remaining messages (after compaction, first is summary)
                assert_eq!(
                    kept,
                    kept_before - removed,
                    "kept must be kept_before - removed, not kept_before + removed"
                );
                found_compacted = true;
            }
        }
        assert!(found_compacted, "a Compacted event must have been emitted");
    }

    // ========================================================================
    // run_inner integration tests
    // These tests call run_inner() directly to kill mutants deep inside the
    // main agent loop that cannot be reached by unit tests of sub-functions.
    // ========================================================================

    /// Helper: build a RunCore that can run through `run_inner`.
    /// Messages start with a single user turn; LLM and max_steps are caller-set.
    fn make_run_core_for_inner<'a>(
        messages: Vec<Message>,
        hooks: &'a crate::hooks::HookRegistry,
        provider: Arc<crate::llm::MockProvider>,
        max_steps: usize,
    ) -> RunCore<'a> {
        use std::sync::atomic::AtomicBool;
        RunCore {
            messages: Arc::new(messages),
            llm: provider,
            tools: Arc::new(crate::tools::ToolRegistry::default()),
            max_steps,
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

    /// Kills: `replace >= with <` at line 579.
    ///
    /// When `chars >= limit` after trimming, `run_inner` must return
    /// `TranscriptLimit`.  With the mutant (`< limit`), the condition is
    /// inverted and `TranscriptLimit` would fire when chars is BELOW the
    /// limit — the happy path.
    #[tokio::test]
    async fn run_inner_returns_transcript_limit_when_chars_at_or_above_limit() {
        use crate::agent::FinishReason;

        let hooks = crate::hooks::HookRegistry::new();
        // System message "hello" = 5 chars; set limit = 3 → 5 >= 3 → TranscriptLimit.
        let messages = vec![Message::system("hello".to_string())];
        let provider = Arc::new(crate::llm::MockProvider::new(vec![]));
        let mut core = make_run_core_for_inner(messages, &hooks, provider, 1);
        core.max_transcript_chars = Some(3); // 5 chars > 3 → fires before LLM call

        let outcome = core.run_inner().await.expect("run_inner must not error");
        assert!(
            matches!(outcome.finish_reason, FinishReason::TranscriptLimit { .. }),
            "expected TranscriptLimit, got {:?}",
            outcome.finish_reason,
        );
    }

    /// Kills: `delete !` at line 679.
    ///
    /// When `reasoning_content` is non-empty, `run_inner` must emit a
    /// `AgentEvent::Reasoning` event.  With the mutant (delete `!`), the
    /// guard becomes `if reasoning.is_empty()`, so no event is emitted for
    /// non-empty reasoning.
    #[tokio::test]
    async fn run_inner_emits_reasoning_event_when_reasoning_nonempty() {
        use crate::event::AgentEvent;
        use crate::llm::Completion;
        use tokio::sync::mpsc;

        let hooks = crate::hooks::HookRegistry::new();
        let provider = Arc::new(crate::llm::MockProvider::new(vec![Completion {
            content: "done".to_string(),
            tool_calls: vec![],
            finish_reason: Some("stop".to_string()),
            usage: None,
            reasoning_content: Some("I thought about it".to_string()),
        }]));
        let messages = vec![Message::user("hello".to_string())];
        let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
        let mut core = make_run_core_for_inner(messages, &hooks, provider, 1);
        core.events = Some(tx);

        let outcome = core.run_inner().await.expect("run_inner must not error");
        drop(outcome); // satisfy unused-var lint

        let events: Vec<AgentEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::Reasoning { .. })),
            "AgentEvent::Reasoning must be emitted when reasoning_content is non-empty; got: {events:?}",
        );
    }

    /// Kills: `replace match guard … with false` at line 700:32 AND
    ///        `replace != with ==` at line 700:49.
    ///
    /// When the LLM returns `finish_reason = Some("rate_limited")` with no
    /// tool calls, `run_inner` must classify it as `FinishReason::ProviderStop`.
    /// Both mutants break the match guard so "rate_limited" falls to the
    /// default `NoMoreToolCalls` arm instead.
    #[tokio::test]
    async fn run_inner_provider_stop_for_nonstandard_finish_reason() {
        use crate::agent::FinishReason;
        use crate::llm::Completion;

        let hooks = crate::hooks::HookRegistry::new();
        let provider = Arc::new(crate::llm::MockProvider::new(vec![Completion {
            content: "provider paused".to_string(),
            tool_calls: vec![],
            finish_reason: Some("rate_limited".to_string()),
            usage: None,
            reasoning_content: None,
        }]));
        let messages = vec![Message::user("hello".to_string())];
        let core = make_run_core_for_inner(messages, &hooks, provider, 1);

        let outcome = core.run_inner().await.expect("run_inner must not error");
        assert!(
            matches!(
                &outcome.finish_reason,
                FinishReason::ProviderStop(r) if r == "rate_limited"
            ),
            "expected ProviderStop(\"rate_limited\"), got {:?}",
            outcome.finish_reason,
        );
    }

    /// Complementary: `finish_reason = Some("stop")` → `NoMoreToolCalls`.
    /// Pins the pass-through branch of the same match guard.
    #[tokio::test]
    async fn run_inner_no_provider_stop_for_stop_finish_reason() {
        use crate::agent::FinishReason;
        use crate::llm::Completion;

        let hooks = crate::hooks::HookRegistry::new();
        let provider = Arc::new(crate::llm::MockProvider::new(vec![Completion {
            content: "all done".to_string(),
            tool_calls: vec![],
            finish_reason: Some("stop".to_string()),
            usage: None,
            reasoning_content: None,
        }]));
        let messages = vec![Message::user("hello".to_string())];
        let core = make_run_core_for_inner(messages, &hooks, provider, 1);

        let outcome = core.run_inner().await.expect("run_inner must not error");
        assert!(
            matches!(outcome.finish_reason, FinishReason::NoMoreToolCalls),
            "expected NoMoreToolCalls, got {:?}",
            outcome.finish_reason,
        );
    }

    /// Kills (all three together):
    ///   `replace && with ||` at line 820:43  — stuck guard uses `||`
    ///   `replace / with *`   at line 827:51  — rate = count * window (huge)
    ///   `replace += with *=` at line 833:59  — count never increments
    ///
    /// Run the agent with 3 consecutive single-tool-call steps that all fail
    /// with "ERROR: …" (unknown tool).  With `stuck_window = 3` and
    /// `stuck_error_rate = 1.0`, stuck detection must fire on step 3.
    ///
    /// Each mutant breaks a different aspect of stuck detection:
    /// - `|| ` guard: fires after the FIRST error (window not yet full)
    /// - `*` rate:    fires after the first error (rate = 1 * 3 = 3 ≥ 1.0)
    /// - `*=` count:  all error_count = 0 → rate = 0 → stuck NEVER fires
    #[tokio::test]
    async fn run_inner_stuck_detection_fires_after_window_of_errors() {
        use crate::agent::FinishReason;
        use crate::llm::{Completion, ToolCall};

        let hooks = crate::hooks::HookRegistry::new();
        // Three steps each requesting the same non-existent tool → all fail.
        let make_tc_completion = |id: &str| Completion {
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: id.to_string(),
                name: "NonExistentTool".to_string(),
                arguments: serde_json::json!({}),
            }],
            finish_reason: None,
            usage: None,
            reasoning_content: None,
        };
        let provider = Arc::new(crate::llm::MockProvider::new(vec![
            make_tc_completion("tc1"),
            make_tc_completion("tc2"),
            make_tc_completion("tc3"),
        ]));
        let messages = vec![Message::user("keep trying".to_string())];
        let mut core = make_run_core_for_inner(messages, &hooks, provider, 5);
        core.stuck_window = 3;
        core.stuck_error_rate = 1.0;

        let outcome = core.run_inner().await.expect("run_inner must not error");
        assert!(
            matches!(outcome.finish_reason, FinishReason::Stuck { .. }),
            "expected Stuck after 3 consecutive errors, got {:?}",
            outcome.finish_reason,
        );
    }

    /// Kills: `replace && with ||` at line 820:43.
    ///
    /// The guard is `stuck_finish.is_none() && recent_errors.len() == stuck_window`.
    /// With `||`, it fires whenever `stuck_finish.is_none()` is true (always before
    /// stuck is set), so it triggers on every step — not just when the window fills.
    ///
    /// Setup: `stuck_window=3`, `stuck_error_rate=0.5`, `max_steps=2`.
    /// After 2 errors the window is NOT yet full (len=2 < window=3).
    ///   • Original (`&&`): condition false → no stuck → `BudgetExceeded`.
    ///   • Mutant (`||`): condition fires at step 2 (rate = 2/3 = 0.666 ≥ 0.5) → `Stuck`.
    #[tokio::test]
    async fn run_inner_stuck_fires_only_after_window_full() {
        use crate::agent::FinishReason;
        use crate::llm::{Completion, ToolCall};

        let hooks = crate::hooks::HookRegistry::new();
        let make_tc = |id: &str| Completion {
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: id.to_string(),
                name: "NonExistentTool".to_string(),
                arguments: serde_json::json!({}),
            }],
            finish_reason: None,
            usage: None,
            reasoning_content: None,
        };
        // Two steps — both produce errors, but the window (size 3) never fills.
        let provider = Arc::new(crate::llm::MockProvider::new(vec![
            make_tc("tc1"),
            make_tc("tc2"),
        ]));
        let messages = vec![Message::user("go".to_string())];
        let mut core = make_run_core_for_inner(messages, &hooks, provider, 2);
        core.stuck_window = 3;
        core.stuck_error_rate = 0.5; // 2/3 ≈ 0.666 ≥ 0.5 with mutant, but window not full

        let outcome = core.run_inner().await.expect("run_inner must not error");
        assert!(
            matches!(outcome.finish_reason, FinishReason::BudgetExceeded),
            "expected BudgetExceeded (window not yet full), got {:?}",
            outcome.finish_reason,
        );
    }

    /// Kills: `replace / with *` at line 827:51.
    ///
    /// `rate = error_count as f64 / stuck_window as f64` vs `* stuck_window`.
    /// With `window=3`, `error_count=1`, `stuck_error_rate=0.5`:
    ///   • Original: 1/3 ≈ 0.333 < 0.5 → no stuck → `BudgetExceeded`.
    ///   • Mutant:   1×3 = 3.0 ≥ 0.5   → stuck fires.
    ///
    /// We need exactly 1 error in a full window of 3: two successful SuccessTool
    /// calls followed by one NonExistentTool error fills the window with 1 error.
    #[tokio::test]
    async fn run_inner_stuck_rate_uses_division_not_multiplication() {
        use crate::agent::FinishReason;
        use crate::llm::ToolSpec;
        use crate::llm::{Completion, ToolCall};
        use crate::tools::{Tool, ToolRegistry};
        use async_trait::async_trait;

        struct SuccessTool;

        #[async_trait]
        impl Tool for SuccessTool {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "SuccessTool".to_string(),
                    description: "Returns ok".to_string(),
                    parameters: serde_json::json!({ "type": "object", "properties": {} }),
                }
            }
            async fn execute(&self, _args: serde_json::Value) -> crate::error::Result<String> {
                Ok("ok".to_string())
            }
        }

        let hooks = crate::hooks::HookRegistry::new();
        let registry = ToolRegistry::default().register(Arc::new(SuccessTool));

        let make_ok = |id: &str| Completion {
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: id.to_string(),
                name: "SuccessTool".to_string(),
                arguments: serde_json::json!({}),
            }],
            finish_reason: None,
            usage: None,
            reasoning_content: None,
        };
        let make_err = |id: &str| Completion {
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: id.to_string(),
                name: "NonExistentTool".to_string(),
                arguments: serde_json::json!({}),
            }],
            finish_reason: None,
            usage: None,
            reasoning_content: None,
        };
        // success, success, error → window=3 full with 1 error
        let provider = Arc::new(crate::llm::MockProvider::new(vec![
            make_ok("tc1"),
            make_ok("tc2"),
            make_err("tc3"),
        ]));
        let messages = vec![Message::user("test".to_string())];
        let mut core = make_run_core_for_inner(messages, &hooks, provider, 3);
        core.tools = Arc::new(registry);
        core.stuck_window = 3;
        core.stuck_error_rate = 0.5; // 1/3 ≈ 0.333 < 0.5 → no stuck (orig); 1×3=3.0 ≥ 0.5 (mutant)

        let outcome = core.run_inner().await.expect("run_inner must not error");
        assert!(
            matches!(outcome.finish_reason, FinishReason::BudgetExceeded),
            "expected BudgetExceeded (rate below threshold), got {:?}",
            outcome.finish_reason,
        );
    }

    /// Kills: `replace || with &&` at line 750:57 AND
    ///        `replace == with !=` at line 750:69.
    ///
    /// `DENIAL_LIMIT_SENTINEL = "ERROR_DENIAL_LIMIT:"` does NOT start with "ERROR: "
    /// (note the space), so `is_error` is true only via the `|| o.result == DENIAL_LIMIT_SENTINEL`
    /// branch.
    ///
    /// - Mutant 750:57 (`&&` instead of `||`): `false && true = false` → wrong
    /// - Mutant 750:69 (`!=` instead of `==`): `false || false = false` → wrong
    ///
    /// Both mutants emit `AgentEvent::ToolResult { is_error: false }` for the sentinel,
    /// but the correct code must emit `is_error: true`.
    #[tokio::test]
    async fn run_inner_denial_sentinel_emits_is_error_true() {
        use crate::event::AgentEvent;
        use crate::llm::{Completion, ToolCall, ToolSpec};
        use crate::tools::{Tool, ToolRegistry};
        use async_trait::async_trait;
        use tokio::sync::mpsc;

        struct DenialTool;

        #[async_trait]
        impl Tool for DenialTool {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "DenialTool".to_string(),
                    description: "Returns PermissionDeniedLimit".to_string(),
                    parameters: serde_json::json!({ "type": "object", "properties": {} }),
                }
            }
            async fn execute(&self, _args: serde_json::Value) -> crate::error::Result<String> {
                Err(crate::error::Error::PermissionDeniedLimit {
                    name: "DenialTool".to_string(),
                })
            }
        }

        let hooks = crate::hooks::HookRegistry::new();
        let registry = ToolRegistry::default().register(Arc::new(DenialTool));
        let provider = Arc::new(crate::llm::MockProvider::new(vec![Completion {
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "tc1".to_string(),
                name: "DenialTool".to_string(),
                arguments: serde_json::json!({}),
            }],
            finish_reason: None,
            usage: None,
            reasoning_content: None,
        }]));

        let messages = vec![Message::user("trigger denial".to_string())];
        let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
        let mut core = make_run_core_for_inner(messages, &hooks, provider, 3);
        core.tools = Arc::new(registry);
        core.events = Some(tx);

        let outcome = core.run_inner().await.expect("run_inner must not error");
        assert!(
            matches!(
                outcome.finish_reason,
                crate::agent::FinishReason::PermissionDenialLimit
            ),
            "expected PermissionDenialLimit, got {:?}",
            outcome.finish_reason,
        );

        let mut events = vec![];
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        let denial_is_error = events.iter().find_map(|e| {
            if let AgentEvent::ToolResult { name, is_error, .. } = e {
                if name == "DenialTool" {
                    return Some(*is_error);
                }
            }
            None
        });
        assert_eq!(
            denial_is_error,
            Some(true),
            "ToolResult for DENIAL_LIMIT_SENTINEL must have is_error = true; events: {events:?}"
        );
    }
}
