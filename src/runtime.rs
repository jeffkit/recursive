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

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, RwLock};

use crate::agent::FinishReason;
use crate::checkpoint::{CheckpointId, ShadowRepo};
use crate::checkpoint_log::CheckpointLogWriter;
use crate::compact::Compactor;
use crate::error::Result;
use crate::event::{AgentEvent, EventSink};
use crate::hooks::HookEvent;
use crate::kernel::{AgentKernel, TurnContext, TurnOutcome};
use crate::llm::{ChatProvider, TokenUsage};
use crate::message::Message;
use crate::tools::plan_mode::{ExitPlanModeTool, PlanApprovalGate, PlanModeRequestGate};
use crate::tools::{TodoItem, TodoWriteTool, TouchedFiles};

// Sub-modules extracted to keep `runtime.rs` under the invariant #1 line
// budget (see `tests/invariants/loop_size_orthogonality.rs`).
mod builder;
pub use builder::AgentRuntimeBuilder;

mod checkpoint;
pub(crate) use checkpoint::CheckpointState;

// ──────────────────────────────────────────────────────────────────────────
// Goal-168: GoalState / GoalStatus / GoalEvaluator
// ──────────────────────────────────────────────────────────────────────────

// Goal-loop data + judge live in `crate::runtime_goal`. Re-exported here so
// historical paths like `crate::runtime::GoalState` keep working.
pub use crate::runtime_goal::{GoalEvaluator, GoalState, GoalStatus, GoalVerdict};

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
    /// Checkpoint id captured at the end of this turn (if checkpointing
    /// is enabled and the runtime is bound to a session).
    pub checkpoint_id: Option<CheckpointId>,
}

