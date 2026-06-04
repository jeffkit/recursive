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

use std::sync::{Arc, Mutex, RwLock};

use crate::agent::{FinishReason, PlanningMode};
use crate::checkpoint::{CheckpointId, ShadowRepo};
use crate::checkpoint_log::{CheckpointLogWriter, CheckpointRecord, TouchedVia};
use crate::error::Result;
use crate::event::{AgentEvent, EventSink, NullSink};
use crate::hooks::{HookEvent, HookRegistry};
use crate::kernel::{AgentKernel, AgentKernelBuilder, TurnContext, TurnOutcome};
use crate::llm::{LlmProvider, TokenUsage};
use crate::message::Message;
use crate::tools::plan_mode::{
    EnterPlanModeTool, ExitPlanModeTool, PlanApprovalGate, PlanModeRequestGate, RequestPlanModeTool,
};
use crate::tools::PermissionHook;
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
/// When `shadow` and `session_id` are both `Some`, snapshotting is active.
/// When `None`, all snapshot/log operations are no-ops.
struct CheckpointState {
    shadow: Option<Arc<ShadowRepo>>,
    session_id: Option<String>,
    /// 0-indexed turn counter used for checkpoint labels.
    turn_index: usize,
    writer: Option<CheckpointLogWriter>,
    touched_files: Option<Arc<Mutex<TouchedFiles>>>,
}

impl CheckpointState {
    fn disabled() -> Self {
        Self {
            shadow: None,
            session_id: None,
            turn_index: 0,
            writer: None,
            touched_files: None,
        }
    }

    fn enabled(&self) -> bool {
        self.shadow.is_some() && self.session_id.is_some()
    }

    /// Take a snapshot just before a turn begins. Errors are logged
    /// as warnings and swallowed so a checkpoint failure cannot brick a run.
    ///
    /// The underlying git subprocess is blocking; it runs inside
    /// `tokio::task::spawn_blocking` to avoid starving the async runtime.
    async fn snapshot_pre_turn(&self, user_text: &str) -> Option<CheckpointId> {
        let repo = self.shadow.as_ref()?.clone();
        let sid = self.session_id.as_ref()?.clone();
        let label = format!(
            "turn {} pre: {}",
            self.turn_index,
            truncate_label(user_text)
        );
        match tokio::task::spawn_blocking(move || repo.snapshot_for_session(&sid, &label)).await {
            Ok(Ok(id)) => Some(id),
            Ok(Err(e)) => {
                tracing::warn!("checkpoint pre-snapshot failed: {e}");
                None
            }
            Err(e) => {
                tracing::warn!("checkpoint pre-snapshot task panicked: {e}");
                None
            }
        }
    }

