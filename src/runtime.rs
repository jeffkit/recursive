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

// Issue #99: bounded exponential-backoff retry for a failed loop turn.
mod loop_retry;
pub use loop_retry::LoopRetryPolicy;

// Goal-393: frontend-neutral context-management assembly (compactor /
// microcompactor / transcript cap), shared by the CLI and HTTP builders.
mod context_management;
pub use context_management::apply_context_management;

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
    /// Issue #31: environment teardown is once-per-runtime (mirrors
    /// `closed`); repeated `destroy_environment` is a no-op even for
    /// transports whose own destroy isn't idempotent.
    environment_destroyed: bool,
}

impl SessionLifecycle {
    fn open() -> Self {
        Self {
            closed: false,
            environment_destroyed: false,
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
    /// Accumulated conversation transcript (shared via Arc for O(1) clone
    /// when building TurnContext).
    transcript: Arc<Vec<Message>>,
    /// Event sink for streaming events (Arc for sharing with forwarder task).
    event_sink: Arc<dyn EventSink>,
    /// Goal M2-1: permission hook that flows into TurnContext so client
    /// tools (AG-UI bridge) get denied before dispatch and surface as
    /// interrupts to the frontend.
    permission_hook: Option<Arc<dyn crate::tools::PermissionHook>>,
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
    /// Issue #47④: optional bound on the `exit_plan_mode` approval wait for
    /// sink-swapping hosts (REPL). `None` keeps wait-forever semantics.
    approval_wait_timeout_secs: Option<u64>,
    /// Issue #48 / Goal 409: the surface's per-turn interrupt token, mirrored
    /// here so `set_event_sink` can re-register `ExitPlanModeTool` *with* the
    /// token on every swap. Without the mirror, the REPL's per-turn
    /// `set_event_sink` call would replace the token-carrying tool with a
    /// tokenless one and the approval wait would become uncancellable again.
    plan_approval_interrupt_token: Option<tokio_util::sync::CancellationToken>,
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
    /// Issue #99: retry budget for a loop turn that failed with a transient
    /// error (see [`LoopRetryPolicy`]). Only `run_loop` uses it.
    loop_retry: LoopRetryPolicy,
    /// Issue #99: session directory the pending wakeup is persisted into, so
    /// a restart can restore it instead of dropping it. `None` disables
    /// persistence (no session recording, or a host that keeps loop state in
    /// memory only). Set via [`AgentRuntimeBuilder::wakeup_store_dir`].
    wakeup_store_dir: Option<std::path::PathBuf>,
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

        match self.drive_turn().await {
            Ok(outcome) => Ok(outcome),
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
                    self.drive_turn().await
                } else {
                    Err(e)
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Execute the turn already staged in the transcript, then do the
    /// post-turn bookkeeping: emit the turn's messages, run cross-turn
    /// compaction, and advance the turn counter.
    ///
    /// Split out of [`run`](AgentRuntime::run) for issue #99 so a loop-level
    /// retry can re-drive a failed turn *without* appending the user message a
    /// second time. A failed kernel turn leaves the transcript at the point it
    /// stopped (`execute_kernel_turn` folds the attempt's committed messages
    /// back in), so the re-drive resumes there rather than starting over.
    async fn drive_turn(&mut self) -> Result<RuntimeOutcome> {
        let turn_outcome = self.execute_kernel_turn().await?;
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
    /// The forwarder also returns the messages it saw appended, which a failed
    /// kernel turn folds back into the transcript (see below).
    async fn execute_kernel_turn(&mut self) -> Result<crate::kernel::TurnOutcome> {
        let (event_tx, mut event_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::event::AgentEvent>();
        let sink = self.event_sink.clone();
        let forwarder = tokio::spawn(async move {
            let mut deferred_finished: Option<crate::event::AgentEvent> = None;
            let mut committed: Vec<Message> = Vec::new();
            while let Some(ev) = event_rx.recv().await {
                match &ev {
                    AgentEvent::TurnFinished { .. } => {
                        deferred_finished = Some(ev);
                        continue;
                    }
                    AgentEvent::MessageAppended { message, .. }
                    | AgentEvent::MessageAppendedWithAudit { message, .. } => {
                        committed.push(message.clone());
                    }
                    _ => {}
                }
                sink.emit(ev).await;
            }
            (deferred_finished, committed)
        });

        let ctx = TurnContext {
            messages: Arc::clone(&self.transcript),
            tool_specs: self.kernel.tools().specs(),
            step_events_tx: Some(event_tx.clone()),
            streaming: self.streaming,
            permission_hook: self.permission_hook.clone(),
            exploring_plan_mode: self.plan_approval_gate.exploring_plan_mode.clone(),
            permission_mode: self.kernel.tools().permission_mode(),
            mailbox: None,
            turn: self.checkpoints.turn_index.load(Ordering::Relaxed) as u32,
            prompt_segments: self.prompt_segments.clone(),
            // Goal 399: seed the per-turn context from the kernel-level
            // budget (set via `AgentRuntimeBuilder::wall_timeout_secs`).
            // `AgentKernel::run` resolves the effective value; 0 = unlimited.
            wall_timeout_secs: self.kernel.wall_timeout_secs,
        };

        let turn_outcome = self.kernel.run(ctx).await;
        drop(event_tx);
        // Wait for forwarder; stash the deferred TurnFinished for emit_turn_messages.
        let (deferred_finished, committed) = match forwarder.await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("forwarder task panicked, TurnFinished will be synthesized: {e}");
                (None, Vec::new())
            }
        };
        self.deferred_turn_finished = deferred_finished;
        let turn_outcome = match turn_outcome {
            Ok(outcome) => outcome,
            Err(e) => {
                // Issue #99: the kernel runs on a copy-on-write clone of the
                // transcript, so a failed turn's already-committed messages
                // never reached it — even though `RunCore` persisted them at
                // push time (`transcript.jsonl` already has them). Fold them
                // back so the canonical transcript matches what is on disk and
                // a loop retry resumes at the step that failed instead of
                // re-running the turn's tools.
                //
                // A kernel failure always happens while dispatching the LLM
                // call for a step, i.e. before that step's assistant message is
                // pushed, so the fold can never end mid-way through a
                // tool_call/tool_result pair (invariant #8).
                Arc::make_mut(&mut self.transcript).extend(committed);
                return Err(e);
            }
        };
        Ok(turn_outcome)
    }

    /// Incorporate the kernel's new messages into the wrapper transcript and
    /// flush the deferred `TurnFinished` event.
    ///
    /// Since the real-time persistence change, `RunCore` emits
    /// `MessageAppended` / `MessageAppendedWithAudit` at push time (per ReAct
    /// step), so persistence sinks already wrote every committed message to
    /// `transcript.jsonl` while the turn was running. This method therefore
    /// only extends the canonical transcript and releases the `TurnFinished`
    /// event the forwarder withheld — preserving the SDK ordering guarantee
    /// (TurnFinished strictly after every MessageAppended).
    async fn emit_turn_messages(&mut self, outcome: &crate::kernel::TurnOutcome) {
        Arc::make_mut(&mut self.transcript).extend(outcome.new_messages.iter().cloned());
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

    /// Issue #31: session-bound environment teardown. Drains the session's
    /// background-job manager (transport-backed jobs die with the
    /// environment) and calls `destroy()` on the registry's transport —
    /// idempotent by contract, safe on every teardown path (DELETE / idle
    /// eviction / graceful shutdown).
    pub async fn destroy_environment(&mut self) {
        // Issue #31 §D: session teardown order — drain the session's
        // background jobs (transport-backed jobs die with the environment
        // when the transport is destroyed below), then destroy the
        // transport itself. The once-guard makes this idempotent even for
        // transports whose own destroy isn't; the manager clear runs first
        // so a late-arriving job completion finds an empty map instead of
        // a destroyed environment it cannot record into.
        if self.session.environment_destroyed {
            return;
        }
        self.session.environment_destroyed = true;
        let tools = self.kernel.tools();
        tools.bg_manager().lock().await.clear();
        tools.transport().destroy().await;
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
    ///
    /// Issue #48 / Goal 409: the token is also mirrored onto the registered
    /// `ExitPlanModeTool` (when present) so a plan-approval wait is
    /// cancellable *inside* the await — the between-step cancel probes can
    /// never fire while the turn is parked waiting for a reviewer decision.
    /// Hosts that swap the event sink per turn (REPL) keep this property
    /// because `set_event_sink` re-attaches the mirrored token on every
    /// re-registration.
    pub fn set_interrupt_token(&mut self, token: tokio_util::sync::CancellationToken) {
        self.plan_approval_interrupt_token = Some(token.clone());
        self.kernel.shutdown_token = Some(token);
        // Re-register the plan tool so the new token reaches the approval
        // wait. Presence-guarded like the sink fan-out above: never
        // re-introduce a tool the surface filter dropped (issue #65).
        self.refresh_plan_tool();
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
        // Issue #65: only when the tool is actually in the registry — a
        // surface filter (operator allow-list) that dropped TodoWrite must
        // not be silently undone by a per-session/per-turn sink swap, which
        // is exactly when HTTP/CLI sessions call this.
        if self.kernel.tools().find_by_name("TodoWrite").is_some() {
            self.kernel
                .tools_mut()
                .register_mut(Arc::new(TodoWriteTool::new(
                    self.todo_list.clone(),
                    sink.clone(),
                )));
        }
        // Goal-165: re-register ExitPlanModeTool with the new sink so that
        // AgentEvent::PlanProposed reaches the new consumer (e.g. TUI).
        // Issue #47④: hosts that opt in (REPL) get a bounded approval wait
        // so an unanswered plan review cannot park the turn forever; TUI /
        // SDK hosts keep the default wait-forever semantics.
        // Issue #48 / Goal 409: the per-turn interrupt token is re-attached
        // on every re-registration so Ctrl-C can cancel the approval wait
        // regardless of how many sink swaps happened this turn.
        // Issue #65: same presence guard — re-point, never re-introduce.
        self.refresh_plan_tool();
    }

    /// Enable a bounded approval wait for `exit_plan_mode` when the event
    /// sink is swapped (REPL turn loop). On timeout the plan is treated as
    /// rejected ("plan approval timed out") so the turn can finish (#7).
    pub fn set_approval_wait_timeout_secs(&mut self, secs: u64) {
        self.approval_wait_timeout_secs = Some(secs);
        self.refresh_plan_tool();
    }

    /// Remove the bounded approval wait, restoring wait-forever semantics
    /// (issue #48 / Goal 409: `RECURSIVE_PLAN_APPROVAL_TIMEOUT_SECS=0`).
    /// The interrupt token, if installed, still cancels the wait.
    pub fn clear_approval_wait_timeout(&mut self) {
        self.approval_wait_timeout_secs = None;
        self.refresh_plan_tool();
    }

    /// Re-register `ExitPlanModeTool` from the current timeout / interrupt
    /// token / sink state. Presence-guarded (issue #65): never re-introduces
    /// a tool the surface filter dropped.
    fn refresh_plan_tool(&mut self) {
        if self
            .kernel
            .tools()
            .find_by_name(crate::tools::plan_mode::EXIT_PLAN_MODE_TOOL_NAME)
            .is_none()
        {
            return;
        }
        let mut tool =
            ExitPlanModeTool::new(self.plan_approval_gate.clone(), self.event_sink.clone());
        if let Some(secs) = self.approval_wait_timeout_secs {
            tool = tool.with_approval_wait_timeout(std::time::Duration::from_secs(secs));
        }
        if let Some(token) = self.plan_approval_interrupt_token.clone() {
            tool = tool.with_cancellation_token(token);
        }
        self.kernel.tools_mut().register_mut(Arc::new(tool));
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
        self.permission_hook = Some(hook.clone());
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
    ///
    /// Issue #99 — the loop survives two things it used to die of:
    ///
    /// - **Restart.** When a wakeup store directory is configured (see
    ///   `AgentRuntimeBuilder::wakeup_store_dir`), the pending wakeup is
    ///   written to disk (with its due time) before the sleep and cleared as
    ///   soon as it fires (and again when the loop ends), so an
    ///   upgraded/restarted process can restore a record whose due time has
    ///   passed ([`crate::tasks::wakeup_store`]).
    /// - **A single failed turn.** A turn whose failure left no assistant
    ///   output is re-driven under [`LoopRetryPolicy`] backoff instead of
    ///   ending the whole loop. The re-drive resumes from the failed step
    ///   because `execute_kernel_turn` folds the attempt's committed messages
    ///   back into the transcript; a turn that already answered is never
    ///   replayed (`AgentRuntime::retry_is_safe`).
    pub async fn run_loop(
        &mut self,
        initial_goal: impl Into<String>,
        wakeup_slot: &crate::tools::WakeupSlot,
    ) -> Result<Vec<RuntimeOutcome>> {
        let mut outcomes = Vec::new();
        let mut next_goal = initial_goal.into();

        loop {
            let outcome = match self.run_turn_with_retry(&next_goal).await {
                Ok(outcome) => outcome,
                Err(e) => {
                    // The loop is over — leaving a pending record behind would
                    // resurrect a finished loop on the next start.
                    self.clear_pending_wakeup();
                    return Err(e);
                }
            };
            outcomes.push(outcome);

            // Check if the agent scheduled a wakeup
            let wakeup = wakeup_slot.lock().ok().and_then(|mut slot| slot.take());

            match wakeup {
                Some(req) => {
                    self.persist_pending_wakeup(&req);
                    tokio::time::sleep(req.delay).await;
                    // The wakeup just fired: the request is this turn's goal
                    // now, not a pending record. Leaving it on disk (due time
                    // in the past) for the whole turn would let another loop in
                    // the same workspace steal it. A crash during the turn is
                    // recoverable — the transcript holds the goal.
                    self.clear_pending_wakeup();
                    next_goal = req.prompt;
                }
                None => {
                    self.clear_pending_wakeup();
                    break;
                }
            }
        }
        Ok(outcomes)
    }

    /// Run one loop turn, retrying a transient failure with exponential
    /// backoff (issue #99).
    ///
    /// Only a turn that is safe to replay is retried (`retry_is_safe`); the
    /// replay goes through `drive_turn` because `run` already appended the
    /// goal, and appending it again would double the prompt in the transcript.
    async fn run_turn_with_retry(&mut self, goal: &str) -> Result<RuntimeOutcome> {
        let mut result = self.run(goal).await;
        let mut attempt = 0usize;
        loop {
            match result {
                Ok(outcome) => return Ok(outcome),
                Err(err) => {
                    if !LoopRetryPolicy::is_retryable(&err) || !self.retry_is_safe() {
                        return Err(err);
                    }
                    let Some(backoff) = self.loop_retry.backoff_for(attempt) else {
                        return Err(err);
                    };
                    attempt += 1;
                    tracing::warn!(
                        attempt,
                        max_retries = self.loop_retry.max_retries,
                        backoff_ms = u64::try_from(backoff.as_millis()).unwrap_or(u64::MAX),
                        error = %err,
                        "loop turn failed; retrying after backoff"
                    );
                    tokio::time::sleep(backoff).await;
                }
            }
            result = self.drive_turn().await;
        }
    }

    /// Whether replaying the current turn can duplicate work.
    ///
    /// A replay is safe unless the turn already produced its final answer:
    /// `execute_kernel_turn` folds a failed attempt's committed messages back
    /// into the transcript, so the tail is the *resume point* — the staged
    /// prompt, a tool result, or an injected system note — and re-driving the
    /// turn re-issues only the LLM call that failed, never a tool that already
    /// ran. An assistant tail means the turn ended and the failure came from
    /// post-turn bookkeeping; replaying that would append a second answer for
    /// work that is already recorded.
    fn retry_is_safe(&self) -> bool {
        !matches!(
            self.transcript.last().map(|m| m.role),
            None | Some(crate::message::Role::Assistant)
        )
    }

    /// Write the pending wakeup to the session directory so a restart can
    /// restore it. Best-effort: a full disk must not end a healthy loop — the
    /// in-memory loop continues either way.
    fn persist_pending_wakeup(&self, req: &crate::tools::WakeupRequest) {
        let Some(dir) = self.wakeup_store_dir.as_deref() else {
            return;
        };
        let record = crate::tasks::wakeup_store::PersistedWakeup::new(
            req.reason.clone(),
            req.prompt.clone(),
            req.delay,
            crate::tasks::wakeup_store::now_ms(),
        );
        if let Err(e) = crate::tasks::wakeup_store::persist(dir, &record) {
            tracing::warn!(
                error = %e,
                dir = %dir.display(),
                "could not persist pending wakeup; a restart will drop it"
            );
        }
    }

    /// Drop the session's pending wakeup record (no-op when persistence is
    /// disabled). Best-effort, for the same reason as
    /// [`Self::persist_pending_wakeup`].
    fn clear_pending_wakeup(&self) {
        let Some(dir) = self.wakeup_store_dir.as_deref() else {
            return;
        };
        if let Err(e) = crate::tasks::wakeup_store::clear(dir) {
            tracing::warn!(
                error = %e,
                dir = %dir.display(),
                "could not clear pending wakeup record"
            );
        }
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

// Goal 385: unit tests extracted to a sibling file so `runtime.rs` fits the
// invariant #1 line budget (total-lines guard counts this file + tests).
// Pure mechanical move - no behaviour change.
#[cfg(test)]
mod tests;