impl From<TurnOutcome> for RuntimeOutcome {
    fn from(t: TurnOutcome) -> Self {
        Self {
            final_text: t.final_text,
            finish_reason: t.finish_reason,
            total_usage: t.usage,
            steps: t.steps,
            llm_latency_ms: t.llm_latency_ms,
            checkpoint_id: None,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────
// SessionLifecycle
// ──────────────────────────────────────────────────────────────────────────

/// Session-lifecycle state. Currently a single `closed` flag set by
/// `AgentRuntime::close()` to prevent duplicate `SessionEnd` events on
/// repeat calls. Kept as a sub-struct so future session-scoped signals
/// (last-activity timestamps, abort signals, etc.) have an obvious
/// home without bloating `AgentRuntime`'s top-level field list.
///
/// Named `SessionLifecycle` (not `SessionState`) to avoid confusion with
/// `crate::http::SessionState` and `agui_tui::app::SessionState`, which
/// describe session *metadata* (id, prompt count, last-active timestamp)
/// rather than the runtime's own lifecycle phase.
struct SessionLifecycle {
    closed: bool,
}

impl SessionLifecycle {
    fn open() -> Self {
        Self { closed: false }
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
    /// Accumulated conversation transcript (shared via Arc for O(1) clone
    /// when building TurnContext).
    transcript: Arc<Vec<Message>>,
    /// Event sink for streaming events (Arc for sharing with forwarder task).
    event_sink: Arc<dyn EventSink>,
    /// Whether to request streaming responses from the LLM.
    streaming: bool,
    /// Optional compactor for cross-turn transcript summarization.
    compactor: Option<Compactor>,
    /// Optional microcompactor for no-LLM proactive pruning of old tool
    /// results by count at cross-turn boundaries.
    microcompactor: Option<crate::compact::Microcompactor>,
    /// Goal-331: consecutive proactive compaction failures (cross-turn).
    /// Same semantics as `RunCore::consecutive_compact_failures`.
    consecutive_compact_failures: u32,
    /// Checkpoint subsystem (snapshot, session-id, writer, touched-files).
    checkpoints: CheckpointState,
    /// Session-lifecycle signals (close flag, future per-session toggles).
    /// See [`SessionLifecycle`] — kept small for now but is the natural home
    /// for any new "set once at session start / flip once at close" state.
    session: SessionLifecycle,
    /// Goal-167: shared task-list state written by `todo_write` calls.
    /// Read back via [`current_todos`](AgentRuntime::current_todos).
    todo_list: Arc<RwLock<Vec<TodoItem>>>,
    /// Goal-165: plan mode 2.0 gate — shared with `EnterPlanModeTool` and
    /// `ExitPlanModeTool`. `confirm_plan` / `reject_plan` forward to it.
    plan_approval_gate: Arc<PlanApprovalGate>,
    /// Goal-202: pre-confirmation gate — shared with `RequestPlanModeTool`.
    /// `approve_plan_mode_request` / `reject_plan_mode_request` forward here.
    plan_mode_request_gate: Arc<PlanModeRequestGate>,
    /// Goal-168: active goal state (set by `/goal`). `None` when no goal is active.
    /// Use [`current_goal`], [`set_goal`], and [`clear_goal`] for all access.
    goal_state: Arc<RwLock<Option<GoalState>>>,
    /// Goal-181: FIFO queue of user messages waiting to be processed.
    /// Callers use [`enqueue`](AgentRuntime::enqueue) instead of
    /// [`run`](AgentRuntime::run) directly; the queue is drained in FIFO
    /// order so that messages sent while a turn is in flight are processed
    /// automatically when the current turn completes.
    message_queue: std::collections::VecDeque<String>,
    /// Deferred `TurnFinished` event held by `execute_kernel_turn` until
    /// `emit_turn_messages` can flush it after all assistant messages.
    deferred_turn_finished: Option<AgentEvent>,
    /// Goal-291: number of most-recent transcript messages passed to the
    /// goal-evaluator judge on each turn. Smaller values reduce judge cost;
    /// larger values give the judge more context for long sessions.
    /// Default 12. Set via [`AgentRuntimeBuilder::goal_eval_transcript_tail`].
    goal_eval_transcript_tail: usize,
    /// Goal-328: structured prompt segments for ContextBreakdown estimator.
    prompt_segments: Option<crate::system_prompt::PromptSegments>,
    /// Goal-334: file re-injector (recently-read files as System atts).
    file_reinjector: Option<crate::compact::FileReinjector>,
    /// Goal-335: skill re-injector (invoked skill bodies as System atts).
    skill_reinjector: Option<crate::compact::SkillReinjector>,
    /// Goal-340: plan/todo re-injector (pending plan + task list as System atts).
    plan_todo_reinjector: Option<crate::compact::PlanTodoReinjector>,
    /// Goal-338: turn index of the most recent compaction, used to detect
    /// recompaction chains (compacting again within a few turns of the
    /// previous compaction). `None` when no compaction has occurred yet.
    last_compact_turn: Option<u32>,
}

impl std::fmt::Debug for AgentRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRuntime")
            .field("kernel", &self.kernel)
            .field("transcript", &self.transcript)
            .field("event_sink", &"<EventSink>")
            .field("streaming", &self.streaming)
            .field(
                "todo_list",
                &self.todo_list.read().map(|l| l.len()).unwrap_or(0),
            )
            .field(
                "goal_state",
                &self
                    .goal_state
                    .read()
                    .ok()
                    .and_then(|g| g.as_ref().map(|s| s.condition.clone())),
            )
            .field(
                "deferred_turn_finished",
                &self.deferred_turn_finished.as_ref().map(|_| "<event>"),
            )
            .field("goal_eval_transcript_tail", &self.goal_eval_transcript_tail)
            .field("file_reinjector", &self.file_reinjector.is_some())
            .field("skill_reinjector", &self.skill_reinjector.is_some())
            .field("plan_todo_reinjector", &self.plan_todo_reinjector.is_some())
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
    /// Appends `Message::user(text)` to the transcript, delegates to the kernel,
    /// appends the new messages back, and returns a [`RuntimeOutcome`].
    ///
    /// **Goal 284**: automatic pre/post checkpoints have been removed.
    /// The agent must call `checkpoint_save` explicitly to record restore
    /// points. `outcome.checkpoint_id` is always `None` here.
    pub async fn run(&mut self, user_text: impl Into<String>) -> Result<RuntimeOutcome> {
        let user_text = user_text.into();

        let turn = self.checkpoints.turn_index.load(Ordering::Relaxed);
        tracing::Span::current().record(
            "session_id",
            self.checkpoints.session_id.as_deref().unwrap_or(""),
        );
        tracing::debug!(
            session_id = self.checkpoints.session_id.as_deref().unwrap_or(""),
            turn,
            "agent.turn: starting"
        );

        // SessionStart fires exactly once — at the beginning of the first turn.
        if turn == 0 {
            self.kernel
                .hooks()
                .dispatch(HookEvent::SessionStart { goal: &user_text });
        }

        self.reset_touched_files();
        self.kernel.hooks().dispatch(HookEvent::UserPromptSubmit {
            content: &user_text,
        });
        self.append_user_message(&user_text).await;

        let turn_outcome = match self.execute_kernel_turn().await {
            Ok(outcome) => outcome,
            Err(e) if is_context_window_exceeded(&e) => {
                // The LLM rejected the request because the transcript exceeded its
                // context window.  Try to compact in-place (bypassing the normal
                // threshold gate) and retry the turn once before propagating.
                // The user message is already at the tail of `self.transcript`, so
                // after compaction the shorter transcript still ends with it.
                tracing::warn!(
                    target: "recursive::agent",
                    error = %e,
                    "context window exceeded; attempting emergency compaction before retry"
                );
                if self.compact_on_overflow().await? {
                    self.execute_kernel_turn().await?
                } else {
                    return Err(e);
                }
            }
            Err(e) => return Err(e),
        };
        self.emit_turn_messages(&turn_outcome).await;
        // Goal 289: cross-turn compaction runs AFTER the turn so the
        // threshold check sees the full turn's growth (user + assistant +
        // tool messages) rather than the pessimistic pre-turn size. One
        // pass per turn covers the entire growth instead of firing
        // reactively at the start of every turn.
        //
        // Use `last_prompt_tokens` — the single most-recent LLM call's
        // prompt_tokens — rather than `usage.prompt_tokens` (the accumulated
        // sum across all LLM calls in the turn). The accumulated sum grows
        // proportionally to the number of tool-use steps in the turn, causing
        // `should_compact` to fire prematurely on multi-step turns even when
        // the actual context usage is well below the threshold.
        //
        // Pass the full TokenUsage so cache_hit_tokens / cache_miss_tokens
        // land on the CompactionBoundary event (g336).
        self.maybe_compact_cross_turn(&turn_outcome.usage).await?;

        let outcome: RuntimeOutcome = turn_outcome.into();

        tracing::info!(
            steps = outcome.steps,
            finish_reason = ?outcome.finish_reason,
            "agent.turn: finished"
        );
        self.checkpoints.turn_index.fetch_add(1, Ordering::Relaxed);

        Ok(outcome)
    }

    /// Signal that the session is permanently over and fire `SessionEnd`.
    ///
    /// Call this exactly once, after the last `run()` or `enqueue()` call, to
    /// give hooks a chance to do post-session cleanup. Calling `run()` after
    /// `close()` is safe but `SessionEnd` will not fire again.
    pub async fn close(&mut self, last_outcome: Option<&RuntimeOutcome>) {
        if self.session.closed {
            return;
        }
        self.session.closed = true;
        if let Some(outcome) = last_outcome {
            if !matches!(outcome.finish_reason, FinishReason::Cancelled) {
                self.kernel
                    .hooks()
                    .dispatch(HookEvent::SessionEnd { outcome });
            }
        }
    }

    /// Reset the touched-files collector at the start of a turn.
    fn reset_touched_files(&self) {
        if let Some(slot) = &self.checkpoints.touched_files {
            if let Ok(mut t) = slot.lock() {
                *t = TouchedFiles::new();
            }
        }
    }

    /// Append a user message to the transcript and emit `MessageAppended`.
    async fn append_user_message(&mut self, user_text: &str) {
        let user_msg = Message::user(user_text.to_string());
        Arc::make_mut(&mut self.transcript).push(user_msg.clone());
        self.event_sink
            .emit(AgentEvent::MessageAppended {
                message: user_msg,
                usage: None,
            })
            .await;
    }

    /// Run cross-turn compaction if threshold is exceeded, emitting boundary events.
    ///
    /// This is the Wrapper's responsibility — the kernel only does intra-turn trim.
    /// The compaction summary is emitted as `MessageAppended` so it lands in the
    /// on-disk jsonl. A `CompactionBoundary` event (g157) lets the reader skip
    /// pre-compaction messages on resume.
    ///
    /// `last_usage` is the [`TokenUsage`] from the turn that just completed.
    /// Its `prompt_tokens` field is the actual prompt token count reported by the
    /// API — when non-zero and `compactor.threshold_prompt_tokens` is set, the
    /// token-based check takes priority over the character estimate (more reliable
    /// for CJK content where the 4-char/token assumption significantly
    /// underestimates token density). The `cache_hit_tokens` /
    /// `cache_miss_tokens` fields are forwarded to the emitted
    /// `CompactionBoundary` event for cache-telemetry (g336).
    pub async fn maybe_compact_cross_turn(&mut self, last_usage: &TokenUsage) -> Result<()> {
        // Goal 333: run microcompact before the LLM-summary check so that
        // count-based pruning of old tool results may drop the transcript
        // below the compaction threshold, skipping the expensive summary.
        if let Some(m) = &self.microcompactor {
            let turn = self.checkpoints.turn_index.load(Ordering::Relaxed);
            let pruned = m.prune(&mut *Arc::make_mut(&mut self.transcript));
            if pruned > 0 {
                self.event_sink
                    .emit(AgentEvent::Microcompact { step: turn, pruned })
                    .await;
            }
        }

        let Some(ref compactor) = self.compactor else {
            return Ok(());
        };

        // Circuit breaker: stop trying after too many consecutive failures.
        if self.consecutive_compact_failures >= crate::compact::MAX_CONSECUTIVE_COMPACT_FAILURES {
            self.event_sink
                .emit(AgentEvent::CompactionSkipped {
                    step: self.checkpoints.turn_index.load(Ordering::Relaxed),
                    reason: crate::event::CompactionSkipReason::CircuitBreaker,
                })
                .await;
            return Ok(());
        }

        let bytes = Compactor::estimate_bytes(&self.transcript);
        if !compactor.should_compact(bytes, last_usage.prompt_tokens) {
            return Ok(());
        }
        // Goal 345: only dispatch PreCompact when compaction will actually run,
        // so PreCompact / PostCompact stay balanced (mirrors run_core's
        // maybe_compact). Without this the degenerate-slice Ok(None) path
        // fired PreCompact with no matching PostCompact.
        if !compactor.would_compact(&self.transcript) {
            return Ok(());
        }
        self.kernel.hooks().dispatch(HookEvent::PreCompact {
            transcript_len: bytes,
        });
        // Snapshot pre-compact for file+skill reinjection (apply_to_transcript drains).
        let pre_compact: Vec<Message> = self.transcript.iter().cloned().collect();
        let result = compactor
            .apply_to_transcript(
                self.kernel.llm().as_ref(),
                Arc::make_mut(&mut self.transcript),
                self.checkpoints
                    .turn_index
                    .load(std::sync::atomic::Ordering::Relaxed),
            )
            .await;
        match result {
            Ok(Some((removed, summary_chars))) => {
                // Success — reset the circuit breaker.
                self.consecutive_compact_failures = 0;
                self.kernel.hooks().dispatch(HookEvent::PostCompact {
                    removed,
                    summary_chars,
                });
                self.event_sink
                    .emit(AgentEvent::CompactionBoundary {
                        turn: self.checkpoints.turn_index.load(Ordering::Relaxed) as u32,
                        compacted_count: removed,
                        summary_uuid: None,
                        cache_hit_tokens: last_usage.cache_hit_tokens,
                        cache_miss_tokens: last_usage.cache_miss_tokens,
                        is_recompaction_in_chain: self.last_compact_turn.is_some(),
                        turns_since_previous_compact: match self.last_compact_turn {
                            Some(prev) => {
                                let current =
                                    self.checkpoints.turn_index.load(Ordering::Relaxed) as u32;
                                current.saturating_sub(prev)
                            }
                            None => 0,
                        },
                        previous_compact_turn: self.last_compact_turn,
                    })
                    .await;
                self.last_compact_turn =
                    Some(self.checkpoints.turn_index.load(Ordering::Relaxed) as u32);
                if let Some(summary) = self.transcript.first().cloned() {
                    self.event_sink
                        .emit(AgentEvent::MessageAppended {
                            message: summary,
                            usage: None,
                        })
                        .await;
                }
                // Goal-334: re-inject recently-read files right after the summary.
                // Transcript = [summary, <file-atts>, <skill-atts>, ...preserved].
                if let Some(r) = &self.file_reinjector {
                    // Capture the preserved tail BEFORE we start inserting, so the
                    // slice indices stay valid. Summary is at index 0; preserved
                    // messages follow.
                    let preserved: Vec<_> = self.transcript.iter().skip(1).cloned().collect();
                    let atts = r.reinject(&preserved);
                    // Insert at index 1, shifting forward for final order.
                    for (offset, att) in atts.into_iter().enumerate() {
                        Arc::make_mut(&mut self.transcript).insert(1 + offset, att.clone());
                        self.event_sink
                            .emit(AgentEvent::MessageAppended {
                                message: att,
                                usage: None,
                            })
                            .await;
                    }
                }
                // Goal-335: re-invoke skills (scans pre-compact for Skill tool calls).
                if let Some(sr) = &self.skill_reinjector {
                    let atts = sr.reinject(&pre_compact);
                    // Insert after file attachments (which start at index 1).
                    let insert_base: usize = 1 + self.file_reinjector.as_ref().map_or(0, |r| {
                        // Approximate: the number of file attachments inserted.
                        // We don't track the count directly, but we know the
                        // file-reinjector reserves at most r.max_files slots.
                        // Use a safer heuristic: count existing system messages
                        // after index 1 that start with the file restore prefix.
                        self.transcript
                            .iter()
                            .skip(1)
                            .take(r.max_files)
                            .filter(|m| m.content.starts_with("[post-compact file restore:"))
                            .count()
                    });
                    for (offset, att) in atts.into_iter().enumerate() {
                        Arc::make_mut(&mut self.transcript)
                            .insert(insert_base + offset, att.clone());
                        self.event_sink
                            .emit(AgentEvent::MessageAppended {
                                message: att,
                                usage: None,
                            })
                            .await;
                    }
                }
                // Goal-340: re-inject pending plan and task list (reads shared state).
                if let Some(ptr) = &self.plan_todo_reinjector {
                    let atts = ptr.reinject();
                    if !atts.is_empty() {
                        // Count all already-inserted post-compact attachments so we
                        // insert right after them, before the preserved tail.
                        let insert_base: usize = 1 + self
                            .transcript
                            .iter()
                            .skip(1)
                            .take_while(|m| {
                                m.content.starts_with("[post-compact file restore:")
                                    || m.content.starts_with("[post-compact skill restore:")
                            })
                            .count();
                        for (offset, att) in atts.into_iter().enumerate() {
                            Arc::make_mut(&mut self.transcript)
                                .insert(insert_base + offset, att.clone());
                            self.event_sink
                                .emit(AgentEvent::MessageAppended {
                                    message: att,
                                    usage: None,
                                })
                                .await;
                        }
                    }
                }
            }
            Ok(None) => {
                // Transcript too short to compact — not a failure, leave counter unchanged.
            }
            Err(e) => {
                // Compaction failed — increment the breaker, emit event, continue.
                tracing::warn!(error = %e, "cross-turn proactive compaction failed");
                self.consecutive_compact_failures += 1;
                self.event_sink
                    .emit(AgentEvent::CompactionSkipped {
                        step: self.checkpoints.turn_index.load(Ordering::Relaxed),
                        reason: crate::event::CompactionSkipReason::Error,
                    })
                    .await;
            }
        }
        Ok(())
    }

    /// Force compact the transcript regardless of the configured threshold.
    ///
    /// Called when the LLM returns a context-window-exceeded error. Because
    /// the turn already failed we have no `prompt_tokens` reading; we bypass
    /// the threshold check entirely and compact immediately.
    ///
    /// Returns `true` when compaction succeeded (the transcript was long enough),
    /// `false` when the transcript was too short to compact or no compactor is
    /// configured. A `false` return means the caller should propagate the
    /// original error rather than retrying.
    async fn compact_on_overflow(&mut self) -> Result<bool> {
        let Some(ref compactor) = self.compactor else {
            return Ok(false);
        };
        // Keep the compaction lifecycle balanced: a rejected transcript must
        // not emit PreCompact because it has no matching PostCompact event.
        if !compactor.would_compact(&self.transcript) {
            return Ok(false);
        }
        let bytes = Compactor::estimate_bytes(&self.transcript);
        self.kernel.hooks().dispatch(HookEvent::PreCompact {
            transcript_len: bytes,
        });
        let turn = self
            .checkpoints
            .turn_index
            .load(std::sync::atomic::Ordering::Relaxed);
        let Some((removed, summary_chars)) = compactor
            .apply_to_transcript(
                self.kernel.llm().as_ref(),
                Arc::make_mut(&mut self.transcript),
                turn,
            )
            .await?
        else {
            return Ok(false);
        };
        self.kernel.hooks().dispatch(HookEvent::PostCompact {
            removed,
            summary_chars,
        });
        // The turn failed before reporting usage, so cache metrics are 0.
        self.event_sink
            .emit(AgentEvent::CompactionBoundary {
                turn: turn as u32,
                compacted_count: removed,
                summary_uuid: None,
                cache_hit_tokens: 0,
                cache_miss_tokens: 0,
                is_recompaction_in_chain: self.last_compact_turn.is_some(),
                turns_since_previous_compact: match self.last_compact_turn {
                    Some(prev) => (turn as u32).saturating_sub(prev),
                    None => 0,
                },
                previous_compact_turn: self.last_compact_turn,
            })
            .await;
        self.last_compact_turn = Some(turn as u32);
        if let Some(summary) = self.transcript.first().cloned() {
            self.event_sink
                .emit(AgentEvent::MessageAppended {
                    message: summary,
                    usage: None,
                })
                .await;
        }
        tracing::info!(
            target: "recursive::agent",
            removed,
            summary_chars,
            "emergency compaction complete; retrying turn"
        );
        Ok(true)
    }

    /// Build a `TurnContext`, run the kernel, and return the outcome.
    ///
    /// Spawns a forwarder task that withholds `TurnFinished` until after all
    /// assistant/tool `MessageAppended` events have been emitted (prevents SDK
    /// consumers from closing their stream before receiving the final text).
    async fn execute_kernel_turn(&mut self) -> Result<crate::kernel::TurnOutcome> {
        let (event_tx, mut event_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::event::AgentEvent>();
        let sink = self.event_sink.clone();
        let forwarder = tokio::spawn(async move {
            let mut deferred_finished: Option<crate::event::AgentEvent> = None;
            while let Some(ev) = event_rx.recv().await {
                if matches!(ev, AgentEvent::TurnFinished { .. }) {
                    deferred_finished = Some(ev);
                    continue;
                }
                sink.emit(ev).await;
            }
            deferred_finished
        });

        let ctx = TurnContext {
            messages: Arc::clone(&self.transcript),
            tool_specs: self.kernel.tools().specs(),
            step_events_tx: Some(event_tx.clone()),
            streaming: self.streaming,
            permission_hook: None,
            exploring_plan_mode: self.plan_approval_gate.exploring_plan_mode.clone(),
            permission_mode: self.kernel.tools().permission_mode(),
            mailbox: None,
            turn: self.checkpoints.turn_index.load(Ordering::Relaxed) as u32,
            prompt_segments: self.prompt_segments.clone(),
            wall_timeout_secs: 0,
        };

        let turn_outcome = self.kernel.run(ctx).await?;
        drop(event_tx);
        // Wait for forwarder; stash the deferred TurnFinished for emit_turn_messages.
        self.deferred_turn_finished = match forwarder.await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("forwarder task panicked, TurnFinished will be synthesized: {e}");
                None
            }
        };
        Ok(turn_outcome)
    }

    /// Append new kernel messages to the transcript and emit `MessageAppended`
    /// (or `MessageAppendedWithAudit`) for each, then flush the deferred
    /// `TurnFinished` event.
    async fn emit_turn_messages(&mut self, outcome: &crate::kernel::TurnOutcome) {
        let new_messages = &outcome.new_messages;
        let turn_usage = crate::session::UsageMeta::from_token_usage(&outcome.usage);
        let mut tool_audits = outcome.tool_audits.clone();
        // Token usage belongs only on the last assistant message of the turn —
        // attaching it to every assistant message would cause consumers to
        // multiply-count tokens.
        let last_assistant_idx = new_messages
            .iter()
            .rposition(|m| matches!(m.role, crate::message::Role::Assistant));
        Arc::make_mut(&mut self.transcript).extend(new_messages.iter().cloned());
        for (idx, msg) in new_messages.iter().enumerate() {
            let event = if msg.role == crate::message::Role::Tool {
                if let Some(tcid) = &msg.tool_call_id {
                    if let Some(audit) = tool_audits.remove(&(outcome.turn, tcid.clone())) {
                        AgentEvent::MessageAppendedWithAudit {
                            message: msg.clone(),
                            audit,
                        }
                    } else {
                        AgentEvent::MessageAppended {
                            message: msg.clone(),
                            usage: None,
                        }
                    }
                } else {
                    AgentEvent::MessageAppended {
                        message: msg.clone(),
                        usage: None,
                    }
                }
            } else {
                let usage = if matches!(msg.role, crate::message::Role::Assistant)
                    && Some(idx) == last_assistant_idx
                {
                    Some(turn_usage.clone())
                } else {
                    None
                };
                AgentEvent::MessageAppended {
                    message: msg.clone(),
                    usage,
                }
            };
            self.event_sink.emit(event).await;
        }
        // Emit TurnFinished after all messages are on the wire (SDK ordering guarantee).
        if let Some(ev) = self.deferred_turn_finished.take() {
            self.event_sink.emit(ev).await;
        }
    }

    // ── Goal-181: message queue ────────────────────────────────────────────

    /// Enqueue a user message and drain the queue in FIFO order.
    ///
    /// This is the preferred entry point for all interaction layers (TUI,
    /// HTTP, CLI).  Unlike calling [`run`](Self::run) directly, `enqueue`
    /// is safe to call while a turn is already in flight: the runtime is
    /// single-threaded (`&mut self`), so multiple callers naturally
    /// serialise.  The queue ensures messages submitted before the runtime
    /// is ready are not lost and are processed in order.
    ///
    /// ```text
    /// user sends A → enqueue(A) → run(A)
    /// user sends B while A runs → enqueue(B) → queue=[B]  (A already running via prior call)
    /// A finishes → loop pops B → run(B)
    /// ```
    ///
    /// In practice the outer loop (`drain_queue`) is what creates this
    /// ordering: a call to `enqueue` that arrives while another `enqueue`
    /// is executing on the same runtime will block on `&mut self` borrow,
    /// so the messages are processed strictly in order.
    pub async fn enqueue(&mut self, text: impl Into<String>) -> Result<Option<RuntimeOutcome>> {
        self.message_queue.push_back(text.into());
        self.drain_queue().await
    }

    /// Process all queued messages in FIFO order.
    ///
    /// Returns `Ok(Some(outcome))` for the last turn processed, or
    /// `Ok(None)` if the queue is empty when called.
    ///
    /// Stops on the first error and returns it to the caller. Messages
    /// that were not yet popped from the queue remain in the queue for
    /// later processing.
    async fn drain_queue(&mut self) -> Result<Option<RuntimeOutcome>> {
        let mut last: Option<RuntimeOutcome> = None;
        // Peek then run: only pop the message from the queue after `run`
        // returns Ok. Goal-259 — a transient error during `run` would
        // otherwise permanently lose the in-flight message. The message
        // stays at the front of the queue and can be retried by calling
        // `drain_queue` again once the error is handled.
        while let Some(msg) = self.message_queue.front().cloned() {
            match self.run(msg).await {
                Ok(outcome) => {
                    self.message_queue.pop_front();
                    last = Some(outcome);
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }
        Ok(last)
    }

    /// Number of messages currently waiting in the queue.
    ///
    /// Callers can expose this to the UI (e.g. status bar: "+N queued").
    pub fn queue_len(&self) -> usize {
        self.message_queue.len()
    }

    // ── Transcript access ──────────────────────────────────────────────────

    /// Return a reference to the accumulated transcript.
    pub fn transcript(&self) -> &[Message] {
        &self.transcript
    }

    /// Return the most-recent `n` transcript messages, or the full
    /// transcript if `n >= transcript.len()`. Returns an empty slice
    /// when `n == 0`.
    ///
    /// Used by the goal-loop judge (`run_goal_loop`) to keep the
    /// per-turn evaluator payload bounded as the transcript grows.
    /// Goal-260.
    pub fn transcript_tail(&self, n: usize) -> &[Message] {
        let t: &Vec<Message> = &self.transcript;
        let len = t.len();
        if n >= len {
            t
        } else {
            &t[len - n..]
        }
    }

    /// Replace the current transcript (useful for restoring from a saved session).
    pub fn set_transcript(&mut self, transcript: Vec<Message>) {
        self.transcript = Arc::new(transcript);
    }

    /// Discard all transcript messages after index `len`, restoring the
    /// transcript to the state it had before a turn started. Used by the
    /// TUI abort path to prevent orphan tool_call entries.
    pub fn truncate_transcript(&mut self, len: usize) {
        Arc::make_mut(&mut self.transcript).truncate(len);
    }

    /// Return a reference to the inner kernel.
    pub fn kernel(&self) -> &AgentKernel {
        &self.kernel
    }

    /// Hot-swap the LLM provider backing this runtime.
    ///
    /// Delegates to [`AgentKernel::set_llm`]. Used by the TUI `/model` picker
    /// to switch models mid-session. Must be called between turns (not while
    /// `run` / `enqueue` / `run_goal_loop` is executing on this runtime).
    pub fn set_llm(&mut self, llm: Arc<dyn ChatProvider>) {
        self.kernel.set_llm(llm);
    }

    /// Return the event sink currently in use.
    pub fn event_sink(&self) -> &dyn EventSink {
        self.event_sink.as_ref()
    }

    /// Set a cancellation token that interrupts the current (or next) agent turn.
    ///
    /// When the token is cancelled the step loop exits with
    /// [`FinishReason::Cancelled`](crate::agent::FinishReason::Cancelled) at
    /// the next step boundary.  This method replaces any previously installed
    /// token — call it before each `run()` so a fresh token is in place.
    pub fn set_interrupt_token(&mut self, token: tokio_util::sync::CancellationToken) {
        self.kernel.shutdown_token = Some(token);
    }

    /// Set the session id used for tracing-span labels and turn log lines.
    ///
    /// After this is called, every `run()` emits a tracing span record with
    /// `session_id=<id>` and an info log line carrying the same field, so logs
    /// and OTEL/Datadog traces can be filtered per session via
    /// `RUST_LOG=recursive[{session_id}]=debug` or the `session_id` label.
    pub fn set_session_id(&mut self, id: impl Into<String>) {
        self.checkpoints.session_id = Some(id.into());
    }

    /// Set a new event sink (useful for REPL mode between turns).
    ///
    /// **Replaces the sink AND re-registers the tools that hold an `Arc<dyn EventSink>`**
    /// — specifically [`TodoWriteTool`](crate::tools::todo::TodoWriteTool) (Goal-167)
    /// and [`ExitPlanModeTool`](crate::tools::plan_mode::ExitPlanModeTool) (Goal-165) —
    /// so that `AgentEvent::TodoUpdated` and `AgentEvent::PlanProposed` reach the new
    /// consumer (e.g. when the TUI swaps in a `TuiEventSink` after construction).
    ///
    /// The side effect is intentional: every caller that swaps the sink (CLI per-turn,
    /// HTTP per-session, TUI on backend init) expects those tools to forward events to
    /// the new sink. The method name documents the side effect; callers that only want
    /// to swap the sink without touching the tool registry must use
    /// [`replace_event_sink`](Self::replace_event_sink) instead.
    pub fn set_event_sink(&mut self, sink: Arc<dyn EventSink>) {
        self.event_sink = sink.clone();
        // Goal-167: re-register TodoWriteTool with the new sink so that
        // AgentEvent::TodoUpdated reaches the new consumer (e.g. TUI).
        self.kernel
            .tools_mut()
            .register_mut(Arc::new(TodoWriteTool::new(
                self.todo_list.clone(),
                sink.clone(),
            )));
        // Goal-165: re-register ExitPlanModeTool with the new sink so that
        // AgentEvent::PlanProposed reaches the new consumer (e.g. TUI).
        self.kernel
            .tools_mut()
            .register_mut(Arc::new(ExitPlanModeTool::new(
                self.plan_approval_gate.clone(),
                sink,
            )));
    }

    /// Swap the event sink **without** re-registering any sink-dependent tools.
    ///
    /// Use this when you know the new sink should only receive events emitted by
    /// `AgentRuntime` itself (e.g. `MessageAppended`, `TurnFinished`, compaction
    /// boundaries) and do not need the `TodoUpdated` / `PlanProposed` fan-out to
    /// the new consumer. Most callers want [`set_event_sink`](Self::set_event_sink)
    /// — its tool-reregistration side effect is what makes the TUI's
    /// `TodoUpdated` updates reach the live UI.
    ///
    /// Added in the P0-2 cleanup so the implicit side effect has a non-side-effect
    /// sibling.
    pub fn replace_event_sink(&mut self, sink: Arc<dyn EventSink>) {
        self.event_sink = sink;
    }

    /// Goal-167: return a snapshot of the current agent task list.
    ///
    /// Returns a clone of the list as it stands at call time. Returns an
    /// empty vec if the internal lock is poisoned.
    pub fn current_todos(&self) -> Vec<TodoItem> {
        self.todo_list.read().map(|l| l.clone()).unwrap_or_default()
    }

    /// Goal-161: attach a [`crate::tools::PermissionHook`] to the
    /// underlying tool registry so every tool invocation passes through
    /// the async permission gate before execution.
    pub fn set_permission_hook(&mut self, hook: Arc<dyn crate::tools::PermissionHook>) {
        self.kernel.tools_mut().set_permission_hook(hook);
    }

    /// Install a Claude SDK hook forwarder on the registry's
    /// [`ExternalHookRunner`] (control-channel `hook_callback`).
    pub fn set_sdk_hook_forwarder(
        &mut self,
        forwarder: Option<Arc<dyn crate::hooks::SdkHookForwarder>>,
    ) {
        self.kernel
            .tools_mut()
            .hook_runner
            .set_sdk_forwarder(forwarder);
    }

    /// Return a shared reference to the plan-approval gate.
    ///
    /// Callers (e.g. HTTP handlers) that need to inspect `pending_plan` or
    /// call `approve`/`reject` without holding the runtime `Mutex` can clone
    /// this `Arc` and operate on the gate directly.
    pub fn plan_approval_gate(&self) -> Arc<PlanApprovalGate> {
        self.plan_approval_gate.clone()
    }

    /// Return a shared reference to the plan-mode-request gate (Goal-202).
    ///
    /// The TUI backend's `run_turn_select_loop` clones this arc so it can
    /// forward `ApprovePlanMode` / `RejectPlanMode` user-actions to the gate
    /// while the runtime is executing inside a spawned task.
    pub fn plan_mode_request_gate(&self) -> Arc<PlanModeRequestGate> {
        self.plan_mode_request_gate.clone()
    }

    /// Confirm the pending plan, allowing execution to proceed.
    ///
    /// Wakes `exit_plan_mode`'s blocking wait via the Plan Mode 2.0 gate.
    pub fn confirm_plan(&mut self) {
        self.plan_approval_gate.approve();
    }

    /// Force a compaction pass right now, regardless of the
    /// configured threshold. Useful for TUI / API surfaces that
    /// expose a manual "/compact" command.
    ///
    /// No-op (returns `Ok(())`) when no compactor is configured or
    /// when the transcript is too small to compact (fewer than
    /// `keep_recent_n + 2` messages).
    pub async fn compact_now(&mut self) -> Result<()> {
        let Some(ref compactor) = self.compactor else {
            return Ok(());
        };
        if compactor
            .apply_to_transcript(
                self.kernel.llm().as_ref(),
                Arc::make_mut(&mut self.transcript),
                self.checkpoints
                    .turn_index
                    .load(std::sync::atomic::Ordering::Relaxed),
            )
            .await?
            .is_some()
        {
            self.last_compact_turn =
                Some(self.checkpoints.turn_index.load(Ordering::Relaxed) as u32);
        }
        Ok(())
    }

    /// Goal-342: partial compaction — summarise messages *before* a given
    /// transcript index, keeping everything from `pivot_index` onward
    /// (plus the compactor's `keep_recent_n` safety margin) verbatim.
    ///
    /// `pivot_index` is a transcript message index (0-based, same indexing
    /// as `transcript()`). Uses `Compactor::safe_split_point` on the
    /// sub-transcript `[..=pivot_index]` to find a safe split that never
    /// breaks tool-call pairs (invariant #8).
    ///
    /// No-op when no compactor is configured, `pivot_index` is out of
    /// range, or the computed split is 0 (nothing to compact).
    pub async fn compact_partial_before(&mut self, pivot_index: usize) -> Result<()> {
        let Some(ref compactor) = self.compactor else {
            return Ok(());
        };
        let transcript = Arc::make_mut(&mut self.transcript);
        if pivot_index >= transcript.len() {
            return Ok(());
        }
        // Consider the sub-transcript up to and including the pivot.
        let scope = &transcript[..=pivot_index];
        let split = Compactor::safe_split_point(scope, compactor.keep_recent_n);
        if split == 0 {
            return Ok(());
        }
        // Create a temporary compactor with keep_recent_n=0 so that
        // compact() summarises *all* of transcript[..split] (not just
        // the oldest part of it as the full keep_recent_n would).
        let zero_keep = Compactor {
            threshold_chars: compactor.threshold_chars,
            threshold_prompt_tokens: compactor.threshold_prompt_tokens,
            keep_recent_n: 0,
        };
        let step = self.checkpoints.turn_index.load(Ordering::Relaxed);
        let summary_msg = zero_keep
            .compact(self.kernel.llm().as_ref(), &transcript[..split], step)
            .await?;
        transcript.drain(..split);
        transcript.insert(0, summary_msg);
        self.last_compact_turn = Some(self.checkpoints.turn_index.load(Ordering::Relaxed) as u32);
        Ok(())
    }

    /// Goal-342: partial compaction — summarise messages *after* a given
    /// transcript index, keeping everything *before* `pivot_index` verbatim.
    ///
    /// `pivot_index` is a transcript message index (0-based). Backs up
    /// from the pivot as needed to avoid splitting tool-call pairs
    /// (invariant #8): any `Tool` or `Assistant` carrying `tool_calls`
    /// at the boundary is included with its pair.
    ///
    /// No-op when no compactor is configured or `pivot_index` is out of
    /// range.
    pub async fn compact_partial_after(&mut self, pivot_index: usize) -> Result<()> {
        let Some(ref compactor) = self.compactor else {
            return Ok(());
        };
        let transcript = Arc::make_mut(&mut self.transcript);
        if pivot_index >= transcript.len() {
            return Ok(());
        }
        // Back up from the pivot to avoid splitting tool-call pairs.
        // If the message at pivot_index is a Tool result or an
        // Assistant that issued tool_calls, include its pair by
        // starting earlier.
        let mut start = pivot_index;
        loop {
            if start == 0 {
                break;
            }
            let msg = &transcript[start];
            let should_back_up = msg.role == crate::message::Role::Tool
                || (msg.role == crate::message::Role::Assistant && !msg.tool_calls.is_empty());
            if should_back_up {
                start -= 1;
            } else {
                break;
            }
        }
        if start >= transcript.len() {
            return Ok(());
        }
        let suffix = transcript[start..].to_vec();
        // Nothing meaningful to summarise when the suffix is a single message
        // (or empty) — mirror compact_partial_before's `split == 0` no-op so a
        // degenerate slice does not surface a compact() error. (Goal 347
        // follow-up: surfaced by the too_short test once lib-test compilation
        // was re-enabled.)
        if suffix.len() <= 1 {
            return Ok(());
        }
        // Same zero-keep trick so compact() summarises everything in the suffix.
        let zero_keep = Compactor {
            threshold_chars: compactor.threshold_chars,
            threshold_prompt_tokens: compactor.threshold_prompt_tokens,
            keep_recent_n: 0,
        };
        let step = self.checkpoints.turn_index.load(Ordering::Relaxed);
        let summary_msg = zero_keep
            .compact(self.kernel.llm().as_ref(), &suffix, step)
            .await?;
        transcript.truncate(start);
        transcript.push(summary_msg);
        self.last_compact_turn = Some(self.checkpoints.turn_index.load(Ordering::Relaxed) as u32);
        Ok(())
    }

    /// Goal-202: approve the plan-mode entry request.
    ///
    /// Wakes `RequestPlanModeTool`'s blocking wait, returning `{"approved": true}`
    /// to the LLM so it can proceed with `enter_plan_mode`.
    pub fn approve_plan_mode_request(&self) {
        self.plan_mode_request_gate.approve();
    }

    /// Goal-202: reject the plan-mode entry request with a reason.
    ///
    /// Wakes `RequestPlanModeTool`'s blocking wait, returning
    /// `{"approved": false, "reason": "..."}` so the LLM can execute directly.
    pub fn reject_plan_mode_request(&self, reason: &str) {
        self.plan_mode_request_gate.reject(reason);
    }

    /// Reject the pending plan with a reason.
    ///
    /// Injects a user message into the transcript and wakes `exit_plan_mode`'s
    /// blocking wait (Plan Mode 2.0 gate) with the rejection reason.
    pub fn reject_plan(&mut self, reason: &str) {
        let rejection_msg = Message::user(format!("Plan rejected: {}", reason));
        Arc::make_mut(&mut self.transcript).push(rejection_msg);
        self.plan_approval_gate.reject(reason);
    }

    // ── Goal-168: goal state accessors ────────────────────────────────────

    /// Return a clone of the current goal state (or `None`).
    pub fn current_goal(&self) -> Option<GoalState> {
        self.goal_state.read().ok().and_then(|g| g.clone())
    }

    /// Set a new active goal. Emits `AgentEvent::GoalSet` via the event sink.
    pub async fn set_goal(&self, condition: String, max_turns: u32) {
        let state = GoalState {
            condition: condition.clone(),
            status: GoalStatus::Pursuing,
            turns: 0,
            max_turns,
            last_reason: None,
        };
        if let Ok(mut g) = self.goal_state.write() {
            *g = Some(state);
        }
        self.event_sink
            .emit(AgentEvent::GoalSet {
                condition,
                max_turns,
            })
            .await;
    }

    /// Clear the active goal. Emits `AgentEvent::GoalCleared`.
    pub async fn clear_goal(&self) {
        if let Ok(mut g) = self.goal_state.write() {
            *g = None;
        }
        self.event_sink.emit(AgentEvent::GoalCleared).await;
    }

    /// Run a goal loop: execute turns until the judge says the condition
    /// is met, the turn budget is exhausted, or the goal is cleared externally.
    ///
    /// Steps per iteration:
    /// 1. `run(prompt)` — execute one agent turn.
    /// 2. Increment `GoalState.turns`.
    /// 3. If `turns >= max_turns` → emit `GoalCleared` (budget exceeded), break.
    /// 4. Call `GoalEvaluator::evaluate(condition, transcript_tail)`.
    /// 5. If `achieved` → emit `GoalAchieved`, break.
    /// 6. Else → emit `GoalContinuing { reason }`, continue with auto-prompt.
    pub async fn run_goal_loop(
        &mut self,
        initial_prompt: impl Into<String>,
        condition: impl Into<String>,
        max_turns: u32,
    ) -> Result<Vec<RuntimeOutcome>> {
        let condition = condition.into();
        self.set_goal(condition.clone(), max_turns).await;

        let evaluator = GoalEvaluator::new(self.kernel.llm().clone());
        let mut outcomes = Vec::new();
        let mut next_prompt = initial_prompt.into();

        loop {
            // Check if goal was externally cleared while we were looping.
            let active = self
                .goal_state
                .read()
                .ok()
                .and_then(|g| g.clone())
                .map(|g| g.status == GoalStatus::Pursuing)
                .unwrap_or(false);
            if !active {
                break;
            }

            let outcome = self.run(&next_prompt).await?;
            outcomes.push(outcome);

            // Increment turn counter and check budget in a single write lock
            // (C-2: TOCTOU fix — previously two separate locks created a window
            // where an external clear_goal() call could set goal=None between the
            // increment and the budget check, causing a duplicate GoalCleared emit).
            enum TurnOutcomeKind {
                Continue(u32),
                BudgetExceeded(u32),
                ExternallyCleared,
            }
            let turn_outcome = {
                let mut guard = match self.goal_state.write().ok() {
                    Some(g) => g,
                    None => break,
                };
                match *guard {
                    None => TurnOutcomeKind::ExternallyCleared,
                    Some(ref mut gs) => {
                        gs.turns += 1;
                        let turns = gs.turns;
                        if turns >= max_turns {
                            *guard = None;
                            TurnOutcomeKind::BudgetExceeded(turns)
                        } else {
                            TurnOutcomeKind::Continue(turns)
                        }
                    }
                }
            };

            let turns = match turn_outcome {
                TurnOutcomeKind::ExternallyCleared => break,
                TurnOutcomeKind::BudgetExceeded(t) => {
                    self.event_sink.emit(AgentEvent::GoalCleared).await;
                    tracing::warn!(
                        "goal loop: turn budget of {max_turns} exceeded without achieving condition"
                    );
                    let _ = t;
                    break;
                }
                TurnOutcomeKind::Continue(t) => t,
            };

            // Ask the judge.
            // Goal-260: pass a tail slice, not the full transcript. The judge
            // only needs recent progress; the full transcript grows every turn
            // and would balloon the judge call's payload.
            // Goal-291: the slice length is now configurable via
            // `goal_eval_transcript_tail` (default 12, matching the previous
            // `GOAL_EVAL_TRANSCRIPT_TAIL` constant).
            let tail = self.transcript_tail(self.goal_eval_transcript_tail);
            let verdict = evaluator.evaluate(&condition, tail).await?;
            if verdict.achieved {
                if let Ok(mut g) = self.goal_state.write() {
                    if let Some(ref mut gs) = *g {
                        gs.status = GoalStatus::Achieved;
                        gs.last_reason = Some(verdict.reason.clone());
                    }
                    *g = None;
                }
                self.event_sink
                    .emit(AgentEvent::GoalAchieved {
                        condition: condition.clone(),
                        turns,
                    })
                    .await;
                break;
            } else {
                // Store reason and continue.
                if let Ok(mut g) = self.goal_state.write() {
                    if let Some(ref mut gs) = *g {
                        gs.last_reason = Some(verdict.reason.clone());
                    }
                }
                self.event_sink
                    .emit(AgentEvent::GoalContinuing {
                        reason: verdict.reason.clone(),
                        turns,
                    })
                    .await;

                next_prompt = format!(
                    "(Goal: {condition})\n\nPrevious attempt reason: {}\n\nContinue.",
                    verdict.reason
                );
            }
        }

        Ok(outcomes)
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

        // Completed background jobs drained in one wake but not yet serviced
        // as their own turn. Kept here (not just in the manager) so a
        // multi-job completion — where the manager's `notify_one()` permits
        // coalesce into a single wake — cannot orphan a job (goal-379).
        let mut pending_jobs: std::collections::VecDeque<(String, String)> =
            std::collections::VecDeque::new();

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
            // Use .lock().await instead of try_lock() so a completed job is
            // never silently skipped when the lock is momentarily contended.
            // Drain EVERY currently-completed job (via `take_completed`) in
            // one wake into `pending_jobs`, then service one per turn —
            // identical to the old single-job path (one
            // `Background job 'X' completed:` turn per job), but with no job
            // left behind when ≥2 jobs finish while a turn is in flight.
            if let Some(mgr) = bg_manager {
                let mut mgr = mgr.lock().await;
                while let Some((id, output)) = mgr.take_completed() {
                    pending_jobs.push_back((id, output));
                }
            }
            if let Some((id, output)) = pending_jobs.pop_front() {
                next_goal = format!("Background job '{}' completed:\n{}", id, output);
                continue;
            }

            // Nothing to do → loop ends
            break;
        }
        Ok(outcomes)
    }

    // ──────────────────────────────────────────────────────────────────
    // Checkpoint helpers
    // ──────────────────────────────────────────────────────────────────

    /// Bind this runtime to a checkpoint chain. With **Goal 284**,
    /// automatic per-turn snapshots are removed. Checkpoints are
    /// created only when the agent explicitly calls `checkpoint_save`.
    ///
    /// Side effect: registers `checkpoint_list`, `checkpoint_diff`, and
    /// `checkpoint_save` tools, scoped to this session, onto the kernel's
    /// tool registry.
    pub fn enable_checkpoints(
        &mut self,
        shadow: Arc<ShadowRepo>,
        session_id: impl Into<String>,
        log_path: std::path::PathBuf,
        touched_slot: Option<Arc<Mutex<TouchedFiles>>>,
    ) -> Result<()> {
        let writer = Arc::new(Mutex::new(CheckpointLogWriter::open(&log_path)?));
        let session_id = session_id.into();

        // Register session-scoped read-only checkpoint tools onto the
        // kernel's registry. The shadow repo is shared via
        // Arc<Mutex<ShadowRepo>> so the tools and the runtime see the
        // same checkpoint chain.
        let tool_repo = Arc::new(Mutex::new(ShadowRepo::clone(&shadow)));
        let ctx = crate::tools::CheckpointToolCtx {
            repo: tool_repo.clone(),
            session_id: session_id.clone(),
        };
        let tools = self.kernel.tools_mut();
        tools.register_mut(Arc::new(crate::tools::CheckpointList::new(ctx.clone())));
        tools.register_mut(Arc::new(crate::tools::CheckpointDiff::new(ctx)));

        // Goal 284: register the on-demand checkpoint_save tool.
        let save_tool = crate::tools::checkpoint::build_checkpoint_save_tool(
            tool_repo,
            session_id.clone(),
            touched_slot.clone(),
            writer.clone(),
            self.checkpoints.turn_index.clone(),
            log_path.clone(),
        );
        tools.register_mut(Arc::new(save_tool));

        self.checkpoints.shadow = Some(shadow);
        self.checkpoints.session_id = Some(session_id);
        self.checkpoints.writer = Some(writer);
        self.checkpoints.touched_files = touched_slot;
        self.checkpoints.log_path = Some(log_path);
        Ok(())
    }

    /// Whether checkpoint snapshots are active.
    pub fn checkpoints_enabled(&self) -> bool {
        self.checkpoints.enabled()
    }

    /// Returns the 0-indexed counter that will be assigned to the
    /// *next* turn (i.e. the count of turns already executed).
    pub fn turn_index(&self) -> usize {
        self.checkpoints.turn_index.load(Ordering::Relaxed)
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Context-window overflow detection
// ──────────────────────────────────────────────────────────────────────────

use crate::error::is_context_window_exceeded;

// ──────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::HookRegistry;
    use crate::llm::{Completion, MockProvider};
    use crate::tools::plan_mode::{ENTER_PLAN_MODE_TOOL_NAME, EXIT_PLAN_MODE_TOOL_NAME};
    use crate::tools::todo::TodoStatus;
    use crate::tools::Tool;
    use crate::tools::ToolRegistry;
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

    // ── checkpoint integration ────────────────────────────────────────

    fn has_git() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok()
    }

    /// Workspace tempdir + sibling shadow tempdir, both alive together.
    /// Tests open `ShadowRepo::open_at(...)` against `shadow_dir()` to
    /// avoid touching `paths::user_data_dir()` and the global env lock.
    struct ShadowWs {
        workspace: tempfile::TempDir,
        shadow: tempfile::TempDir,
    }

    impl ShadowWs {
        fn path(&self) -> &std::path::Path {
            self.workspace.path()
        }
        fn shadow_dir(&self) -> std::path::PathBuf {
            self.shadow.path().join("shadow-git")
        }
    }

    fn shadow_ws() -> ShadowWs {
        ShadowWs {
            workspace: tempfile::tempdir().expect("workspace tempdir"),
            shadow: tempfile::tempdir().expect("shadow tempdir"),
        }
    }

    /// Goal 284: with on-demand checkpoints, automatic per-turn snapshots
    /// are gone. Verify that `outcome.checkpoint_id` is `None` and no
    /// log entries are written automatically. The agent must call
    /// `checkpoint_save` to persist a checkpoint.
    #[tokio::test]
    async fn runtime_no_auto_snapshots_with_checkpoints_enabled() {
        if !has_git() {
            return;
        }
        let dir = shadow_ws();
        std::fs::write(dir.path().join("seed.txt"), "v0").unwrap();

        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "ok".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "ok2".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));
        let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();

        let shadow = Arc::new(crate::ShadowRepo::open_at(dir.path(), dir.shadow_dir()).unwrap());
        let log_path = dir.path().join("checkpoints.jsonl");
        rt.enable_checkpoints(shadow.clone(), "sess", log_path.clone(), None)
            .unwrap();
        assert!(rt.checkpoints_enabled());

        let o1 = rt.run("turn 0").await.unwrap();
        assert!(o1.checkpoint_id.is_none(), "no auto-snapshot in Goal 284");
        let o2 = rt.run("turn 1").await.unwrap();
        assert!(o2.checkpoint_id.is_none(), "no auto-snapshot in Goal 284");

        // No log entries should exist (agent never called checkpoint_save).
        let recs = crate::read_checkpoint_log(&log_path).unwrap();
        assert_eq!(recs.len(), 0, "no auto log entries");
    }

    /// Goal 284: verify that `checkpoint_save` tool is registered
    /// when checkpoints are enabled.
    #[tokio::test]
    async fn checkpoint_save_tool_is_registered() {
        if !has_git() {
            return;
        }
        let dir = shadow_ws();
        std::fs::write(dir.path().join("a.txt"), "hi").unwrap();

        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "ok".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();

        let shadow = Arc::new(crate::ShadowRepo::open_at(dir.path(), dir.shadow_dir()).unwrap());
        let log_path = dir.path().join("checkpoints.jsonl");
        rt.enable_checkpoints(shadow, "sess", log_path, None)
            .unwrap();

        let tools = rt.kernel.tools();
        assert!(
            tools.get("checkpoint_save").is_some(),
            "checkpoint_save must be registered"
        );
        assert!(
            tools.get("checkpoint_list").is_some(),
            "checkpoint_list must be registered"
        );
        assert!(
            tools.get("checkpoint_diff").is_some(),
            "checkpoint_diff must be registered"
        );
    }

    #[tokio::test]
    async fn runtime_works_when_checkpoints_disabled() {
        // No call to enable_checkpoints → outcome.checkpoint_id is None,
        // no log file created.
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "ok".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();
        let out = rt.run("hi").await.unwrap();
        assert!(out.checkpoint_id.is_none());
        assert!(!rt.checkpoints_enabled());
    }

    // ── compact_now (Goal 146) ────────────────────────────────────────────

    #[tokio::test]
    async fn compact_now_invokes_compactor() {
        // Provider used (a) to answer two normal turns, (b) to answer
        // the compactor's "summarize" call.
        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "first reply".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "second reply".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "compacted summary".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));
        // Threshold = MAX so the auto-compaction in `run` never fires;
        // keep_recent_n=1 so we only need 3 messages before compact_now
        // has work to do.
        let compactor = crate::compact::Compactor::new(usize::MAX).keep_recent_n(1);
        let mut rt = AgentRuntime::builder()
            .llm(llm)
            .compactor(compactor)
            .build()
            .unwrap();
        rt.run("turn 1").await.unwrap();
        rt.run("turn 2").await.unwrap();
        let len_before = rt.transcript().len();
        assert!(len_before >= 3, "expected ≥3 messages, got {len_before}");

