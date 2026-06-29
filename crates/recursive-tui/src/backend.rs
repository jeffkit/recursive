//! In-process agent backend for the TUI.
//!
//! [`Backend`] owns one tokio task that holds an [`recursive::AgentRuntime`].
//! The UI thread sends [`UserAction`]s into the worker via `action_tx` and
//! the worker pushes [`UiEvent`]s back via `event_rx`.
//!
//! Runtime construction and bash-mode dispatch live in sibling modules
//! (`runtime_builder`, `bash`) to keep this file focused on event bridging.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use recursive::event::CompositeSink;
use recursive::session::{SessionPersistenceSink, SessionWriter};
use recursive::tools::{PermissionHook, SharedSandboxRoots};
use recursive::{new_shared_sandbox_roots, AgentEvent, AgentRuntime, EventSink, SessionStatus};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::bash::{build_bash_registry, resolve_workspace_root, run_bash_command};
#[cfg(feature = "weixin")]
use crate::events::WeixinBackendRequest;
use crate::events::{PermissionRequest, SkillInstallEvent, UiEvent, UserAction};
use crate::runtime_builder::RuntimeBuild;

/// Local helper to fan-out from two channels in the worker loop.
enum Either<L, R> {
    Left(L),
    #[allow(dead_code)]
    Right(R),
}

/// A handle to the agent worker task.
pub struct Backend {
    pub action_tx: mpsc::UnboundedSender<UserAction>,
    pub event_rx: mpsc::UnboundedReceiver<UiEvent>,
    /// Shared cancel flag: the UI flips this to `true` to interrupt an
    /// in-flight turn; the worker's `tokio::select!` wakes and aborts.
    pub cancel_flag: Arc<AtomicBool>,
    /// Goal-161: side-channel for runtime permission requests.
    /// Separate from `event_rx` because `PermissionRequest` carries a
    /// `oneshot::Sender<bool>` which is not `PartialEq`/`Clone`.
    pub perm_rx: mpsc::UnboundedReceiver<PermissionRequest>,
    /// Goal-161: shared flag that enables/disables the runtime permission
    /// hook. The UI thread can flip this via `/permissions on|off`.
    pub permission_enabled: Arc<AtomicBool>,
    /// Goal-230: side-channel for skill-hub install requests from `install_skill`.
    /// Always present; when the `skill-hub` feature is disabled the receiver is
    /// backed by a channel whose sender is immediately dropped, so it never fires.
    pub skill_install_rx: mpsc::UnboundedReceiver<SkillInstallEvent>,
    /// WeChat side-channel: the daemon sends `WeixinBackendRequest`s here.
    /// The UI loop passes this into [`Backend::weixin_tx`] to the daemon.
    #[cfg(feature = "weixin")]
    pub weixin_tx: mpsc::UnboundedSender<WeixinBackendRequest>,
    /// Session-mutable sandbox roots shared with the agent's fs tools.
    /// The UI mutates this in place via `/add-dir` to grant the agent
    /// access to directories outside the workspace at runtime.
    pub session_roots: SharedSandboxRoots,
    _worker: JoinHandle<()>,
}

impl Backend {
    pub fn spawn() -> Self {
        #[cfg(feature = "skill-hub")]
        {
            let (state, skill_install_rx, session_roots) =
                crate::runtime_builder::build_runtime_for_tui();
            Self::spawn_with_state_and_skill_rx(state, skill_install_rx, session_roots)
        }
        #[cfg(not(feature = "skill-hub"))]
        {
            let (state, session_roots) = build_runtime();
            Self::spawn_with_state(state, session_roots)
        }
    }

    pub fn spawn_with_runtime(rt: AgentRuntime) -> Self {
        Self::spawn_with_state(
            RuntimeBuild::Ready(Some(Box::new(rt))),
            new_shared_sandbox_roots(),
        )
    }

    fn spawn_with_state(state: RuntimeBuild, session_roots: SharedSandboxRoots) -> Self {
        let (action_tx, action_rx) = mpsc::unbounded_channel::<UserAction>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (perm_tx, perm_rx) = mpsc::unbounded_channel::<PermissionRequest>();
        // Dummy skill-install channel: sender dropped immediately → receiver never fires.
        let (_dummy_skill_tx, skill_install_rx) = mpsc::unbounded_channel::<SkillInstallEvent>();
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let cancel_notify = Arc::new(tokio::sync::Notify::new());
        let permission_enabled = Arc::new(AtomicBool::new(false));
        #[cfg(feature = "weixin")]
        let (weixin_tx, weixin_rx) = mpsc::unbounded_channel::<WeixinBackendRequest>();

        let worker = tokio::spawn(worker_loop(
            state,
            action_rx,
            event_tx,
            perm_tx,
            cancel_flag.clone(),
            cancel_notify.clone(),
            permission_enabled.clone(),
            #[cfg(feature = "weixin")]
            weixin_rx,
        ));

        Self {
            action_tx,
            event_rx,
            perm_rx,
            cancel_flag,
            permission_enabled,
            #[cfg(feature = "weixin")]
            weixin_tx,
            skill_install_rx,
            session_roots,
            _worker: worker,
        }
    }