    /// Take a snapshot at the end of a turn, compute the touched-file set,
    /// and append a [`CheckpointRecord`] to the log.
    ///
    /// Blocking git subprocesses run inside `tokio::task::spawn_blocking`.
    async fn snapshot_post_turn(
        &self,
        user_text: &str,
        pre: Option<&CheckpointId>,
        started_at: i64,
    ) -> Option<CheckpointId> {
        let repo = self.shadow.as_ref()?.clone();
        let sid = self.session_id.as_ref()?.clone();
        let label = format!(
            "turn {} post: {}",
            self.turn_index,
            truncate_label(user_text)
        );

        // Collect touched files before moving into spawn_blocking.
        let (mut paths, saw_shell) = match &self.touched_files {
            Some(slot) => match slot.lock() {
                Ok(t) => (t.paths_sorted(), t.saw_shell),
                Err(_) => (vec![], false),
            },
            None => (vec![], true),
        };

        let pre_cloned = pre.cloned();
        let turn_index = self.turn_index;
        let writer = self.writer.clone();
        let repo2 = repo.clone();

        let post = match tokio::task::spawn_blocking(move || {
            let post = repo2.snapshot_for_session(&sid, &label)?;

            let mut via = TouchedVia::Structured;
            if saw_shell {
                via = TouchedVia::ShellDiff;
                if let Some(ref pre_id) = pre_cloned {
                    if let Ok(diff_paths) = repo2.changed_paths(pre_id, &post) {
                        let mut set: std::collections::HashSet<String> = paths.drain(..).collect();
                        for p in diff_paths {
                            set.insert(p);
                        }
                        paths = {
                            let mut v: Vec<String> = set.into_iter().collect();
                            v.sort();
                            v
                        };
                    }
                }
            }

            if let Some(w) = writer {
                let rec = CheckpointRecord {
                    turn: turn_index,
                    pre: pre_cloned,
                    post: post.clone(),
                    touched_files: paths,
                    touched_via: via,
                    started_at,
                    finished_at: unix_now(),
                };
                if let Err(e) = w.append(&rec) {
                    tracing::warn!("checkpoint log append failed: {e}");
                }
            }

            Ok::<CheckpointId, crate::error::Error>(post)
        })
        .await
        {
            Ok(Ok(id)) => id,
            Ok(Err(e)) => {
                tracing::warn!("checkpoint post-snapshot failed: {e}");
                return None;
            }
            Err(e) => {
                tracing::warn!("checkpoint post-snapshot task panicked: {e}");
                return None;
            }
        };

        Some(post)
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
    permission_hook: Option<Arc<dyn PermissionHook>>,
    /// Planning mode (immediate vs plan-first).
    planning_mode: PlanningMode,
    /// Optional compactor for cross-turn transcript summarization.
    compactor: Option<Compactor>,
    /// Checkpoint subsystem (snapshot, session-id, writer, touched-files).
    /// Grouped to reduce field count; inactive when checkpoints are disabled.
    checkpoints: CheckpointState,
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
    pub goal_state: Arc<RwLock<Option<GoalState>>>,
    /// Goal-181: FIFO queue of user messages waiting to be processed.
    /// Callers use [`enqueue`](AgentRuntime::enqueue) instead of
    /// [`run`](AgentRuntime::run) directly; the queue is drained in FIFO
    /// order so that messages sent while a turn is in flight are processed
    /// automatically when the current turn completes.
    message_queue: std::collections::VecDeque<String>,
    /// Deferred `TurnFinished` event held by `execute_kernel_turn` until
    /// `emit_turn_messages` can flush it after all assistant messages.
    deferred_turn_finished: Option<AgentEvent>,
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
    pub async fn run(&mut self, user_text: impl Into<String>) -> Result<RuntimeOutcome> {
        let user_text = user_text.into();

        tracing::Span::current().record(
            "session_id",
            self.checkpoints.session_id.as_deref().unwrap_or(""),
        );
        tracing::debug!(
            session_id = self.checkpoints.session_id.as_deref().unwrap_or(""),
            turn = self.checkpoints.turn_index,
            "agent.turn: starting"
        );

        self.kernel
            .hooks()
            .dispatch(HookEvent::SessionStart { goal: &user_text });

        let started_at = unix_now();
        let pre_id = self.checkpoints.snapshot_pre_turn(&user_text).await;
        self.reset_touched_files();
        self.kernel.hooks().dispatch(HookEvent::UserPromptSubmit {
            content: &user_text,
        });
        self.append_user_message(&user_text).await;
        self.maybe_compact_cross_turn().await?;

        let turn_outcome = self.execute_kernel_turn().await?;
        self.update_plan_state(&turn_outcome);
        self.emit_turn_messages(&turn_outcome).await;

        let mut outcome: RuntimeOutcome = turn_outcome.into();
        outcome.checkpoint_id = self
            .checkpoints
            .snapshot_post_turn(&user_text, pre_id.as_ref(), started_at)
            .await;

        tracing::info!(
            steps = outcome.steps,
            finish_reason = ?outcome.finish_reason,
            "agent.turn: finished"
        );
        self.checkpoints.turn_index += 1;

        // Do not fire SessionEnd on graceful cancellation — hooks registered
        // for SessionEnd typically perform post-run cleanup / summarisation
        // that makes no sense if the turn was cancelled by the user.
        if !matches!(outcome.finish_reason, FinishReason::Cancelled) {
            self.kernel
                .hooks()
                .dispatch(HookEvent::SessionEnd { outcome: &outcome });
        }

        Ok(outcome)
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
        self.transcript.push(user_msg.clone());
        self.event_sink
            .emit(AgentEvent::MessageAppended {
                message: user_msg,
                parent_uuid: None,
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
            .apply_to_transcript(self.kernel.llm().as_ref(), &mut self.transcript, 0)
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
                turn: self.checkpoints.turn_index as u32,
                compacted_count: removed,
                summary_uuid: None,
            })
            .await;
        if let Some(summary) = self.transcript.first().cloned() {
            self.event_sink
                .emit(AgentEvent::MessageAppended {
                    message: summary,
                    parent_uuid: None,
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
            messages: self.transcript.clone(),
            tool_specs: self.kernel.tools().specs(),
            step_events_tx: Some(event_tx.clone()),
            plan_confirmed: self.plan_confirmed,
            plan_buffer: self.pending_plan_calls.clone(),
            streaming: self.streaming,
            permission_hook: self.permission_hook.clone(),
            planning_mode: self.planning_mode.clone(),
            exploring_plan_mode: self.plan_approval_gate.exploring_plan_mode.clone(),
            permission_mode: self.kernel.tools().permission_mode(),
            mailbox: None,
        };

        let turn_outcome = self.kernel.run(ctx).await?;
        drop(event_tx);
        // Wait for forwarder; stash the deferred TurnFinished for emit_turn_messages.
        self.deferred_turn_finished = forwarder.await.ok().flatten();
        Ok(turn_outcome)
    }

    /// Update plan-confirmation state from a completed turn outcome.
    fn update_plan_state(&mut self, outcome: &crate::kernel::TurnOutcome) {
        if outcome.finish_reason == crate::agent::FinishReason::PlanPending {
            self.pending_plan_calls = outcome.plan_buffer.clone();
        }
        let was_confirmed = self.plan_confirmed;
        self.plan_confirmed = false;
        if was_confirmed {
            self.pending_plan_calls = None;
        }
    }

    /// Append new kernel messages to the transcript and emit `MessageAppended`
    /// (or `MessageAppendedWithAudit`) for each, then flush the deferred
    /// `TurnFinished` event.
    async fn emit_turn_messages(&mut self, outcome: &crate::kernel::TurnOutcome) {
        let new_messages = outcome.new_messages.clone();
        let turn_usage = crate::session::UsageMeta::from_token_usage(&outcome.usage);
        let mut tool_audits = outcome.tool_audits.clone();
        self.transcript.extend(new_messages.iter().cloned());
        for msg in &new_messages {
            let event = if msg.role == crate::message::Role::Tool {
                if let Some(tcid) = &msg.tool_call_id {
                    if let Some(audit) = tool_audits.remove(tcid) {
                        AgentEvent::MessageAppendedWithAudit {
                            message: msg.clone(),
                            audit,
                        }
                    } else {
                        AgentEvent::MessageAppended {
                            message: msg.clone(),
                            parent_uuid: None,
                            usage: None,
                        }
                    }
                } else {
                    AgentEvent::MessageAppended {
                        message: msg.clone(),
                        parent_uuid: None,
                        usage: None,
                    }
                }
            } else {
                let usage = if matches!(msg.role, crate::message::Role::Assistant) {
                    Some(turn_usage.clone())
                } else {
                    None
                };
                AgentEvent::MessageAppended {
                    message: msg.clone(),
                    parent_uuid: None,
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
    async fn drain_queue(&mut self) -> Result<Option<RuntimeOutcome>> {
        let mut last: Option<RuntimeOutcome> = None;
        while let Some(msg) = self.message_queue.pop_front() {
            last = Some(self.run(msg).await?);
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

    /// Replace the current transcript (useful for restoring from a saved session).
    pub fn set_transcript(&mut self, transcript: Vec<Message>) {
        self.transcript = transcript;
    }

    /// Discard all transcript messages after index `len`, restoring the
    /// transcript to the state it had before a turn started. Used by the
    /// TUI abort path to prevent orphan tool_call entries.
    pub fn truncate_transcript(&mut self, len: usize) {
        self.transcript.truncate(len);
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

    /// Return a shared reference to the plan-approval gate.
    ///
    /// Callers (e.g. HTTP handlers) that need to inspect `pending_plan` or
    /// call `approve`/`reject` without holding the runtime `Mutex` can clone
    /// this `Arc` and operate on the gate directly.
    pub fn plan_approval_gate(&self) -> Arc<PlanApprovalGate> {
        self.plan_approval_gate.clone()
    }

    /// Confirm the pending plan, allowing execution to proceed on the next run.
    ///
    /// Covers both PlanFirst mode (sets `plan_confirmed` flag for kernel) and
    /// Plan Mode 2.0 (wakes `exit_plan_mode`'s blocking wait via the gate).
    pub fn confirm_plan(&mut self) {
        self.plan_confirmed = true;
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
            .apply_to_transcript(self.kernel.llm().as_ref(), &mut self.transcript, 0)
            .await?;
        Ok(())
    }

    /// Update the planning mode in place. Allows the TUI's
    /// `/plan on|off` command to flip plan-first vs immediate
    /// without rebuilding the runtime.
    pub fn set_planning_mode(&mut self, mode: PlanningMode) {
        self.planning_mode = mode;
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
    /// Covers both PlanFirst mode (injects a user message) and Plan Mode 2.0
    /// (wakes `exit_plan_mode`'s blocking wait with the rejection reason).
    pub fn reject_plan(&mut self, reason: &str) {
        // PlanFirst mode: clear pending plan calls and inject rejection message.
        self.pending_plan_calls = None;
        self.plan_confirmed = false;
        let rejection_msg = Message::user(format!("Plan rejected: {}", reason));
        self.transcript.push(rejection_msg);

        // Plan Mode 2.0: wake the blocking ExitPlanModeTool with the reason.
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
            if let Some(ref mut s) = *g {
                s.status = GoalStatus::Cleared;
            }
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

            // Increment turn counter.
            let turns = {
                let mut guard = self.goal_state.write().ok();
                if let Some(ref mut guard) = guard {
                    if let Some(ref mut gs) = **guard {
                        gs.turns += 1;
                        gs.turns
                    } else {
                        break; // goal was cleared externally
                    }
                } else {
                    break;
                }
            };

            // Budget exceeded?
            if turns >= max_turns {
                if let Ok(mut g) = self.goal_state.write() {
                    if let Some(ref mut gs) = *g {
                        gs.status = GoalStatus::Cleared;
                    }
                    *g = None;
                }
                self.event_sink.emit(AgentEvent::GoalCleared).await;
                tracing::warn!(
                    "goal loop: turn budget of {max_turns} exceeded without achieving condition"
                );
                break;
            }

            // Ask the judge.
            let verdict = evaluator.evaluate(&condition, self.transcript()).await?;
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

    // ──────────────────────────────────────────────────────────────────
    // Checkpoint helpers
    // ──────────────────────────────────────────────────────────────────

    /// Bind this runtime to a checkpoint chain. Subsequent calls to
    /// `run()` will snapshot before and after each turn under
    /// `refs/sessions/<session_id>/HEAD` and append a record to
    /// `checkpoint_log_path` (a `checkpoints.jsonl` file).
    ///
    /// `touched_slot` is the same collector previously installed on
    /// the [`ToolRegistry`] via `with_touched_files`. If no collector
    /// is provided, file-attribution falls back to "shell-diff" for
    /// every turn.
    ///
    /// Side effect: registers the read-only `checkpoint_list` and
    /// `checkpoint_diff` tools, scoped to this session, onto the
    /// kernel's tool registry — so the agent can introspect its own
    /// checkpoint chain (but cannot save or restore; those are
    /// orchestration concerns).
    pub fn enable_checkpoints(
        &mut self,
        shadow: Arc<ShadowRepo>,
        session_id: impl Into<String>,
        log_path: std::path::PathBuf,
        touched_slot: Option<Arc<Mutex<TouchedFiles>>>,
    ) -> Result<()> {
        let writer = CheckpointLogWriter::open(&log_path)?;
        let session_id = session_id.into();

        // Register session-scoped read-only checkpoint tools onto the
        // kernel's registry. The shadow repo is shared via
        // Arc<Mutex<ShadowRepo>> so the tools and the runtime see the
        // same checkpoint chain.
        let tool_repo = Arc::new(Mutex::new(ShadowRepo::clone(&shadow)));
        let ctx = crate::tools::CheckpointToolCtx {
            repo: tool_repo,
            session_id: session_id.clone(),
        };
        let tools = self.kernel.tools_mut();
        tools.register_mut(Arc::new(crate::tools::CheckpointList::new(ctx.clone())));
        tools.register_mut(Arc::new(crate::tools::CheckpointDiff::new(ctx)));

        self.checkpoints.shadow = Some(shadow);
        self.checkpoints.session_id = Some(session_id);
        self.checkpoints.writer = Some(writer);
        self.checkpoints.touched_files = touched_slot;
        Ok(())
    }

    /// Whether checkpoint snapshots are active.
    pub fn checkpoints_enabled(&self) -> bool {
        self.checkpoints.enabled()
    }

    /// Returns the 0-indexed counter that will be assigned to the
    /// *next* turn (i.e. the count of turns already executed).
    pub fn turn_index(&self) -> usize {
        self.checkpoints.turn_index
    }
}

/// Current Unix timestamp in seconds.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Truncate a turn label to keep checkpoint commit messages readable.
fn truncate_label(s: &str) -> String {
    const MAX: usize = 120;
    let trimmed: String = s.lines().next().unwrap_or("").chars().take(MAX).collect();
    if trimmed.len() < s.len() {
        format!("{trimmed}…")
    } else {
        trimmed
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
    permission_hook: Option<Arc<dyn PermissionHook>>,
    planning_mode: PlanningMode,
    saved_event_sink: Option<Arc<dyn EventSink>>,
    compactor: Option<Compactor>,
    /// UUID of the parent agent's last message. When set, the first
    /// `MessageAppended` event will carry this as `parent_uuid`, branching
    /// the subagent's messages off that point in the conversation tree (g155).
    /// Stored here for future multi-agent orchestration; not yet wired to
    /// the actual event emission path.
    pub parent_agent_last_uuid: Option<String>,
    /// When `true`, register `enter_plan_mode`, `exit_plan_mode`, and
    /// `request_plan_mode` tools. These tools block waiting for human
    /// approval via the plan approval gate, so they must only be registered
    /// when a live interactive channel (TUI or interactive CLI) is present
    /// to call `confirm_plan()` / `reject_plan()`. Headless and non-interactive
    /// callers must leave this `false` (the default) — the tools simply do not
    /// exist in the registry, so the model cannot invoke them.
    with_plan_mode_tools: bool,
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
            parent_agent_last_uuid: None,
            with_plan_mode_tools: false,
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

    /// Set the UUID of the parent agent's last message.
    ///
    /// When set, this runtime's messages will be stamped with this UUID as
    /// `parent_uuid` on their first `MessageAppended` event, branching the
    /// subagent chain off the given point in the parent's conversation tree
    /// (g155). Currently stored for future multi-agent orchestration.
    pub fn parent_agent_last_uuid(mut self, uuid: impl Into<String>) -> Self {
        self.parent_agent_last_uuid = Some(uuid.into());
        self
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
    pub fn permission_hook(mut self, hook: Arc<dyn PermissionHook>) -> Self {
        self.permission_hook = Some(hook);
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

    /// Build the [`AgentRuntime`].
    ///
    /// Returns an error if the LLM provider is missing.
    pub fn build(self) -> Result<AgentRuntime> {
        let mut kernel = self.kernel_builder.build()?;

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

        Ok(AgentRuntime {
            kernel,
            transcript,
            event_sink,
            pending_plan_calls: None,
            plan_confirmed: false,
            streaming: self.streaming,
            permission_hook: self.permission_hook,
            planning_mode: self.planning_mode,
            compactor: self.compactor,
            checkpoints: CheckpointState::disabled(),
            todo_list,
            plan_approval_gate,
            plan_mode_request_gate,
            goal_state: Arc::new(RwLock::new(None)),
            message_queue: std::collections::VecDeque::new(),
            deferred_turn_finished: None,
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

    #[tokio::test]
    async fn runtime_snapshots_at_turn_boundaries() {
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

        let o1 = rt.run("turn 0").await.unwrap();
        assert!(o1.checkpoint_id.is_some());
        let o2 = rt.run("turn 1").await.unwrap();
        assert!(o2.checkpoint_id.is_some());
        assert_ne!(o1.checkpoint_id, o2.checkpoint_id);

        let recs = crate::read_checkpoint_log(&log_path).unwrap();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].turn, 0);
        assert_eq!(recs[1].turn, 1);
        // pre exists for both turns (post-snapshot may or may not differ).
        assert!(recs[0].pre.is_some());
        assert!(recs[1].pre.is_some());
    }

    #[tokio::test]
    async fn runtime_records_touched_files_for_write_file() {
        if !has_git() {
            return;
        }
        let dir = shadow_ws();
        // Provider plans one write_file then ends.
        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "writing".into(),
                tool_calls: vec![crate::llm::ToolCall {
                    id: "c1".into(),
                    name: "Write".into(),
                    arguments: json!({"path": "out.txt", "contents": "hello"}),
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

        let touched = Arc::new(Mutex::new(TouchedFiles::new()));
        let tools = ToolRegistry::local()
            .register(Arc::new(crate::tools::WriteFile::new(dir.path())))
            .with_touched_files(touched.clone());

        let mut rt = AgentRuntime::builder()
            .llm(llm)
            .tools(tools)
            .build()
            .unwrap();
        let shadow = Arc::new(crate::ShadowRepo::open_at(dir.path(), dir.shadow_dir()).unwrap());
        let log_path = dir.path().join("checkpoints.jsonl");
        rt.enable_checkpoints(shadow, "sess", log_path.clone(), Some(touched))
            .unwrap();

        let _ = rt.run("please write").await.unwrap();

        let recs = crate::read_checkpoint_log(&log_path).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].touched_files, vec!["out.txt".to_string()]);
        assert_eq!(recs[0].touched_via, crate::TouchedVia::Structured);
    }

    #[tokio::test]
    #[cfg_attr(target_os = "windows", ignore)]
    async fn runtime_falls_back_to_diff_for_run_shell() {
        if !has_git() {
            return;
        }
        let dir = shadow_ws();

        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "shelling".into(),
                tool_calls: vec![crate::llm::ToolCall {
                    id: "c1".into(),
                    name: "Bash".into(),
                    arguments: json!({"command": "echo created > made.txt"}),
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

        let touched = Arc::new(Mutex::new(TouchedFiles::new()));
        let tools = ToolRegistry::local()
            .register(Arc::new(crate::tools::RunShell::new(dir.path())))
            .with_touched_files(touched.clone());

        let mut rt = AgentRuntime::builder()
            .llm(llm)
            .tools(tools)
            .build()
            .unwrap();
        let shadow = Arc::new(crate::ShadowRepo::open_at(dir.path(), dir.shadow_dir()).unwrap());
        let log_path = dir.path().join("checkpoints.jsonl");
        rt.enable_checkpoints(shadow, "sess", log_path.clone(), Some(touched))
            .unwrap();

        let _ = rt.run("please make a file").await.unwrap();

        let recs = crate::read_checkpoint_log(&log_path).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].touched_via, crate::TouchedVia::ShellDiff);
        // The shell created made.txt which should be in the diff.
        assert!(
            recs[0].touched_files.iter().any(|p| p == "made.txt"),
            "expected made.txt in touched_files, got {:?}",
            recs[0].touched_files
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

    // ── compact_now / set_planning_mode (Goal 146) ────────────────────

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

    #[tokio::test]
    async fn set_planning_mode_updates_field() {
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "ok".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();
        // Default is Immediate.
        assert_eq!(rt.planning_mode, PlanningMode::Immediate);
        rt.set_planning_mode(PlanningMode::PlanFirst);
        assert_eq!(rt.planning_mode, PlanningMode::PlanFirst);
        rt.set_planning_mode(PlanningMode::Immediate);
        assert_eq!(rt.planning_mode, PlanningMode::Immediate);
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

    // ── Goal-201: plan mode tools are registered by the runtime builder ──

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
            tools.get("enter_plan_mode").is_some(),
            "enter_plan_mode must be registered by AgentRuntimeBuilder"
        );
        assert!(
            tools.get("exit_plan_mode").is_some(),
            "exit_plan_mode must be registered by AgentRuntimeBuilder"
        );
    }
}