        rt.compact_now().await.unwrap();
        // The compactor replaces older messages with one summary
        // system message plus keep_recent_n=1 verbatim message.
        assert_eq!(rt.transcript().len(), 2);
        assert_eq!(rt.transcript()[0].role, crate::message::Role::System);
        assert!(rt.transcript()[0].content.starts_with("[compacted:"));
    }

    #[tokio::test]
    async fn compact_now_is_noop_without_compactor() {
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "x".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();
        rt.run("hi").await.unwrap();
        let before = rt.transcript().len();
        rt.compact_now().await.unwrap();
        assert_eq!(rt.transcript().len(), before);
    }

    // ── Goal-305: turn index propagated to compaction summary header ──

    #[tokio::test]
    async fn compact_now_uses_turn_index_in_header() {
        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "first reply".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "second reply".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "compacted summary text".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));
        // keep_recent_n = 1 → transcript after compaction is [summary, last message].
        // The summary is always at index 0, so we can inspect its header.
        let compactor = crate::compact::Compactor::new(usize::MAX).keep_recent_n(1);
        let mut rt = AgentRuntime::builder()
            .llm(llm)
            .compactor(compactor)
            .build()
            .unwrap();

        // 2 turns → turn_index advances to 2.
        rt.run("turn 1").await.unwrap();
        rt.run("turn 2").await.unwrap();
        assert_eq!(rt.turn_index(), 2, "turn_index should be 2 after 2 turns");

        rt.compact_now().await.unwrap();

        // Transcript: [compaction summary, last verbatim message].
        assert_eq!(rt.transcript().len(), 2);
        let summary = &rt.transcript()[0].content;
        assert!(
            summary.contains("at step 2"),
            "compaction header should contain 'at step 2', got: {summary}"
        );
    }

    // ── Goal-342: compact_partial_before / compact_partial_after ─────────

    #[tokio::test]
    async fn compact_partial_before_summarizes_prefix_keeps_suffix() {
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "partial summary before pivot".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let compactor = crate::compact::Compactor::new(usize::MAX).keep_recent_n(2);
        let mut rt = AgentRuntime::builder()
            .llm(provider)
            .compactor(compactor)
            .build()
            .unwrap();
        // Seed: [system, user0, asst0, user1, asst1, user2, asst2]
        let msgs = vec![
            Message::system("sys"),
            Message::user("u0"),
            Message::assistant("a0"),
            Message::user("u1"),
            Message::assistant("a1"),
            Message::user("u2"),
            Message::assistant("a2"),
        ];
        *Arc::make_mut(&mut rt.transcript) = msgs;

        // Compact before index 5 (u2). The exact split depends on
        // safe_split_point; assert the invariants that matter: compaction
        // happened (transcript shrank, a summary now leads), and the tail
        // (pivot and after) is preserved verbatim.
        let len_before = rt.transcript().len();
        rt.compact_partial_before(5).await.unwrap();

        let after = rt.transcript();
        assert!(
            after.len() < len_before,
            "compact_partial_before must shrink the transcript: {} -> {}",
            len_before,
            after.len()
        );
        assert!(
            after[0].content.starts_with("[compacted:"),
            "summary must lead the transcript after compact_partial_before"
        );
        // The pivot (index 5 = "u2") and the message after it (a2) must be
        // preserved verbatim in the tail.
        assert!(
            after.iter().any(|m| m.content == "u2"),
            "pivot message u2 must survive in the tail"
        );
        assert!(
            after.iter().any(|m| m.content == "a2"),
            "message after pivot (a2) must survive in the tail"
        );
    }

    #[tokio::test]
    async fn compact_partial_after_summarizes_suffix_keeps_prefix() {
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "partial summary after pivot".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let compactor = crate::compact::Compactor::new(usize::MAX).keep_recent_n(2);
        let mut rt = AgentRuntime::builder()
            .llm(provider)
            .compactor(compactor)
            .build()
            .unwrap();
        // Seed: [system, user0, asst0, user1, asst1, user2, asst2]
        let msgs = vec![
            Message::system("sys"),
            Message::user("u0"),
            Message::assistant("a0"),
            Message::user("u1"),
            Message::assistant("a1"),
            Message::user("u2"),
            Message::assistant("a2"),
        ];
        *Arc::make_mut(&mut rt.transcript) = msgs;

        // Compact after index 3 (u1). The suffix from the pivot onward is
        // summarised; assert invariants: transcript shrank, the prefix before
        // the pivot is preserved verbatim, and a summary now closes it.
        let len_before = rt.transcript().len();
        rt.compact_partial_after(3).await.unwrap();

        let after = rt.transcript();
        assert!(
            after.len() < len_before,
            "compact_partial_after must shrink the transcript: {} -> {}",
            len_before,
            after.len()
        );
        // Prefix before index 3 ([sys, u0, a0]) must be preserved verbatim.
        assert_eq!(after[0].content, "sys");
        assert_eq!(after[1].content, "u0");
        assert_eq!(after[2].content, "a0");
        // The last message must be the compaction summary.
        assert!(
            after.last().unwrap().content.starts_with("[compacted:"),
            "summary must be the last message after compact_partial_after"
        );
    }

    #[tokio::test]
    async fn compact_partial_too_short_is_noop() {
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "summary".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let compactor = crate::compact::Compactor::new(usize::MAX).keep_recent_n(2);
        let mut rt = AgentRuntime::builder()
            .llm(provider)
            .compactor(compactor)
            .build()
            .unwrap();
        // Very short transcript: just a system prompt.
        *Arc::make_mut(&mut rt.transcript) = vec![Message::system("sys")];

        let before = rt.transcript().len();
        rt.compact_partial_before(0).await.unwrap();
        assert_eq!(rt.transcript().len(), before);

        rt.compact_partial_after(0).await.unwrap();
        assert_eq!(rt.transcript().len(), before);
    }

    #[tokio::test]
    async fn compact_partial_preserves_tool_call_pairing() {
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "tool-pair-summary".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let compactor = crate::compact::Compactor::new(usize::MAX).keep_recent_n(1);
        let mut rt = AgentRuntime::builder()
            .llm(provider)
            .compactor(compactor)
            .build()
            .unwrap();

        // Messages with a tool-call pair: [u0, a0(tool_calls), tool0, u1, a1]
        let tc = crate::llm::ToolCall {
            id: "c1".into(),
            name: "add".into(),
            arguments: serde_json::json!({}),
        };
        let msgs = vec![
            Message::user("u0"),
            Message::assistant_with_tool_calls("a0", vec![tc]),
            Message::tool_result("call_c1", "3"),
            Message::user("u1"),
            Message::assistant("a1"),
        ];
        *Arc::make_mut(&mut rt.transcript) = msgs;

        // compact_partial_before at index 3 (u1). safe_split_point backs up
        // past Tool at index 2 in scope[..=3] → split=2. Older [0..2] compacted.
        rt.compact_partial_before(3).await.unwrap();

        // After: [summary, u1, a1] — no orphan Tool messages.
        assert_eq!(rt.transcript().len(), 3, "summary + 2 kept");
        assert!(rt.transcript()[0].content.starts_with("[compacted:"));
        assert_eq!(rt.transcript()[1].content, "u1");
        assert_eq!(rt.transcript()[2].content, "a1");
        // No orphan Tool messages in the kept region
        for msg in &rt.transcript()[1..] {
            if msg.role == crate::message::Role::Tool {
                panic!("orphan Tool message in kept region: {:?}", msg);
            }
        }
    }

    #[tokio::test]
    async fn compact_partial_noop_without_compactor() {
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "x".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let mut rt = AgentRuntime::builder().llm(provider).build().unwrap();
        rt.set_transcript(vec![Message::user("hi"), Message::assistant("ok")]);
        let before = rt.transcript().len();
        rt.compact_partial_before(1).await.unwrap();
        assert_eq!(rt.transcript().len(), before);
        rt.compact_partial_after(0).await.unwrap();
        assert_eq!(rt.transcript().len(), before);
    }

    // ── Goal-168: GoalState / GoalEvaluator / run_goal_loop tests ──────────

    #[tokio::test]
    async fn set_goal_stores_state() {
        let llm = Arc::new(MockProvider::new(vec![]));
        let rt = AgentRuntime::builder().llm(llm).build().unwrap();
        assert!(rt.current_goal().is_none());
        rt.set_goal("task is done".to_string(), 10).await;
        let g = rt.current_goal().expect("goal should be set");
        assert_eq!(g.condition, "task is done");
        assert_eq!(g.max_turns, 10);
        assert_eq!(g.turns, 0);
        assert_eq!(g.status, GoalStatus::Pursuing);
    }

    #[tokio::test]
    async fn clear_goal_removes_state() {
        let llm = Arc::new(MockProvider::new(vec![]));
        let rt = AgentRuntime::builder().llm(llm).build().unwrap();
        rt.set_goal("anything".to_string(), 5).await;
        assert!(rt.current_goal().is_some());
        rt.clear_goal().await;
        assert!(rt.current_goal().is_none());
    }

    #[tokio::test]
    async fn goal_status_default_is_pursuing() {
        let g = GoalState {
            condition: "done".to_string(),
            status: GoalStatus::Pursuing,
            turns: 0,
            max_turns: 20,
            last_reason: None,
        };
        assert_eq!(g.status, GoalStatus::Pursuing);
        assert_eq!(g.turns, 0);
        assert!(g.last_reason.is_none());
    }

    #[tokio::test]
    async fn goal_evaluator_returns_achieved_on_yes_response() {
        // Mock a provider that returns "YES\nLooks complete."
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "YES\nLooks complete.".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let evaluator = GoalEvaluator::new(llm);
        let msgs = vec![crate::message::Message::user("I completed the task.")];
        let verdict = evaluator
            .evaluate("task is done", &msgs)
            .await
            .expect("evaluate should succeed");
        assert!(verdict.achieved);
        assert!(!verdict.reason.is_empty());
    }

    #[tokio::test]
    async fn goal_evaluator_returns_not_achieved_on_no_response() {
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "NO\nStill in progress.".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let evaluator = GoalEvaluator::new(llm);
        let msgs = vec![crate::message::Message::user("I started the task.")];
        let verdict = evaluator
            .evaluate("task is done", &msgs)
            .await
            .expect("evaluate should succeed");
        assert!(!verdict.achieved);
    }

    #[tokio::test]
    async fn goal_evaluator_tolerates_empty_transcript() {
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "YES\nEmpty transcript but condition trivially met.".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let evaluator = GoalEvaluator::new(llm);
        let verdict = evaluator
            .evaluate("anything", &[])
            .await
            .expect("should not error on empty transcript");
        assert!(verdict.achieved);
    }

    #[tokio::test]
    async fn run_goal_loop_stops_when_achieved() {
        // Provider: first call for the agent turn, second for the judge.
        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "I wrote the greeting.".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "YES\nGreeting was written.".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));
        let null_sink = Arc::new(crate::event::NullSink);
        let mut rt = AgentRuntime::builder()
            .llm(llm)
            .event_sink(null_sink)
            .build()
            .unwrap();
        let _ = rt
            .run_goal_loop("write a greeting", "write a greeting", 5)
            .await;
        // Goal should be cleared after achievement.
        assert!(rt.current_goal().is_none());
    }

    #[tokio::test]
    async fn run_goal_loop_stops_at_max_turns() {
        // Provider: every judge call returns NO → loop hits max_turns.
        let completions: Vec<Completion> = (0..20)
            .flat_map(|_| {
                vec![
                    Completion {
                        content: "still working".into(),
                        tool_calls: vec![],
                        finish_reason: Some("stop".into()),
                        usage: None,
                        reasoning_content: None,
                    },
                    Completion {
                        content: "NO\nNot done yet.".into(),
                        tool_calls: vec![],
                        finish_reason: Some("stop".into()),
                        usage: None,
                        reasoning_content: None,
                    },
                ]
            })
            .collect();
        let llm = Arc::new(MockProvider::new(completions));
        let null_sink = Arc::new(crate::event::NullSink);
        let mut rt = AgentRuntime::builder()
            .llm(llm)
            .event_sink(null_sink)
            .build()
            .unwrap();
        // max_turns=2 so we stop after 2 regardless.
        let _ = rt
            .run_goal_loop("start on impossible task", "impossible task", 2)
            .await;
        // Goal should be cleared after budget exhaustion.
        assert!(rt.current_goal().is_none());
    }

    #[tokio::test]
    async fn goal_serde_round_trip() {
        let g = GoalState {
            condition: "file written".to_string(),
            status: GoalStatus::Achieved,
            turns: 3,
            max_turns: 10,
            last_reason: Some("File was created.".to_string()),
        };
        let json = serde_json::to_string(&g).expect("serialize");
        let g2: GoalState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(g2.condition, g.condition);
        assert_eq!(g2.status, GoalStatus::Achieved);
        assert_eq!(g2.turns, 3);
        assert_eq!(g2.last_reason, Some("File was created.".to_string()));
    }

    #[tokio::test]
    async fn multiple_set_goal_calls_overwrite_state() {
        let llm = Arc::new(MockProvider::new(vec![]));
        let rt = AgentRuntime::builder().llm(llm).build().unwrap();
        rt.set_goal("first goal".to_string(), 5).await;
        rt.set_goal("second goal".to_string(), 15).await;
        let g = rt.current_goal().unwrap();
        assert_eq!(g.condition, "second goal");
        assert_eq!(g.max_turns, 15);
    }

    // ── Goal-260: transcript_tail accessor ───────────────────────────────

    #[test]
    fn transcript_tail_returns_full_when_n_exceeds_len() -> Result<(), Box<dyn std::error::Error>> {
        let llm = Arc::new(MockProvider::new(vec![]));
        let mut rt = AgentRuntime::builder().llm(llm).build()?;
        // Build a 3-message transcript directly (no LLM calls).
        rt.set_transcript(vec![
            crate::message::Message::user("one"),
            crate::message::Message::assistant("two"),
            crate::message::Message::user("three"),
        ]);
        let tail = rt.transcript_tail(10);
        assert_eq!(tail.len(), 3, "n > len should return the full transcript");
        assert_eq!(tail[0].content, "one");
        assert_eq!(tail[2].content, "three");
        Ok(())
    }

    #[test]
    fn transcript_tail_returns_last_n() -> Result<(), Box<dyn std::error::Error>> {
        let llm = Arc::new(MockProvider::new(vec![]));
        let mut rt = AgentRuntime::builder().llm(llm).build()?;
        rt.set_transcript(vec![
            crate::message::Message::user("m0"),
            crate::message::Message::assistant("m1"),
            crate::message::Message::user("m2"),
            crate::message::Message::assistant("m3"),
            crate::message::Message::user("m4"),
        ]);
        let tail = rt.transcript_tail(2);
        assert_eq!(tail.len(), 2, "should return exactly the last 2 messages");
        assert_eq!(tail[0].content, "m3");
        assert_eq!(tail[1].content, "m4");
        Ok(())
    }

    #[test]
    fn transcript_tail_handles_zero() -> Result<(), Box<dyn std::error::Error>> {
        let llm = Arc::new(MockProvider::new(vec![]));
        let mut rt = AgentRuntime::builder().llm(llm).build()?;
        rt.set_transcript(vec![
            crate::message::Message::user("only"),
            crate::message::Message::assistant("reply"),
        ]);
        let tail = rt.transcript_tail(0);
        assert_eq!(tail.len(), 0, "n == 0 should return an empty slice");
        assert!(tail.is_empty());
        Ok(())
    }

    // ── Goal-291: configurable goal_eval_transcript_tail ──────────────────
    //
    // The `goal_eval_transcript_tail` field replaces the old
    // `GOAL_EVAL_TRANSCRIPT_TAIL` constant. We verify three things:
    //   1. The builder field is wired through to the runtime.
    //   2. The default stays at 12 (backward-compatible with sessions that
    //      don't set the field).
    //   3. The configured value is honored, not silently overwritten by
    //      the old constant.
    #[test]
    fn goal_eval_transcript_tail_builder_default_is_twelve() {
        let llm = Arc::new(MockProvider::new(vec![]));
        let rt = AgentRuntime::builder().llm(llm).build().unwrap();
        // The default is 12 — same value the old constant held.
        assert_eq!(rt.goal_eval_transcript_tail, 12);
    }

    #[test]
    fn goal_eval_transcript_tail_builder_override_propagates() {
        let llm = Arc::new(MockProvider::new(vec![]));
        let rt = AgentRuntime::builder()
            .llm(llm)
            .goal_eval_transcript_tail(3)
            .build()
            .unwrap();
        assert_eq!(rt.goal_eval_transcript_tail, 3);
    }

    /// Source-level check: with `goal_eval_transcript_tail = 3` and 6
    /// messages in the transcript, `transcript_tail(n)` returns 3 — the
    /// value the runtime would pass to the judge. Verifies the value
    /// is honored, not silently overwritten by the old constant.
    #[test]
    fn goal_eval_transcript_tail_honored_over_old_default() {
        let llm = Arc::new(MockProvider::new(vec![]));
        let mut rt = AgentRuntime::builder()
            .llm(llm)
            .goal_eval_transcript_tail(3)
            .build()
            .unwrap();
        // 6 messages: u0, a0, u1, a1, u2, a2.
        rt.set_transcript(vec![
            crate::message::Message::user("m0"),
            crate::message::Message::assistant("m1"),
            crate::message::Message::user("m2"),
            crate::message::Message::assistant("m3"),
            crate::message::Message::user("m4"),
            crate::message::Message::assistant("m5"),
        ]);
        // The judge slice should be exactly 3 (the configured value),
        // not 6 (full transcript) and not 12 (old default).
        let judge_slice = rt.transcript_tail(rt.goal_eval_transcript_tail);
        assert_eq!(
            judge_slice.len(),
            3,
            "judge should see only 3 messages, got {}",
            judge_slice.len()
        );
        assert_eq!(judge_slice[0].content, "m3");
        assert_eq!(judge_slice[1].content, "m4");
        assert_eq!(judge_slice[2].content, "m5");
    }

    /// End-to-end check: with `goal_eval_transcript_tail = 1` the loop
    /// runs to completion (no panic, no wrong-tail-length error) using
    /// the configured tail size. This validates the wiring change in
    /// `run_goal_loop` — it now reads from the field, not the constant.
    #[tokio::test]
    async fn run_goal_loop_respects_tail_config() {
        use crate::event::ChannelSink;

        let completions = vec![
            // First turn: agent reply
            Completion {
                content: "still working".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
            // First judge call: NO
            Completion {
                content: "NO\nNot done yet.".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
            // Second turn: agent reply
            Completion {
                content: "trying again".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
            // Second judge call: YES — loop should exit here
            Completion {
                content: "YES\nAll good.".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ];
        let llm = Arc::new(MockProvider::new(completions));
        let (sink, _rx) = ChannelSink::new();
        let mut rt = AgentRuntime::builder()
            .llm(llm)
            .event_sink(Arc::new(sink))
            .goal_eval_transcript_tail(1)
            .build()
            .unwrap();

        // With tail=1, the judge sees only the most recent message per
        // call. This test just verifies the loop runs to completion
        // (no panic, no wrong-tail-length error) using the configured
        // tail size.
        let _ = rt
            .run_goal_loop("achieve it", "achieve it", 5)
            .await
            .expect("goal loop should run without error");
        // Goal is cleared on achievement.
        assert!(rt.current_goal().is_none());
        assert_eq!(rt.goal_eval_transcript_tail, 1);
    }

    // ── Goal-181: message queue ───────────────────────────────────────────

    #[tokio::test]
    async fn enqueue_processes_single_message() {
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "queued reply".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();
        let out = rt.enqueue("hello from queue").await.unwrap();
        assert!(out.is_some());
        assert_eq!(out.unwrap().final_text.as_deref(), Some("queued reply"));
        assert_eq!(rt.transcript().len(), 2);
    }

    #[tokio::test]
    async fn enqueue_drains_multiple_messages_in_order() {
        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "reply A".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "reply B".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));
        let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();
        // Push two messages directly into the queue to simulate concurrent enqueue.
        rt.message_queue.push_back("msg A".into());
        rt.message_queue.push_back("msg B".into());
        let last = rt.drain_queue().await.unwrap();
        assert_eq!(last.unwrap().final_text.as_deref(), Some("reply B"));
        // Both user messages + both assistant replies are in transcript.
        assert_eq!(rt.transcript().len(), 4);
    }

    #[test]
    fn queue_len_reflects_pending_messages() {
        let llm = Arc::new(MockProvider::new(vec![]));
        let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();
        assert_eq!(rt.queue_len(), 0);
        rt.message_queue.push_back("pending".into());
        assert_eq!(rt.queue_len(), 1);
        rt.message_queue.push_back("also pending".into());
        assert_eq!(rt.queue_len(), 2);
    }

    // ── Goal-244: drain_queue error propagation ──

    #[tokio::test]
    async fn drain_queue_returns_ok_for_all_messages() {
        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "reply A".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "reply B".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));
        let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();
        rt.message_queue.push_back("msg A".into());
        rt.message_queue.push_back("msg B".into());
        let result = rt.drain_queue().await;
        assert!(result.is_ok());
        let last = result.unwrap();
        assert!(last.is_some());
        assert_eq!(last.unwrap().final_text.as_deref(), Some("reply B"));
        // Both user messages + both assistant replies are in transcript.
        assert_eq!(rt.transcript().len(), 4);
    }

    #[tokio::test]
    async fn drain_queue_stops_on_first_error() {
        // Only one completion available — second message will fail.
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "reply A".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();
        rt.message_queue.push_back("msg A".into());
        rt.message_queue.push_back("msg B".into());
        let result = rt.drain_queue().await;
        assert!(result.is_err(), "expected error, got {:?}", result);
        // Goal-259: the in-flight message must remain at the front of the
        // queue so it can be retried by calling drain_queue again.
        assert_eq!(
            rt.queue_len(),
            1,
            "second message should remain in queue for retry"
        );
        // Verify it is indeed the second message that was preserved.
        assert_eq!(
            rt.message_queue.front().map(String::as_str),
            Some("msg B"),
            "msg B should still be at the front of the queue"
        );
        // First message was successfully processed and is reflected in the
        // transcript (user message + assistant reply).
        assert_eq!(
            rt.transcript().len(),
            3,
            "transcript should hold msg A, reply A, and the in-flight msg B"
        );
    }

    #[tokio::test]
    async fn drain_queue_preserves_remaining_messages_on_error() {
        // Goal-259: 3 messages queued, only 1 completion available. The
        // second message will fail. The first message must be popped
        // (success), and the remaining two (B and C) must stay in the
        // queue for later retry.
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "reply A".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();
        rt.message_queue.push_back("msg A".into());
        rt.message_queue.push_back("msg B".into());
        rt.message_queue.push_back("msg C".into());
        let result = rt.drain_queue().await;
        assert!(result.is_err(), "expected error, got {:?}", result);
        // First message was processed and popped; B and C remain.
        assert_eq!(
            rt.queue_len(),
            2,
            "B and C should remain in queue for retry"
        );
        // FIFO order preserved: B at the front, C behind it.
        assert_eq!(rt.message_queue.front().map(String::as_str), Some("msg B"));
        // First turn reflected in transcript. The in-flight msg B is also
        // present (run() appends the user message to the transcript before
        // the LLM call), but it has no assistant reply yet because the
        // LLM call failed — the same pre-existing behaviour as in
        // drain_queue_stops_on_first_error.
        assert_eq!(rt.transcript().len(), 3);
    }

    // ── Goal-201: plan mode tools are registered by the runtime builder ──

    #[test]
    fn runtime_builder_skills_stores_skills_list() {
        // kills `replace AgentRuntimeBuilder::skills -> Self with Default::default()`:
        // if skills() discards the argument, the runtime's globs_skills would be empty.
        use crate::skills::{Skill, SkillMode};
        let llm = Arc::new(MockProvider::new(vec![]));
        let skill = Skill {
            name: "my-skill".to_string(),
            description: "A test skill".to_string(),
            path: std::path::PathBuf::from("/tmp/my-skill/SKILL.md"),
            mode: SkillMode::Always,
            triggers: vec![],
            hint: String::new(),
            depends_on: vec![],
            refs: vec![],
            params: vec![],
            scripts: vec![],
            sections: vec![],
            globs: None,
        };
        let rt = AgentRuntime::builder()
            .llm(llm)
            .skills(vec![skill])
            .build()
            .unwrap();
        // `globs_skills` is pub(crate); it must contain the skill we passed.
        assert_eq!(
            rt.kernel.globs_skills.len(),
            1,
            "skills() must store the provided skills list; len was {}",
            rt.kernel.globs_skills.len()
        );
        assert_eq!(rt.kernel.globs_skills[0].name, "my-skill");
    }

    #[test]
    fn reject_plan_appends_rejection_message_to_transcript() {
        // kills `replace AgentRuntime::reject_plan with ()` mutation.
        // If reject_plan is a no-op, the transcript won't grow.
        let llm = Arc::new(MockProvider::new(vec![]));
        let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();
        let before = rt.transcript_tail(100).len();
        rt.reject_plan("too risky");
        let after = rt.transcript_tail(100).len();
        assert!(
            after > before,
            "reject_plan must append a rejection message to the transcript"
        );
        // Verify the message contains the reason
        let tail = rt.transcript_tail(100);
        let last = tail.last().expect("at least one message");
        assert!(
            last.content.contains("too risky"),
            "rejection message must contain the provided reason; got: {:?}",
            last.content
        );
    }

    #[test]
    fn runtime_builder_has_plan_mode_tools() {
        // AgentRuntimeBuilder::build() must register enter_plan_mode and
        // exit_plan_mode when with_plan_mode_tools(true) is set.
        // These are channel capabilities used by the TUI and HTTP paths.
        let llm = Arc::new(MockProvider::new(vec![]));
        let rt = AgentRuntime::builder()
            .llm(llm)
            .with_plan_mode_tools(true)
            .build()
            .unwrap();
        let tools = rt.kernel.tools();
        assert!(
            tools.get(ENTER_PLAN_MODE_TOOL_NAME).is_some(),
            "enter_plan_mode must be registered by AgentRuntimeBuilder"
        );
        assert!(
            tools.get(EXIT_PLAN_MODE_TOOL_NAME).is_some(),
            "exit_plan_mode must be registered by AgentRuntimeBuilder"
        );
    }

    // ── Goal-275: tool_audits keyed by (turn, tool_call_id) ──────────────

    /// When two turns reuse the same `tool_call_id`, the new `(turn, id)`
    /// keying prevents the second turn's audit from overwriting the first
    /// turn's audit before it can be emitted.
    #[tokio::test]
    async fn audit_survives_collision_across_turns() {
        let llm = Arc::new(MockProvider::new(vec![
            // Turn 1: tool call "c1" (adder)
            Completion {
                content: "calculating...".into(),
                tool_calls: vec![crate::llm::ToolCall {
                    id: "c1".into(),
                    name: "add".into(),
                    arguments: json!({"a": 1, "b": 2}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
            // Turn 1: finish
            Completion {
                content: "3".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
            // Turn 2: tool call "c1" (SAME id reused)
            Completion {
                content: "calculating again...".into(),
                tool_calls: vec![crate::llm::ToolCall {
                    id: "c1".into(),
                    name: "add".into(),
                    arguments: json!({"a": 5, "b": 7}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
            // Turn 2: finish
            Completion {
                content: "12".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));
        let tools = ToolRegistry::local().register(Arc::new(Adder));
        let (sink, mut rx) = crate::event::ChannelSink::new();
        let sink_arc = Arc::new(sink);
        let mut rt = AgentRuntime::builder()
            .llm(llm)
            .tools(tools)
            .event_sink(sink_arc)
            .build()
            .unwrap();

        // Drain events from builder registration.
        while let Ok(_ev) = rx.try_recv() {}

        let _ = rt.run("turn 1").await.unwrap();
        let _ = rt.run("turn 2").await.unwrap();

        let mut audit_count = 0usize;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, AgentEvent::MessageAppendedWithAudit { .. }) {
                audit_count += 1;
            }
        }

        // Both turns should produce a tool result with audit metadata.
        // Without (turn, id) keying, turn-2's audit overwrites turn-1's
        // entry before emit_turn_messages processes either, so
        // audit_count would be 1 instead of 2.
        assert_eq!(
            audit_count, 2,
            "expected both turns' tool results to have audit metadata"
        );
    }

    /// A buggy model that emits the same `tool_call_id` twice in a single
    /// assistant message.  The `remove()` semantics mean only the first
    /// tool-result message gets the audit, but at least it gets *one*.
    /// Before the (turn, id) keying fix, cross-turn collisions could
    /// nuke even this one.
    #[tokio::test]
    async fn duplicate_tool_call_id_in_same_response_attaches_at_least_one() {
        let llm = Arc::new(MockProvider::new(vec![
            // Turn 1: two tool calls, both with id "c1"
            Completion {
                content: "doing two things...".into(),
                tool_calls: vec![
                    crate::llm::ToolCall {
                        id: "c1".into(),
                        name: "add".into(),
                        arguments: json!({"a": 1, "b": 2}),
                    },
                    crate::llm::ToolCall {
                        id: "c1".into(), // duplicate id
                        name: "add".into(),
                        arguments: json!({"a": 3, "b": 4}),
                    },
                ],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            },
            // Turn 1: finish
            Completion {
                content: "done".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));
        let tools = ToolRegistry::local().register(Arc::new(Adder));
        let (sink, mut rx) = crate::event::ChannelSink::new();
        let sink_arc = Arc::new(sink);
        let mut rt = AgentRuntime::builder()
            .llm(llm)
            .tools(tools)
            .event_sink(sink_arc)
            .build()
            .unwrap();

        while let Ok(_ev) = rx.try_recv() {}

        let _ = rt.run("do it").await.unwrap();

        let mut audit_count = 0usize;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, AgentEvent::MessageAppendedWithAudit { .. }) {
                audit_count += 1;
            }
        }

        // At least one of the two tool results should carry audit metadata.
        assert!(
            audit_count >= 1,
            "expected at least one tool result to have audit metadata, got {audit_count}"
        );
    }

    // ── Goal-285: DENIAL_LIMIT_SENTINEL double-push regression ──────────

    /// When a batch of tool calls includes a `DENIAL_LIMIT_SENTINEL` as the
    /// second result, the transcript must have exactly N tool-result messages
    /// — not N+duplicates. The pre-Goal-285 code pushed earlier non-sentinel
    /// results twice (once in the outer loop, once in the sentinel inner loop),
    /// violating Invariant #8 (unique tool-call ↔ tool-result pairing).
    #[tokio::test]
    async fn denial_limit_sentinel_no_duplicate_pushes() {
        use crate::error::Error;

        struct DenialTool;

        #[async_trait]
        impl Tool for DenialTool {
            fn spec(&self) -> crate::llm::ToolSpec {
                crate::llm::ToolSpec {
                    name: "denial_tool".into(),
                    description: "always triggers permission denial limit".into(),
                    parameters: json!({"type": "object", "properties": {}}),
                }
            }
            async fn execute(&self, _args: Value) -> crate::error::Result<String> {
                Err(Error::PermissionDeniedLimit {
                    name: "denial_tool".into(),
                })
            }
        }

        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "Let me try two things...".into(),
            tool_calls: vec![
                crate::llm::ToolCall {
                    id: "c1".into(),
                    name: "add".into(),
                    arguments: json!({"a": 1, "b": 2}),
                },
                crate::llm::ToolCall {
                    id: "c2".into(),
                    name: "denial_tool".into(),
                    arguments: json!({}),
                },
            ],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        }]));

        let tools = ToolRegistry::local()
            .register(Arc::new(Adder))
            .register(Arc::new(DenialTool));

        let mut rt = AgentRuntime::builder()
            .llm(llm)
            .tools(tools)
            .build()
            .unwrap();

        let out = rt.run("test").await.unwrap();

        // Verify finish reason
        assert_eq!(out.finish_reason, FinishReason::PermissionDenialLimit);

        // Count tool-result messages in transcript.
        // Should have exactly 2 (add + denial_tool), NOT 3.
        let tool_msgs: Vec<_> = rt
            .transcript()
            .iter()
            .filter(|m| m.role == crate::message::Role::Tool)
            .collect();

        assert_eq!(
            tool_msgs.len(),
            2,
            "expected exactly 2 tool-result messages, got {} (double-push bug?)",
            tool_msgs.len()
        );

        // The first tool result ("add") must appear exactly once.
        let add_count = tool_msgs
            .iter()
            .filter(|m| m.tool_call_id.as_deref() == Some("c1"))
            .count();
        assert_eq!(
            add_count, 1,
            "add result (c1) should appear exactly once, got {add_count}"
        );

        // The denial tool result must also appear exactly once.
        let denial_count = tool_msgs
            .iter()
            .filter(|m| m.tool_call_id.as_deref() == Some("c2"))
            .count();
        assert_eq!(
            denial_count, 1,
            "denial result (c2) should appear exactly once, got {denial_count}"
        );

        // Total transcript messages: user(1) + assistant(1) + 2 tool results = 4
        assert_eq!(
            rt.transcript().len(),
            4,
            "transcript should have 4 messages (user, assistant, 2× tool), got {}",
            rt.transcript().len()
        );
    }

    /// Invariant #8 regression: when stuck detection fires *mid-batch* (the
    /// error rate threshold is reached while iterating the results of a
    /// multi-call step), the turn must still push a tool_result for EVERY
    /// tool_call of the triggering assistant message. The old code returned
    /// from inside the result loop before pushing the remaining results,
    /// leaving orphaned `tool_use` blocks in the committed transcript — which
    /// the provider then rejects on every subsequent turn with HTTP 400
    /// ("tool_use ids ... were found without tool_result blocks").
    #[tokio::test]
    async fn stuck_detection_keeps_tool_calls_paired() {
        use crate::error::Error;

        struct AlwaysFails;

        #[async_trait]
        impl Tool for AlwaysFails {
            fn spec(&self) -> crate::llm::ToolSpec {
                crate::llm::ToolSpec {
                    name: "always_fails".into(),
                    description: "always returns an error".into(),
                    parameters: json!({"type": "object", "properties": {}}),
                }
            }
            async fn execute(&self, _args: Value) -> crate::error::Result<String> {
                Err(Error::Tool {
                    name: "always_fails".into(),
                    call_id: None,
                    message: "boom".into(),
                })
            }
        }

        // One assistant message with three failing tool_calls. With
        // stuck_window=2 and stuck_error_rate=1.0, the second error trips
        // the stuck threshold — mid-batch, before the third result is
        // processed.
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "Trying three things at once...".into(),
            tool_calls: vec![
                crate::llm::ToolCall {
                    id: "c1".into(),
                    name: "always_fails".into(),
                    arguments: json!({}),
                },
                crate::llm::ToolCall {
                    id: "c2".into(),
                    name: "always_fails".into(),
                    arguments: json!({}),
                },
                crate::llm::ToolCall {
                    id: "c3".into(),
                    name: "always_fails".into(),
                    arguments: json!({}),
                },
            ],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        }]));

        let tools = ToolRegistry::local().register(Arc::new(AlwaysFails));

        let mut rt = AgentRuntime::builder()
            .llm(llm)
            .tools(tools)
            .stuck_window(2)
            .stuck_error_rate(1.0)
            .build()
            .unwrap();

        let out = rt.run("go").await.unwrap();

        // The turn ends as Stuck (error rate hit the threshold).
        assert!(
            matches!(out.finish_reason, FinishReason::Stuck { .. }),
            "expected Stuck finish, got {:?}",
            out.finish_reason
        );

        // Every one of the assistant's three tool_calls must have a matching
        // tool_result, even though stuck fired after the second.
        let assistant = rt
            .transcript()
            .iter()
            .find(|m| m.role == crate::message::Role::Assistant && !m.tool_calls.is_empty())
            .expect("assistant-with-tool_calls must be in transcript");
        let tool_results: Vec<&str> = rt
            .transcript()
            .iter()
            .filter(|m| m.role == crate::message::Role::Tool)
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        for tc in &assistant.tool_calls {
            assert!(
                tool_results.contains(&tc.id.as_str()),
                "tool_call {} has no matching tool_result (orphaned tool_use); results={tool_results:?}",
                tc.id
            );
        }
        assert_eq!(
            tool_results.len(),
            3,
            "expected exactly 3 tool_result messages, got {}",
            tool_results.len()
        );
    }

    /// Goal 287 / Goal 288: verify that LLM errors propagate correctly.
    /// After Goal 288 removed the outer retry loop, the provider's internal
    /// `RetryPolicy` handles retries. MockProvider does not retry internally,
    /// so a `RateLimited` error surfaces immediately as a run error.
    #[tokio::test]
    async fn llm_retry_emits_event() {
        use crate::event::ChannelSink;

        let (sink, _event_rx) = ChannelSink::new();
        let sink = Arc::new(sink);

        // MockProvider returns a RateLimited error — without an outer retry
        // loop, this propagates to the caller.
        let provider = Arc::new(
            MockProvider::new(vec![Completion {
                content: "Hello!".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            }])
            .with_errors(vec![crate::error::Error::RateLimited {
                provider: "mock".into(),
                retry_after_ms: 1,
            }]),
        );

        let mut rt = AgentRuntime::builder()
            .llm(provider)
            .event_sink(sink)
            .build()
            .unwrap();

        // The error should propagate — retry is now handled at the provider
        // layer via `RetryPolicy`, not in `run_core`.
        let result = rt.run("hi").await;
        assert!(
            result.is_err(),
            "expected error from RateLimited MockProvider without outer retry loop"
        );
        let err = result.unwrap_err();
        let err_str = format!("{err}");
        assert!(
            err_str.contains("rate limited"),
            "expected rate-limited error, got: {err_str}"
        );
    }

    // ── P0-2: set_event_sink / replace_event_sink side-effect contract ────

    /// `replace_event_sink` swaps the runtime sink but must NOT touch the
    /// tool registry. This pins down the explicit non-side-effect path —
    /// callers that only want to redirect `MessageAppended` / `TurnFinished`
    /// events without triggering `TodoWriteTool` / `ExitPlanModeTool`
    /// re-registration can use this.
    #[tokio::test]
    async fn replace_event_sink_does_not_reregister_tools() {
        let llm = Arc::new(MockProvider::new(vec![]));
        let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();

        // Capture the pre-swap TodoWriteTool Arc identity.
        let pre_todo = rt
            .kernel
            .tools()
            .get("TodoWrite")
            .expect("TodoWrite is in the default registry")
            .clone();

        let (new_sink, _rx) = crate::event::ChannelSink::new();
        rt.replace_event_sink(Arc::new(new_sink));

        // The registry's TodoWriteTool identity is unchanged — no re-register.
        let post_todo = rt
            .kernel
            .tools()
            .get("TodoWrite")
            .expect("TodoWrite still registered")
            .clone();
        assert!(
            Arc::ptr_eq(&pre_todo, &post_todo),
            "replace_event_sink must not re-register TodoWriteTool"
        );
    }

    /// `set_event_sink` keeps its existing side effect: re-registering
    /// TodoWriteTool so it points to the new sink. This is the contract
    /// every caller (CLI per-turn, HTTP per-session, TUI on backend init)
    /// depends on; if you intentionally change it, also audit those callers.
    #[tokio::test]
    async fn set_event_sink_reregisters_todo_write_tool() {
        let llm = Arc::new(MockProvider::new(vec![]));
        let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();

        let pre_todo = rt
            .kernel
            .tools()
            .get("TodoWrite")
            .expect("TodoWrite registered")
            .clone();

        let (new_sink, _rx) = crate::event::ChannelSink::new();
        rt.set_event_sink(Arc::new(new_sink));

        let post_todo = rt
            .kernel
            .tools()
            .get("TodoWrite")
            .expect("TodoWrite still registered after set_event_sink")
            .clone();
        assert!(
            !Arc::ptr_eq(&pre_todo, &post_todo),
            "set_event_sink MUST re-register TodoWriteTool — this side effect is \
             load-bearing for the TUI/CLI/HTTP sink-swap flows; removing it would \
             silently drop TodoUpdated events on the new sink."
        );
    }

    // ── is_context_window_exceeded ──────────────────────────────────────────

    #[test]
    fn context_overflow_detector_matches_known_patterns() {
        let cases = [
            // OpenAI / NVIDIA NIM error code
            "HTTP 400: {\"error\":{\"code\":\"context_length_exceeded\",\"message\":\"too long\"}}",
            // OpenAI human message
            "HTTP 400: This model's maximum context length is 200000 tokens, however you requested 201234",
            // Generic phrasing
            "HTTP 400: prompt is too long for the model",
            "HTTP 400: tokens exceeds model limit",
            "HTTP 400: exceeds the model context window",
        ];
        for msg in &cases {
            let err = crate::error::Error::Llm {
                provider: "test".into(),
                message: msg.to_string(),
            };
            assert!(
                is_context_window_exceeded(&err),
                "should detect context overflow in: {msg}"
            );
        }
    }

    #[test]
    fn context_overflow_detector_ignores_unrelated_errors() {
        let cases = [
            "HTTP 400: invalid request body",
            "HTTP 401: unauthorized",
            "HTTP 429: rate limit exceeded",
            "network error: connection refused",
        ];
        for msg in &cases {
            let err = crate::error::Error::Llm {
                provider: "test".into(),
                message: msg.to_string(),
            };
            assert!(
                !is_context_window_exceeded(&err),
                "should NOT detect context overflow in: {msg}"
            );
        }
    }

    #[test]
    fn context_overflow_detector_ignores_non_llm_errors() {
        let err = crate::error::Error::Timeout {
            duration_ms: 30_000,
        };
        assert!(!is_context_window_exceeded(&err));
        let err = crate::error::Error::Config {
            message: "context_length_exceeded".into(),
        };
        assert!(
            !is_context_window_exceeded(&err),
            "Config errors must not be detected even if they contain the keyword"
        );
    }

    // ── cross-turn microcompact (Goal 333) ──────────────────────────────────

    #[tokio::test]
    async fn cross_turn_microcompact_prunes_before_summary_check() {
        // Build a runtime with a Microcompactor (low trigger=2) and a Compactor
        // (char threshold high enough that the post-prune transcript would NOT
        // trigger the LLM summary). Seed a transcript with many tool results.
        // Verify: Microcompact event was emitted AND the compactor's LLM was
        // NOT called (summary skipped).
        let provider = Arc::new(MockProvider::new(vec![])); // should not be called
        let (sink, mut rx) = crate::event::ChannelSink::new();
        let sink_arc = Arc::new(sink);

        let mc = crate::compact::Microcompactor::new(2, 1); // trigger at 2
        let compactor = crate::compact::Compactor::new(usize::MAX); // never fires by char
        let mut rt = AgentRuntime::builder()
            .llm(provider)
            .microcompactor(mc)
            .compactor(compactor)
            .event_sink(sink_arc)
            .build()
            .unwrap();

        // Seed transcript with 5 tool-result messages (trigger=2, keep=1).
        let mut msgs: Vec<Message> = Vec::new();
        for i in 0..5 {
            msgs.push(Message::user(format!("user {i}")));
            msgs.push(Message::assistant(format!("asst {i}")));
            msgs.push(Message::tool_result(format!("call_{i}"), "x".repeat(300)));
        }
        *Arc::make_mut(&mut rt.transcript) = msgs;

        // Drain channel events from builder setup.
        while rx.try_recv().is_ok() {}

        // Call maybe_compact_cross_turn directly.
        rt.maybe_compact_cross_turn(&TokenUsage::default())
            .await
            .unwrap();

        // Check for Microcompact event.
        let mut microcompact_fired = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, AgentEvent::Microcompact { .. }) {
                microcompact_fired = true;
            }
        }

        assert!(
            microcompact_fired,
            "Microcompact event must be emitted when microcompactor prunes"
        );
    }

    #[tokio::test]
    async fn cross_turn_microcompact_disabled_when_none() {
        // No microcompactor configured → behavior identical to today (no Microcompact).
        let provider = Arc::new(MockProvider::new(vec![]));
        let (sink, mut rx) = crate::event::ChannelSink::new();
        let sink_arc = Arc::new(sink);

        // Build runtime with ONLY a compactor (threshold=MAX so it never fires),
        // but NO microcompactor.
        let compactor = crate::compact::Compactor::new(usize::MAX);
        let mut rt = AgentRuntime::builder()
            .llm(provider)
            .compactor(compactor)
            .event_sink(sink_arc)
            .build()
            .unwrap();

        // Seed transcript with many tool results.
        let mut msgs: Vec<Message> = Vec::new();
        for i in 0..5 {
            msgs.push(Message::user(format!("user {i}")));
            msgs.push(Message::assistant(format!("asst {i}")));
            msgs.push(Message::tool_result(format!("call_{i}"), "x".repeat(300)));
        }
        *Arc::make_mut(&mut rt.transcript) = msgs;

        while rx.try_recv().is_ok() {}

        // Call maybe_compact_cross_turn — it has no microcompactor, so the
        // microcompact block is skipped.
        rt.maybe_compact_cross_turn(&TokenUsage::default())
            .await
            .unwrap();

        let mut microcompact_fired = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, AgentEvent::Microcompact { .. }) {
                microcompact_fired = true;
            }
        }

        assert!(
            !microcompact_fired,
            "Microcompact must NOT fire when no microcompactor is configured"
        );
    }

    // ── compact_on_overflow ─────────────────────────────────────────────────

    #[tokio::test]
    async fn compact_on_overflow_compacts_long_transcript() {
        let summary_resp = Completion {
            content: "Summary of prior conversation.".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        };
        let llm = Arc::new(MockProvider::new(vec![summary_resp]));
        let mut rt = AgentRuntime::builder()
            .llm(llm)
            .compactor(crate::Compactor::new(usize::MAX))
            .build()
            .unwrap();

        // Populate a transcript long enough for compaction (> keep_recent_n + 2).
        let msgs: Vec<crate::message::Message> = (0..14)
            .map(|i| {
                if i % 2 == 0 {
                    crate::message::Message::user(format!("msg {i}"))
                } else {
                    crate::message::Message::assistant(format!("reply {i}"))
                }
            })
            .collect();
        *Arc::make_mut(&mut rt.transcript) = msgs;
        let before = rt.transcript.len();

        let compacted = rt.compact_on_overflow().await.unwrap();
        assert!(compacted, "should return true when compaction ran");
        assert!(
            rt.transcript.len() < before,
            "transcript must shrink after compaction"
        );
        assert_eq!(
            rt.transcript[0].role,
            crate::message::Role::System,
            "first message after compaction is the summary"
        );
    }

    #[tokio::test]
    async fn compact_on_overflow_returns_false_without_compactor() {
        let llm = Arc::new(MockProvider::new(vec![]));
        let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();
        let ok = rt.compact_on_overflow().await.unwrap();
        assert!(!ok, "no compactor → must return false");
    }

    #[tokio::test]
    async fn compact_on_overflow_rejects_degenerate_transcript_without_hook_events() {
        struct CompactionHookRecorder(Arc<std::sync::Mutex<Vec<&'static str>>>);

        impl crate::hooks::Hook for CompactionHookRecorder {
            fn on_event(&self, event: HookEvent) -> crate::hooks::HookAction {
                match event {
                    HookEvent::PreCompact { .. } => {
                        self.0.lock().unwrap().push("PreCompact");
                    }
                    HookEvent::PostCompact { .. } => {
                        self.0.lock().unwrap().push("PostCompact");
                    }
                    _ => {}
                }
                crate::hooks::HookAction::Continue
            }
        }

        let llm = Arc::new(MockProvider::new(vec![]));
        let hook_events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut hooks = HookRegistry::new();
        hooks.register(Arc::new(CompactionHookRecorder(hook_events.clone())));
        let mut rt = AgentRuntime::builder()
            .llm(llm.clone())
            .hooks(hooks)
            .compactor(Compactor::new(usize::MAX).keep_recent_n(8))
            .build()
            .unwrap();

        // The older slice contains only the system prompt, so it cannot be
        // summarized. This is long enough to reach the compaction guard.
        *Arc::make_mut(&mut rt.transcript) = vec![
            Message::system("System prompt".to_string()),
            Message::user("Add a feature".to_string()),
            Message::assistant("Working on it".to_string()),
            Message::user("Status?".to_string()),
            Message::assistant("Almost done".to_string()),
            Message::user("Run tests".to_string()),
            Message::assistant("Tests pass".to_string()),
            Message::user("Commit".to_string()),
            Message::assistant("Done".to_string()),
        ];

        assert!(
            !rt.compact_on_overflow().await.unwrap(),
            "a degenerate transcript must reject emergency compaction"
        );
        assert!(
            hook_events.lock().unwrap().is_empty(),
            "a rejected compaction must emit neither PreCompact nor PostCompact"
        );
        assert!(
            llm.calls().is_empty(),
            "a rejected compaction must not call the provider"
        );
    }

    /// Context-overflow error recovery integration test.
    ///
    /// Scenario:
    ///   1. Agent transcript is long (14 msgs, enough to compact).
    ///   2. First LLM call fails with a `context_length_exceeded` error.
    ///   3. `compact_on_overflow` fires: summarises older messages (uses
    ///      scripted completion [0] for the summary).
    ///   4. Retry uses scripted completion [1] and succeeds.
    #[tokio::test]
    async fn context_overflow_triggers_compact_and_retry() {
        let overflow_err = crate::error::Error::Llm {
            provider: "test-model".into(),
            message: "HTTP 400: {\"error\":{\"code\":\"context_length_exceeded\",\
                      \"message\":\"maximum context length is 200000 tokens\"}}"
                .into(),
        };
        let llm = Arc::new(
            MockProvider::new(vec![
                // [0] compaction summary
                Completion {
                    content: "Prior conversation summary for test.".into(),
                    tool_calls: vec![],
                    finish_reason: Some("stop".into()),
                    usage: None,
                    reasoning_content: None,
                },
                // [1] agent reply after successful retry
                Completion {
                    content: "Reply after emergency compaction.".into(),
                    tool_calls: vec![],
                    finish_reason: Some("stop".into()),
                    usage: None,
                    reasoning_content: None,
                },
            ])
            .with_errors(vec![overflow_err]),
        );

        let mut rt = AgentRuntime::builder()
            .llm(llm)
            .compactor(crate::Compactor::new(usize::MAX))
            .build()
            .unwrap();

        // Pre-populate transcript (> keep_recent_n + 2 = 10) so compaction can run.
        let msgs: Vec<Message> = (0..14)
            .map(|i| {
                if i % 2 == 0 {
                    Message::user(format!("prior msg {i}"))
                } else {
                    Message::assistant(format!("prior reply {i}"))
                }
            })
            .collect();
        *Arc::make_mut(&mut rt.transcript) = msgs;

        let outcome = rt.run("test: overflow recovery").await.unwrap();
        assert_eq!(
            outcome.final_text.as_deref(),
            Some("Reply after emergency compaction."),
            "run() must return the retry's reply"
        );

        // After recovery, the transcript's first message should be the compaction summary.
        assert_eq!(
            rt.transcript[0].role,
            crate::message::Role::System,
            "transcript must start with compaction summary after overflow recovery"
        );
        assert!(
            rt.transcript[0].is_compaction_summary,
            "summary message must be flagged as compaction_summary"
        );
    }

    // ── Goal-340: cross-turn compaction re-injects plan and todos ─────────

    #[tokio::test]
    async fn cross_turn_compaction_reinjects_plan_and_todos() {
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "Summary of prior conversation.".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));

        let mut rt = AgentRuntime::builder()
            .llm(provider)
            .compactor(crate::Compactor::new(0)) // always compact
            .build()
            .unwrap();

        // Seed pending plan via the runtime's shared plan gate.
        rt.plan_approval_gate
            .begin_approval("Step 1: explore\nStep 2: implement".to_string());

        // Seed todos via the runtime's shared todo list.
        {
            let mut todos = rt.todo_list.write().unwrap();
            todos.push(TodoItem {
                content: "Read files".to_string(),
                status: TodoStatus::Completed,
                active_form: None,
            });
            todos.push(TodoItem {
                content: "Edit code".to_string(),
                status: TodoStatus::InProgress,
                active_form: Some("Editing code...".to_string()),
            });
            todos.push(TodoItem {
                content: "Run tests".to_string(),
                status: TodoStatus::Pending,
                active_form: None,
            });
        }

        // Populate transcript long enough for compaction.
        let msgs: Vec<Message> = (0..14)
            .map(|i| {
                if i % 2 == 0 {
                    Message::user(format!("msg {i}"))
                } else {
                    Message::assistant(format!("reply {i}"))
                }
            })
            .collect();
        *Arc::make_mut(&mut rt.transcript) = msgs;

        rt.maybe_compact_cross_turn(&TokenUsage::default())
            .await
            .unwrap();

        let transcript = rt.transcript();

        // Transcript order: [summary, plan-att, todo-att, ...preserved]
        assert!(
            transcript.len() >= 3,
            "at least summary + 2 atts + preserved"
        );
        assert_eq!(transcript[0].role, crate::message::Role::System);
        assert!(
            transcript[0].is_compaction_summary,
            "first message is the compaction summary"
        );

        // Find the plan and todo attachments.
        let plan_msg = transcript
            .iter()
            .find(|m| m.content.starts_with("[post-compact plan restore]"))
            .expect("plan restore message must be present");

        assert!(plan_msg.content.contains("Step 1: explore"));
        assert!(plan_msg.content.contains("You are in plan mode"));

        let todo_msg = transcript
            .iter()
            .find(|m| m.content.starts_with("[post-compact todo restore]"))
            .expect("todo restore message must be present");

        assert!(todo_msg.content.contains("- [x] Read files"));
        assert!(todo_msg.content.contains("- [/] Edit code"));
        assert!(todo_msg.content.contains("(active: Editing code...)"));
        assert!(todo_msg.content.contains("- [ ] Run tests"));

        // Plan attachment should be before the todo attachment (plan first).
        let plan_idx = transcript
            .iter()
            .position(|m| m.content.starts_with("[post-compact plan restore]"))
            .unwrap();
        let todo_idx = transcript
            .iter()
            .position(|m| m.content.starts_with("[post-compact todo restore]"))
            .unwrap();
        assert!(
            plan_idx < todo_idx,
            "plan attachment must come before todo attachment"
        );
    }
}