    #[cfg(feature = "skill-hub")]
    fn spawn_with_state_and_skill_rx(
        state: RuntimeBuild,
        skill_install_rx: mpsc::UnboundedReceiver<SkillInstallEvent>,
        session_roots: SharedSandboxRoots,
    ) -> Self {
        let (action_tx, action_rx) = mpsc::unbounded_channel::<UserAction>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (perm_tx, perm_rx) = mpsc::unbounded_channel::<PermissionRequest>();
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let cancel_notify = Arc::new(tokio::sync::Notify::new());
        let permission_enabled = Arc::new(AtomicBool::new(false));
        #[cfg(feature = "weixin")]
        let (weixin_tx, weixin_rx) = mpsc::unbounded_channel::<WeixinBackendRequest>();

        let worker = tokio::spawn(worker_loop(
            state,
            action_rx,
            event_tx,
            perm_tx,
            cancel_flag.clone(),
            cancel_notify.clone(),
            permission_enabled.clone(),
            #[cfg(feature = "weixin")]
            weixin_rx,
        ));

        Self {
            action_tx,
            event_rx,
            perm_rx,
            cancel_flag,
            permission_enabled,
            #[cfg(feature = "weixin")]
            weixin_tx,
            skill_install_rx,
            session_roots,
            _worker: worker,
        }
    }
}

struct TuiEventSink {
    tx: mpsc::UnboundedSender<UiEvent>,
}

#[async_trait]
impl EventSink for TuiEventSink {
    async fn emit(&self, event: AgentEvent) {
        if let Some(ev) = map_agent_event(event) {
            let _ = self.tx.send(ev);
        }
    }
}

pub fn map_agent_event(event: AgentEvent) -> Option<UiEvent> {
    match event {
        AgentEvent::PartialToken { text, .. } => Some(UiEvent::AssistantPartial { text }),
        AgentEvent::PartialReasoning { text, .. } => Some(UiEvent::ReasoningPartial { text }),
        AgentEvent::Reasoning { text, .. } => Some(UiEvent::Reasoning { content: text }),
        AgentEvent::AssistantText { text, .. } => Some(UiEvent::AssistantMessage { content: text }),
        AgentEvent::ToolCall {
            id,
            name,
            arguments,
            ..
        } => Some(UiEvent::ToolCall {
            id,
            name,
            arguments,
        }),
        AgentEvent::ToolResult {
            id,
            name,
            output,
            is_error,
            ..
        } => {
            let success = !is_error;
            Some(UiEvent::ToolResult {
                id,
                name,
                output,
                success,
            })
        }
        AgentEvent::Usage {
            input_tokens,
            output_tokens,
            cache_hit_tokens,
            cache_miss_tokens,
            ..
        } => Some(UiEvent::Usage {
            input_tokens: input_tokens as u64,
            output_tokens: output_tokens as u64,
            cache_hit_tokens: cache_hit_tokens as u64,
            cache_miss_tokens: cache_miss_tokens as u64,
        }),
        AgentEvent::Latency { llm_ms, .. } => Some(UiEvent::Latency { llm_ms }),
        AgentEvent::Compacted { removed, kept, .. } => Some(UiEvent::Compacted { removed, kept }),
        AgentEvent::TurnFinished { .. } => Some(UiEvent::TurnFinished),
        AgentEvent::PlanProposed {
            plan_text,
            tool_calls,
        } => Some(UiEvent::PlanProposed {
            plan_text,
            tool_calls,
        }),
        AgentEvent::PlanConfirmed => Some(UiEvent::PlanConfirmed),
        AgentEvent::PlanRejected { reason } => Some(UiEvent::PlanRejected { reason }),
        // Goal-202: plan-mode pre-confirmation events.
        AgentEvent::PlanModeRequested { reason } => Some(UiEvent::PlanModeRequested { reason }),
        AgentEvent::PlanModeApproved => Some(UiEvent::PlanModeApproved),
        AgentEvent::PlanModeRejected { reason } => Some(UiEvent::PlanModeRejected { reason }),
        // Goal-167: forward todo updates to the UI.
        AgentEvent::TodoUpdated { todos } => Some(UiEvent::TodoUpdated { todos }),

        // Goal-168: forward goal-loop events.
        AgentEvent::GoalContinuing { reason, turns } => {
            Some(UiEvent::GoalContinuing { reason, turns })
        }
        AgentEvent::GoalAchieved { condition, turns } => {
            Some(UiEvent::GoalAchieved { condition, turns })
        }
        AgentEvent::GoalCleared => Some(UiEvent::GoalCleared),

        // Goal-210: hook progress events.
        AgentEvent::HookStarted {
            hook_event,
            hook_name,
            status_message,
        } => Some(UiEvent::HookStarted {
            hook_event,
            hook_name,
            status_message,
        }),
        AgentEvent::HookProgress {
            hook_event,
            hook_name,
            last_line,
        } => Some(UiEvent::HookProgress {
            hook_event,
            hook_name,
            last_line,
        }),
        AgentEvent::HookFinished {
            hook_event,
            hook_name,
            outcome,
            duration_ms,
        } => Some(UiEvent::HookFinished {
            hook_event,
            hook_name,
            outcome,
            duration_ms,
        }),
        AgentEvent::HookSystemMessage { text } => Some(UiEvent::HookSystemMessage { text }),

        _ => None,
    }
}

