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

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::agent::FinishReason;
use crate::checkpoint::{CheckpointId, ShadowRepo};
use crate::checkpoint_log::CheckpointLogWriter;
use crate::error::Result;
use crate::event::{AgentEvent, EventSink, NullSink};
use crate::hooks::{HookEvent, HookRegistry};
use crate::kernel::{AgentKernel, AgentKernelBuilder, TurnContext, TurnOutcome};
use crate::llm::{ChatProvider, TokenUsage};
use crate::message::Message;
use crate::tools::plan_mode::{
    EnterPlanModeTool, ExitPlanModeTool, PlanApprovalGate, PlanModeRequestGate, RequestPlanModeTool,
};
use crate::tools::{TodoItem, TodoWriteTool, ToolRegistry, TouchedFiles};
use crate::Compactor;

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
// CheckpointState
// ──────────────────────────────────────────────────────────────────────────

/// Checkpoint subsystem state, grouped to reduce field count on [`AgentRuntime`].
///
/// When `shadow` and `session_id` are both `Some`, checkpoint tools are active.
/// When `None`, all checkpoint tools are unavailable.
///
/// **Goal 284**: automatic per-turn snapshots (pre + post) have been removed.
/// Checkpoints are now created only when the agent explicitly calls the
/// `checkpoint_save` tool.
struct CheckpointState {
    shadow: Option<Arc<ShadowRepo>>,
    session_id: Option<String>,
    /// 0-indexed turn counter. Shared with `checkpoint_save` tool via AtomicUsize.
    turn_index: Arc<AtomicUsize>,
    writer: Option<Arc<Mutex<CheckpointLogWriter>>>,
    touched_files: Option<Arc<Mutex<TouchedFiles>>>,
    /// Path to the `checkpoints.jsonl` log file for this session.
    log_path: Option<std::path::PathBuf>,
}

impl CheckpointState {
    fn disabled() -> Self {
        Self {
            shadow: None,
            session_id: None,
            turn_index: Arc::new(AtomicUsize::new(0)),
            writer: None,
            touched_files: None,
            log_path: None,
        }
    }

    fn enabled(&self) -> bool {
        self.shadow.is_some() && self.session_id.is_some()
    }
}

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
    /// Checkpoint subsystem (snapshot, session-id, writer, touched-files).
    /// Grouped to reduce field count; inactive when checkpoints are disabled.
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

        let turn_outcome = self.execute_kernel_turn().await?;
        self.emit_turn_messages(&turn_outcome).await;
        // Goal 289: cross-turn compaction runs AFTER the turn so the
        // threshold check sees the full turn's growth (user + assistant +
        // tool messages) rather than the pessimistic pre-turn size. One
        // pass per turn covers the entire growth instead of firing
        // reactively at the start of every turn.
        self.maybe_compact_cross_turn().await?;

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
    async fn maybe_compact_cross_turn(&mut self) -> Result<()> {
        let Some(ref compactor) = self.compactor else {
            return Ok(());
        };
        let chars = Compactor::estimate_chars(&self.transcript);
        if chars < compactor.threshold_chars {
            return Ok(());
        }
        self.kernel.hooks().dispatch(HookEvent::PreCompact {
            transcript_len: chars,
        });
        let Some((removed, summary_chars)) = compactor
            .apply_to_transcript(
                self.kernel.llm().as_ref(),
                Arc::make_mut(&mut self.transcript),
                self.checkpoints
                    .turn_index
                    .load(std::sync::atomic::Ordering::Relaxed),
            )
            .await?
        else {
            return Ok(());
        };
        self.kernel.hooks().dispatch(HookEvent::PostCompact {
            removed,
            summary_chars,
        });
        self.event_sink
            .emit(AgentEvent::CompactionBoundary {
                turn: self.checkpoints.turn_index.load(Ordering::Relaxed) as u32,
                compacted_count: removed,
                summary_uuid: None,
            })
            .await;
        if let Some(summary) = self.transcript.first().cloned() {
            self.event_sink
                .emit(AgentEvent::MessageAppended {
                    message: summary,
                    usage: None,
                })
                .await;
        }
        Ok(())
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
        compactor
            .apply_to_transcript(
                self.kernel.llm().as_ref(),
                Arc::make_mut(&mut self.transcript),
                self.checkpoints
                    .turn_index
                    .load(std::sync::atomic::Ordering::Relaxed),
            )
            .await?;
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
            if let Some(mgr) = bg_manager {
                let mut mgr = mgr.lock().await;
                if let Some((id, output)) = mgr.take_completed() {
                    next_goal = format!("Background job '{}' completed:\n{}", id, output);
                    continue;
                }
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
    saved_event_sink: Option<Arc<dyn EventSink>>,
    compactor: Option<Compactor>,
    /// When `true`, register `enter_plan_mode`, `exit_plan_mode`, and
    /// `request_plan_mode` tools. These tools block waiting for human
    /// approval via the plan approval gate, so they must only be registered
    /// when a live interactive channel (TUI or interactive CLI) is present
    /// to call `confirm_plan()` / `reject_plan()`. Headless and non-interactive
    /// callers must leave this `false` (the default) — the tools simply do not
    /// exist in the registry, so the model cannot invoke them.
    with_plan_mode_tools: bool,
    /// Goal-291: tail-window size for the goal-evaluator judge. Default 12.
    goal_eval_transcript_tail: usize,
    /// Goal-318: skills passed through to AgentKernel for Globs-mode injection.
    skills: Vec<crate::skills::Skill>,
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
            with_plan_mode_tools: false,
            goal_eval_transcript_tail: 12,
            skills: Vec::new(),
        }
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
            checkpoints: CheckpointState::disabled(),
            todo_list,
            plan_approval_gate,
            plan_mode_request_gate,
            goal_state: Arc::new(RwLock::new(None)),
            message_queue: std::collections::VecDeque::new(),
            deferred_turn_finished: None,
            session: SessionLifecycle::open(),
            goal_eval_transcript_tail: self.goal_eval_transcript_tail,
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
    use crate::tools::plan_mode::{ENTER_PLAN_MODE_TOOL_NAME, EXIT_PLAN_MODE_TOOL_NAME};
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
}