// ── Goal-161: TuiPermissionHook ──────────────────────────────────────────────

/// Forwards tool-permission requests to the UI via a side-channel and blocks
/// until the user responds. When `enabled` is `false`, auto-allows all calls.
struct TuiPermissionHook {
    tx: mpsc::UnboundedSender<PermissionRequest>,
    enabled: Arc<AtomicBool>,
}

#[async_trait]
impl PermissionHook for TuiPermissionHook {
    async fn check(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> recursive::agent::PermissionDecision {
        use recursive::agent::PermissionDecision;
        if !self.enabled.load(Ordering::Relaxed) {
            return PermissionDecision::Allow;
        }
        let args_preview = recursive::tools::args_preview_for_permission(args);
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<bool>();
        let req = PermissionRequest {
            tool_name: tool_name.to_string(),
            args_preview,
            reply: reply_tx,
        };
        if self.tx.send(req).is_err() {
            return PermissionDecision::Allow; // UI dropped — allow so agent isn't stuck.
        }
        if reply_rx.await.unwrap_or(false) {
            PermissionDecision::Allow
        } else {
            PermissionDecision::Deny("denied by user".to_string())
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn worker_loop(
    mut state: RuntimeBuild,
    mut action_rx: mpsc::UnboundedReceiver<UserAction>,
    event_tx: mpsc::UnboundedSender<UiEvent>,
    perm_tx: mpsc::UnboundedSender<PermissionRequest>,
    cancel_flag: Arc<AtomicBool>,
    cancel_notify: Arc<tokio::sync::Notify>,
    permission_enabled: Arc<AtomicBool>,
    #[cfg(feature = "weixin")] mut weixin_rx: mpsc::UnboundedReceiver<WeixinBackendRequest>,
) {
    if let RuntimeBuild::Ready(rt_opt) = &mut state {
        let Some(rt) = rt_opt.as_mut() else {
            tracing::warn!("backend: runtime not initialized in worker_loop init");
            return;
        };
        rt.set_event_sink(Arc::new(TuiEventSink {
            tx: event_tx.clone(),
        }));
        // Goal-161: wire up the permission hook.
        rt.set_permission_hook(Arc::new(TuiPermissionHook {
            tx: perm_tx,
            enabled: permission_enabled,
        }));
        // Signal the UI that the runtime is ready — drives App::connected = true.
        let _ = event_tx.send(UiEvent::RuntimeReady);
    }

    let bash_registry = build_bash_registry(&resolve_workspace_root());
    let bash_seq = AtomicU64::new(0);

    // Lazily-created session writer for TUI interactive sessions.
    // Created on the first SendMessage so that sessions without any
    // actual user messages don't leave empty files on disk.
    // Wrapped in Arc<Mutex<>> so SessionPersistenceSink can share it
    // and write to disk in real-time on every MessageAppended event.
    let mut session_writer: Option<Arc<std::sync::Mutex<SessionWriter>>> = None;

    // Messages the user submitted while a turn was already running. The select
    // loop inside `run_turn_select_loop` buffers them here instead of dropping
    // them, and this loop drains them FIFO once the current turn completes —
    // type-ahead queueing rather than silent message loss.
    let mut queued_messages: std::collections::VecDeque<String> = std::collections::VecDeque::new();

    loop {
        // Drain any messages queued during the previous turn before blocking on
        // new input, so type-ahead is processed in submission order.
        let action = if let Some(text) = queued_messages.pop_front() {
            Either::Left(UserAction::SendMessage(text))
        } else {
            // Select on both the user-action channel and the WeChat side-channel.
            // WeChat messages processed here behave like SendMessage turns but
            // without plan-mode interaction.
            #[cfg(feature = "weixin")]
            {
                tokio::select! {
                    action = action_rx.recv() => {
                        match action {
                            Some(a) => Either::Left(a),
                            None => break,
                        }
                    }
                    wx_req = weixin_rx.recv() => {
                        match wx_req {
                            Some(r) => Either::Right(r),
                            None => continue,
                        }
                    }
                }
            }
            #[cfg(not(feature = "weixin"))]
            {
                match action_rx.recv().await {
                    Some(a) => Either::<UserAction, ()>::Left(a),
                    None => break,
                }
            }
        };

        #[cfg(feature = "weixin")]
        if let Either::Right(wx_req) = action {
            // Notify TUI of the incoming WeChat message.
            let _ = event_tx.send(UiEvent::WeixinMessage {
                user_id: wx_req.user_id.clone(),
                text: wx_req.text.clone(),
            });
            // Run the turn and send response back to WeChat daemon.
            if let RuntimeBuild::Ready(rt_opt) = &mut state {
                let text = wx_req.text.clone();
                let Some(rt) = rt_opt.take() else {
                    tracing::warn!("backend: runtime not available for weixin task");
                    continue;
                };
                let rt_shared = Arc::new(tokio::sync::Mutex::new(rt));
                let rt_clone = rt_shared.clone();
                let handle = tokio::task::spawn(async move {
                    let mut g = rt_clone.lock().await;
                    g.enqueue(text).await
                });
                let result = handle.await;
                let Ok(recovered) = Arc::try_unwrap(rt_shared) else {
                    tracing::error!("backend: arc has multiple owners after weixin task");
                    continue;
                };
                let recovered = recovered.into_inner();
                *rt_opt = Some(recovered);
                let _ = event_tx.send(UiEvent::TurnFinished);
                let final_text = match result {
                    Ok(Ok(Some(outcome))) => outcome.final_text,
                    _ => None,
                };
                let _ = wx_req.reply_tx.send(final_text);
            } else {
                let _ = wx_req.reply_tx.send(None);
            }
            continue;
        }

        let action = match action {
            Either::Left(a) => a,
            Either::Right(_) => unreachable!("weixin Right handled above"),
        };

        match action {
            UserAction::Shutdown => {
                if let Some(sw_arc) = session_writer.take() {
                    if let Ok(mut sw) = sw_arc.lock() {
                        let _ = sw.finish(SessionStatus::Completed);
                    }
                }
                break;
            }

            UserAction::SendMessage(text) => {
                if let RuntimeBuild::Ready(rt_opt) = &mut state {
                    let pre_turn_len = {
                        let Some(rt_ref) = rt_opt.as_ref() else {
                            tracing::warn!("backend: runtime not initialized in SendMessage");
                            continue;
                        };
                        rt_ref.transcript().len()
                    };

                    // On the first user message, create a SessionWriter and wire
                    // it into the runtime's event sink via SessionPersistenceSink
                    // so every MessageAppended event is written to disk in real-time.
                    if session_writer.is_none() {
                        let ws = resolve_workspace_root();
                        let goal: String = text.chars().take(200).collect();
                        let model = crate::cost::detect_model_name();
                        if let Ok(sw) = SessionWriter::create(&ws, &goal, &model, "tui") {
                            let sw_arc = Arc::new(std::sync::Mutex::new(sw));
                            // Build a composite sink: TUI display + session persistence.
                            let composite = Arc::new(CompositeSink::new([
                                Box::new(TuiEventSink {
                                    tx: event_tx.clone(),
                                }) as Box<dyn EventSink>,
                                Box::new(SessionPersistenceSink::new(sw_arc.clone())),
                            ]));
                            let Some(rt_mut) = rt_opt.as_mut() else {
                                tracing::warn!(
                                    "backend: runtime not available for session sink setup"
                                );
                                continue;
                            };
                            rt_mut.set_event_sink(composite);
                            session_writer = Some(sw_arc);
                        }
                    }

                    let Some(rt) = rt_opt.take() else {
                        tracing::warn!("backend: runtime not available for SendMessage task");
                        continue;
                    };
                    // Clone both gates before moving the runtime into the spawned task.
                    // This lets us signal plan approval/rejection via action_rx while
                    // the task is blocked inside exit_plan_mode or request_plan_mode.
                    let gate = rt.plan_approval_gate();
                    let plan_mode_request_gate = rt.plan_mode_request_gate();
                    let rt_shared = Arc::new(tokio::sync::Mutex::new(rt));
                    let rt_clone = rt_shared.clone();
                    cancel_flag.store(false, Ordering::SeqCst);
                    let cancel_clone = cancel_flag.clone();
                    // Re-arm the UI spinner for this turn. Required so a turn
                    // drained from the type-ahead queue shows progress even
                    // though the previous turn's TurnFinished cleared it.
                    let _ = event_tx.send(UiEvent::TurnStarted);
                    let mut handle = tokio::task::spawn(async move {
                        let mut g = rt_clone.lock().await;
                        g.enqueue(text).await.map(|_| ())
                    });
                    let aborted = run_turn_select_loop(
                        &mut handle,
                        &mut action_rx,
                        &event_tx,
                        &cancel_flag,
                        cancel_clone,
                        cancel_notify.clone(),
                        &gate,
                        &plan_mode_request_gate,
                        &mut queued_messages,
                    )
                    .await;
                    let Ok(recovered) = Arc::try_unwrap(rt_shared) else {
                        tracing::error!("backend: arc has multiple owners after SendMessage task");
                        continue;
                    };
                    let mut recovered = recovered.into_inner();
                    if aborted {
                        recovered.truncate_transcript(pre_turn_len);
                        // The user interrupted (or is shutting down); drop any
                        // type-ahead they queued during the aborted turn rather
                        // than running it against their wishes.
                        queued_messages.clear();
                    }
                    *rt_opt = Some(recovered);
                    let _ = event_tx.send(UiEvent::TurnFinished);
                    cancel_flag.store(false, Ordering::SeqCst);
                } else if let RuntimeBuild::Offline { reason } = &state {
                    let _ = event_tx.send(UiEvent::Error {
                        message: reason.clone(),
                    });
                    let _ = event_tx.send(UiEvent::TurnFinished);
                }
            }

            UserAction::RunShell(cmd) => {
                run_bash_command(&bash_registry, &bash_seq, cmd, &event_tx).await;
            }

            UserAction::ConfirmPlan => {
                if let RuntimeBuild::Ready(rt_opt) = &mut state {
                    let Some(rt_mut) = rt_opt.as_mut() else {
                        tracing::warn!("backend: runtime not available in ConfirmPlan");
                        continue;
                    };
                    rt_mut.confirm_plan();
                    let pre_turn_len = rt_mut.transcript().len();
                    let Some(rt) = rt_opt.take() else {
                        tracing::warn!("backend: runtime not available for ConfirmPlan task");
                        continue;
                    };
                    let gate = rt.plan_approval_gate();
                    let plan_mode_request_gate = rt.plan_mode_request_gate();
                    let rt_shared = Arc::new(tokio::sync::Mutex::new(rt));
                    let rt_clone = rt_shared.clone();
                    cancel_flag.store(false, Ordering::SeqCst);
                    let cancel_clone = cancel_flag.clone();
                    let mut handle = tokio::task::spawn(async move {
                        let mut g = rt_clone.lock().await;
                        g.run("").await.map(|_| ())
                    });
                    let aborted = run_turn_select_loop(
                        &mut handle,
                        &mut action_rx,
                        &event_tx,
                        &cancel_flag,
                        cancel_clone,
                        cancel_notify.clone(),
                        &gate,
                        &plan_mode_request_gate,
                        &mut queued_messages,
                    )
                    .await;
                    let Ok(recovered) = Arc::try_unwrap(rt_shared) else {
                        tracing::error!("backend: arc has multiple owners after ConfirmPlan task");
                        continue;
                    };
                    let mut recovered = recovered.into_inner();
                    if aborted {
                        recovered.truncate_transcript(pre_turn_len);
                        queued_messages.clear();
                    }
                    *rt_opt = Some(recovered);
                    let _ = event_tx.send(UiEvent::TurnFinished);
                    cancel_flag.store(false, Ordering::SeqCst);
                }
            }

            UserAction::RejectPlan(reason) => {
                if let RuntimeBuild::Ready(rt_opt) = &mut state {
                    let Some(rt) = rt_opt.as_mut() else {
                        tracing::warn!("backend: runtime not available in RejectPlan");
                        continue;
                    };
                    rt.reject_plan(&reason);
                }
            }

            // Goal-202: plan-mode pre-confirmation responses.
            UserAction::ApprovePlanMode => {
                if let RuntimeBuild::Ready(rt_opt) = &mut state {
                    let Some(rt) = rt_opt.as_ref() else {
                        tracing::warn!("backend: runtime not available in ApprovePlanMode");
                        continue;
                    };
                    rt.approve_plan_mode_request();
                }
            }
            UserAction::RejectPlanMode(reason) => {
                if let RuntimeBuild::Ready(rt_opt) = &mut state {
                    let Some(rt) = rt_opt.as_ref() else {
                        tracing::warn!("backend: runtime not available in RejectPlanMode");
                        continue;
                    };
                    rt.reject_plan_mode_request(&reason);
                }
            }

            UserAction::Compact => match &mut state {
                RuntimeBuild::Ready(rt_opt) => {
                    let Some(rt) = rt_opt.as_mut() else {
                        tracing::warn!("backend: runtime not available for Compact");
                        continue;
                    };
                    if let Err(e) = rt.compact_now().await {
                        let _ = event_tx.send(UiEvent::Error {
                            message: format!("compact failed: {e}"),
                        });
                    }
                }
                RuntimeBuild::Offline { .. } => {
                    let _ = event_tx.send(UiEvent::Error {
                        message: "compact unavailable in offline mode".into(),
                    });
                }
            },

            UserAction::SetPlanningMode(_on) => {
                // PlanFirst mode removed; this action is now a no-op.
                // Plan Mode 2.0 (enter_plan_mode / exit_plan_mode tools) handles
                // human-in-the-loop planning without a runtime-level mode flag.
            }

            UserAction::Interrupt => {
                cancel_flag.store(true, Ordering::SeqCst);
                cancel_notify.notify_waiters();
            }

            // Goal-168: start a condition-based autonomous loop.
            UserAction::SetGoal {
                condition,
                max_turns,
            } => {
                if let RuntimeBuild::Ready(rt_opt) = &mut state {
                    let pre_turn_len = {
                        let Some(rt_ref) = rt_opt.as_ref() else {
                            tracing::warn!("backend: runtime not initialized in SetGoal");
                            continue;
                        };
                        rt_ref.transcript().len()
                    };
                    let Some(rt) = rt_opt.take() else {
                        tracing::warn!("backend: runtime not available for SetGoal task");
                        continue;
                    };
                    let gate = rt.plan_approval_gate();
                    let plan_mode_request_gate = rt.plan_mode_request_gate();
                    let rt_shared = Arc::new(tokio::sync::Mutex::new(rt));
                    let rt_clone = rt_shared.clone();
                    cancel_flag.store(false, Ordering::SeqCst);
                    let cancel_clone = cancel_flag.clone();
                    let prompt = format!(
                        "Start working towards the following goal: {condition}\n\nContinue until the goal is achieved."
                    );
                    let mut handle = tokio::task::spawn(async move {
                        let mut g = rt_clone.lock().await;
                        g.run_goal_loop(prompt, condition, max_turns)
                            .await
                            .map(|_| ())
                    });
                    let aborted = run_turn_select_loop(
                        &mut handle,
                        &mut action_rx,
                        &event_tx,
                        &cancel_flag,
                        cancel_clone,
                        cancel_notify.clone(),
                        &gate,
                        &plan_mode_request_gate,
                        &mut queued_messages,
                    )
                    .await;
                    // Suppress goal-loop errors; they are surfaced via GoalContinuing/GoalAchieved.
                    let Ok(recovered) = Arc::try_unwrap(rt_shared) else {
                        tracing::error!("backend: arc has multiple owners after SetGoal task");
                        continue;
                    };
                    let mut recovered = recovered.into_inner();
                    if aborted {
                        recovered.truncate_transcript(pre_turn_len);
                        queued_messages.clear();
                    }
                    *rt_opt = Some(recovered);
                    let _ = event_tx.send(UiEvent::TurnFinished);
                    cancel_flag.store(false, Ordering::SeqCst);
                } else if let RuntimeBuild::Offline { reason } = &state {
                    let _ = event_tx.send(UiEvent::Error {
                        message: reason.clone(),
                    });
                }
            }

            // Goal-168: clear the active goal.
            UserAction::ClearGoal => {
                if let RuntimeBuild::Ready(rt_opt) = &mut state {
                    let Some(rt) = rt_opt.as_mut() else {
                        tracing::warn!("backend: runtime not available in ClearGoal");
                        continue;
                    };
                    rt.clear_goal().await;
                }
            }

            // Goal-171: load a saved session transcript into the runtime.
            UserAction::ResumeSession { session_dir } => {
                if let RuntimeBuild::Ready(rt_opt) = &mut state {
                    let Some(rt) = rt_opt.as_mut() else {
                        tracing::warn!("backend: runtime not available in ResumeSession");
                        continue;
                    };
                    match recursive::session::SessionReader::load_messages(&session_dir) {
                        Ok(messages) => {
                            let turn_count = messages.len();
                            let blocks = crate::app::render::blocks_from_messages(&messages);
                            rt.set_transcript(messages);
                            let session_id = session_dir
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("unknown")
                                .to_string();
                            let _ = event_tx.send(UiEvent::SessionResumed {
                                session_id,
                                turn_count,
                                blocks,
                            });
                        }
                        Err(e) => {
                            let _ = event_tx.send(UiEvent::Error {
                                message: format!("Failed to load session: {e}"),
                            });
                        }
                    }
                }
            }

            // Goal-173: list MCP servers.
            UserAction::ListMcpServers => {
                let workspace = resolve_workspace_root();
                let tx = event_tx.clone();
                tokio::spawn(async move {
                    let servers = recursive::mcp::discover_mcp_servers(&workspace)
                        .await
                        .unwrap_or_default();
                    let entries: Vec<crate::ui::modal::McpEntry> = servers
                        .iter()
                        .map(|s| {
                            let transport = if s.url.is_some() {
                                "http".to_string()
                            } else if !s.command.is_empty() {
                                "stdio".to_string()
                            } else {
                                "unknown".to_string()
                            };
                            crate::ui::modal::McpEntry {
                                name: s.name.clone(),
                                transport,
                                enabled: true,
                            }
                        })
                        .collect();
                    let _ = tx.send(UiEvent::McpServersLoaded { entries });
                });
            }

            // Goal-169: run an already-expanded skill prompt.
            UserAction::RunSkillPrompt { prompt } => {
                if let RuntimeBuild::Ready(rt_opt) = &mut state {
                    let pre_turn_len = {
                        let Some(rt_ref) = rt_opt.as_ref() else {
                            tracing::warn!("backend: runtime not initialized in RunSkillPrompt");
                            continue;
                        };
                        rt_ref.transcript().len()
                    };
                    let Some(rt) = rt_opt.take() else {
                        tracing::warn!("backend: runtime not available for RunSkillPrompt task");
                        continue;
                    };
                    let rt_shared = Arc::new(tokio::sync::Mutex::new(rt));
                    let rt_clone = rt_shared.clone();
                    cancel_flag.store(false, Ordering::SeqCst);
                    let cancel_clone = cancel_flag.clone();
                    let mut handle = tokio::task::spawn(async move {
                        let mut g = rt_clone.lock().await;
                        g.run(prompt).await.map(|_| ())
                    });
                    let aborted = tokio::select! {
                        res = &mut handle => {
                            if let Err(e) = res
                                .map_err(|e| recursive::Error::Internal {
                                    context: "tui::task_join".to_string(),
                                    message: e.to_string(),
                                })
                                .and_then(|r| r)
                            {
                                let _ = event_tx.send(UiEvent::Error { message: e.to_string() });
                            }
                            false
                        },
                        _ = wait_for_cancel(cancel_clone, cancel_notify.clone()) => {
                            handle.abort();
                            let _ = handle.await;
                            let _ = event_tx.send(UiEvent::Interrupted);
                            true
                        }
                    };
                    let Ok(recovered) = Arc::try_unwrap(rt_shared) else {
                        tracing::error!(
                            "backend: arc has multiple owners after RunSkillPrompt task"
                        );
                        continue;
                    };
                    let mut recovered = recovered.into_inner();
                    if aborted {
                        recovered.truncate_transcript(pre_turn_len);
                    }
                    *rt_opt = Some(recovered);
                    let _ = event_tx.send(UiEvent::TurnFinished);
                    cancel_flag.store(false, Ordering::SeqCst);
                } else if let RuntimeBuild::Offline { reason } = &state {
                    let _ = event_tx.send(UiEvent::Error {
                        message: reason.clone(),
                    });
                    let _ = event_tx.send(UiEvent::TurnFinished);
                }
            }
        }
    }
}

/// Wait until the cancel flag is set. Uses a `Notify` wakeup for near-zero
/// latency instead of a 100ms busy-poll sleep.
pub async fn wait_for_cancel(flag: Arc<AtomicBool>, notify: Arc<tokio::sync::Notify>) {
    loop {
        if flag.load(Ordering::SeqCst) {
            return;
        }
        notify.notified().await;
    }
}

/// Drive a spawned agent turn to completion while remaining responsive to
/// plan-approval and interrupt actions from the UI.
///
/// Returns `true` if the turn was aborted (cancel flag set or Shutdown received),
/// `false` if the task completed normally.
///
/// While a turn runs, `action_rx` is polled concurrently so that
/// `UserAction::ConfirmPlan` / `UserAction::RejectPlan` can signal the
/// `PlanApprovalGate` directly — unblocking `exit_plan_mode` without
/// requiring a new turn. `UserAction::Interrupt` sets the cancel flag.
/// Any other actions received during the turn are silently discarded
/// (they cannot be acted on without the runtime, which is inside the task).
#[allow(clippy::too_many_arguments)]
async fn run_turn_select_loop(
    handle: &mut tokio::task::JoinHandle<Result<(), recursive::Error>>,
    action_rx: &mut tokio::sync::mpsc::UnboundedReceiver<UserAction>,
    event_tx: &tokio::sync::mpsc::UnboundedSender<UiEvent>,
    cancel_flag: &Arc<AtomicBool>,
    cancel_clone: Arc<AtomicBool>,
    cancel_notify: Arc<tokio::sync::Notify>,
    gate: &Arc<recursive::tools::plan_mode::PlanApprovalGate>,
    plan_mode_request_gate: &Arc<recursive::tools::plan_mode::PlanModeRequestGate>,
    queued: &mut std::collections::VecDeque<String>,
) -> bool {
    loop {
        tokio::select! {
            biased;
            res = &mut *handle => {
                if let Err(e) = res
                    .map_err(|e| recursive::Error::Internal {
                        context: "tui::task_join".to_string(),
                        message: e.to_string(),
                    })
                    .and_then(|r| r)
                {
                    let _ = event_tx.send(UiEvent::Error { message: e.to_string() });
                }
                return false;
            }
            _ = wait_for_cancel(cancel_clone.clone(), cancel_notify.clone()) => {
                handle.abort();
                let _ = handle.await;
                let _ = event_tx.send(UiEvent::Interrupted);
                return true;
            }
            maybe_action = action_rx.recv() => {
                match maybe_action {
                    Some(UserAction::ConfirmPlan) => gate.approve(),
                    Some(UserAction::RejectPlan(reason)) => gate.reject(&reason),
                    // Goal-202: forward plan-mode entry approval/rejection to the
                    // PlanModeRequestGate while the runtime is inside the spawned
                    // task. Without this the gate never wakes and the tool blocks
                    // forever — the root cause of request_plan_mode hanging.
                    Some(UserAction::ApprovePlanMode) => plan_mode_request_gate.approve(),
                    Some(UserAction::RejectPlanMode(reason)) => {
                        plan_mode_request_gate.reject(&reason);
                    }
                    Some(UserAction::Interrupt) => {
                        cancel_flag.store(true, Ordering::SeqCst);
                        cancel_notify.notify_waiters();
                    }
                    Some(UserAction::Shutdown) => {
                        handle.abort();
                        let _ = handle.await;
                        return true;
                    }
                    // A message submitted while the turn is running is buffered
                    // for FIFO processing after the turn completes (type-ahead
                    // queueing) instead of being silently dropped.
                    Some(UserAction::SendMessage(text)) => {
                        queued.push_back(text);
                    }
                    // Other actions cannot be serviced while the runtime is
                    // inside the spawned task. Discard them — in normal usage
                    // only plan/interrupt actions arrive during a running turn.
                    Some(_) => {}
                    None => return false,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bash::build_bash_registry;

    #[test]
    fn map_partial_token_to_assistant_partial() {
        let ev = AgentEvent::PartialToken {
            text: "hel".into(),
            step: 0,
        };
        assert_eq!(
            map_agent_event(ev),
            Some(UiEvent::AssistantPartial { text: "hel".into() })
        );
    }

    #[test]
    fn map_assistant_text_to_assistant_message() {
        let ev = AgentEvent::AssistantText {
            text: "hi".into(),
            step: 0,
        };
        assert_eq!(
            map_agent_event(ev),
            Some(UiEvent::AssistantMessage {
                content: "hi".into()
            })
        );
    }

    #[test]
    fn map_tool_result_error_prefix_marks_failure() {
        let ev = AgentEvent::ToolResult {
            id: "1".into(),
            name: "Read".into(),
            output: "ERROR: missing".into(),
            step: 0,
            is_error: true,
        };
        let mapped = map_agent_event(ev).unwrap();
        match mapped {
            UiEvent::ToolResult { success, .. } => assert!(!success),
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn map_compacted_event() {
        let ev = AgentEvent::Compacted {
            removed: 5,
            kept: 2,
            summary_chars: 800,
            step: 0,
        };
        assert_eq!(
            map_agent_event(ev),
            Some(UiEvent::Compacted {
                removed: 5,
                kept: 2
            })
        );
    }

    #[test]
    fn map_plan_proposed_is_forwarded() {
        let ev = AgentEvent::PlanProposed {
            plan_text: "p".into(),
            tool_calls: vec![],
        };
        match map_agent_event(ev) {
            Some(UiEvent::PlanProposed {
                plan_text,
                tool_calls,
            }) => {
                assert_eq!(plan_text, "p");
                assert!(tool_calls.is_empty());
            }
            other => panic!("expected PlanProposed forward, got {other:?}"),
        }
    }

    #[test]
    fn map_plan_confirmed_is_forwarded() {
        let mapped = map_agent_event(AgentEvent::PlanConfirmed);
        assert_eq!(mapped, Some(UiEvent::PlanConfirmed));
    }

    #[test]
    fn map_plan_rejected_is_forwarded() {
        let mapped = map_agent_event(AgentEvent::PlanRejected {
            reason: "user rejected".into(),
        });
        assert_eq!(
            mapped,
            Some(UiEvent::PlanRejected {
                reason: "user rejected".into(),
            })
        );
    }

    #[tokio::test]
    #[cfg_attr(target_os = "windows", ignore)]
    async fn run_shell_action_dispatches_tool_and_emits_events() {
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = build_bash_registry(tmp.path());
        let (tx, mut rx) = mpsc::unbounded_channel::<UiEvent>();
        let seq = AtomicU64::new(0);
        run_bash_command(&registry, &seq, "echo bash-mode-works".into(), &tx).await;

        let call = rx.recv().await.expect("ToolCall event");
        match call {
            UiEvent::ToolCall {
                ref name,
                ref id,
                ref arguments,
            } => {
                assert_eq!(name, "Bash");
                assert!(id.starts_with("ui-bash-"));
                assert!(
                    arguments.contains("echo bash-mode-works"),
                    "arguments missing command: {arguments}"
                );
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }

        let res = rx.recv().await.expect("ToolResult event");
        match res {
            UiEvent::ToolResult {
                ref name,
                ref output,
                success,
                ..
            } => {
                assert_eq!(name, "Bash");
                assert!(success, "shell command should succeed");
                assert!(
                    output.contains("bash-mode-works"),
                    "output missing stdout: {output}"
                );
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_shell_action_works_when_runtime_offline() {
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = build_bash_registry(tmp.path());
        let (tx, mut rx) = mpsc::unbounded_channel::<UiEvent>();
        let seq = AtomicU64::new(42);
        run_bash_command(&registry, &seq, "echo offline".into(), &tx).await;

        let call = rx.recv().await.expect("ToolCall event");
        if let UiEvent::ToolCall { id, .. } = call {
            assert_eq!(id, "ui-bash-42");
        } else {
            panic!("expected ToolCall, got {call:?}");
        }
        let _ = rx.recv().await;
    }

    #[tokio::test]
    async fn run_with_cancel_flag_true_returns_quickly() {
        let flag = Arc::new(AtomicBool::new(true));
        let notify = Arc::new(tokio::sync::Notify::new());
        let started = std::time::Instant::now();
        let timed = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            wait_for_cancel(flag.clone(), notify.clone()),
        )
        .await;
        let elapsed = started.elapsed();
        assert!(timed.is_ok(), "wait_for_cancel didn't return in time");
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "wait_for_cancel was too slow: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn interrupt_action_sets_cancel_flag() {
        let prev_recursive = std::env::var("RECURSIVE_API_KEY").ok();
        let prev_openai = std::env::var("OPENAI_API_KEY").ok();
        std::env::remove_var("RECURSIVE_API_KEY");
        std::env::remove_var("OPENAI_API_KEY");

        let backend = Backend::spawn();
        assert!(!backend.cancel_flag.load(Ordering::SeqCst));
        backend.action_tx.send(UserAction::Interrupt).unwrap();

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            if backend.cancel_flag.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            backend.cancel_flag.load(Ordering::SeqCst),
            "Interrupt should set cancel_flag"
        );
        let _ = backend.action_tx.send(UserAction::Shutdown);

        if let Some(v) = prev_recursive {
            std::env::set_var("RECURSIVE_API_KEY", v);
        }
        if let Some(v) = prev_openai {
            std::env::set_var("OPENAI_API_KEY", v);
        }
    }
}
