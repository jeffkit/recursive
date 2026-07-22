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
use crate::runtime_builder::{RuntimeBuild, TuiRuntime};

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
    /// Goal-323: shared wakeup slot for loop-mode agent self-scheduling.
    pub wakeup_slot: recursive::tools::WakeupSlot,
    /// Goal-323: shared background job manager for loop-mode completion detection.
    pub bg_manager: Arc<tokio::sync::Mutex<recursive::tools::BackgroundJobManager>>,
    _worker: JoinHandle<()>,
}

impl Backend {
    pub fn spawn() -> Self {
        #[cfg(feature = "skill-hub")]
        {
            let (tui_rt, skill_install_rx) = crate::runtime_builder::build_runtime_for_tui();
            Self::spawn_with_state_and_skill_rx(tui_rt, skill_install_rx)
        }
        #[cfg(not(feature = "skill-hub"))]
        {
            let tui_rt = build_runtime();
            Self::spawn_with_state(tui_rt)
        }
    }

    pub fn spawn_with_runtime(rt: AgentRuntime) -> Self {
        Self::spawn_with_state(TuiRuntime {
            state: RuntimeBuild::Ready(Some(Box::new(rt))),
            session_roots: new_shared_sandbox_roots(),
            wakeup_slot: Arc::new(std::sync::Mutex::new(None)),
            bg_manager: Arc::new(tokio::sync::Mutex::new(
                recursive::tools::BackgroundJobManager::new(),
            )),
        })
    }

    fn spawn_with_state(tui_rt: TuiRuntime) -> Self {
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

        let session_roots = tui_rt.session_roots.clone();
        let wakeup_slot = tui_rt.wakeup_slot.clone();
        let bg_manager = tui_rt.bg_manager.clone();

        let worker = tokio::spawn(worker_loop(
            tui_rt.state,
            action_rx,
            event_tx,
            perm_tx,
            cancel_flag.clone(),
            cancel_notify.clone(),
            permission_enabled.clone(),
            wakeup_slot.clone(),
            bg_manager.clone(),
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
            wakeup_slot,
            bg_manager,
            _worker: worker,
        }
    }

    #[cfg(feature = "skill-hub")]
    fn spawn_with_state_and_skill_rx(
        tui_rt: TuiRuntime,
        skill_install_rx: mpsc::UnboundedReceiver<SkillInstallEvent>,
    ) -> Self {
        let (action_tx, action_rx) = mpsc::unbounded_channel::<UserAction>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (perm_tx, perm_rx) = mpsc::unbounded_channel::<PermissionRequest>();
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let cancel_notify = Arc::new(tokio::sync::Notify::new());
        let permission_enabled = Arc::new(AtomicBool::new(false));
        #[cfg(feature = "weixin")]
        let (weixin_tx, weixin_rx) = mpsc::unbounded_channel::<WeixinBackendRequest>();

        let session_roots = tui_rt.session_roots.clone();
        let wakeup_slot = tui_rt.wakeup_slot.clone();
        let bg_manager = tui_rt.bg_manager.clone();

        let worker = tokio::spawn(worker_loop(
            tui_rt.state,
            action_rx,
            event_tx,
            perm_tx,
            cancel_flag.clone(),
            cancel_notify.clone(),
            permission_enabled.clone(),
            wakeup_slot.clone(),
            bg_manager.clone(),
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
            wakeup_slot,
            bg_manager,
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
        AgentEvent::ContextBreakdown { breakdown, .. } => {
            Some(UiEvent::ContextBreakdown { breakdown })
        }
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

// ── Goal-323: Loop arbiter types ───────────────────────────────────────────

/// Active event-driven loop state.
#[derive(Debug)]
struct LoopState {
    active: bool,
    turns_run: u32,
    max_turns: u32, // 0 = unlimited
}

/// Decision returned by the loop arbiter.
enum ArbiterDecision {
    /// Run a turn with this prompt.
    Run {
        prompt: String,
        source: String,
        delay_secs: Option<u64>,
    },
    /// Stop the loop (user requested, or max turns reached).
    Stop,
    /// No trigger ready yet — stay idle.
    Idle,
    /// Forward this action back to the main loop for processing
    /// (e.g. SetGoal, Compact, RunShell — actions not relevant to
    /// the arbiter's trigger selection).
    Forward(UserAction),
}

/// Wait for a wakeup request from the agent's `schedule_wakeup` tool.
///
/// Returns `None` when the slot is empty (nothing scheduled) — the caller
/// should treat this as `Idle`. When a request is present, returns it and
/// clears the slot.
fn wait_wakeup(slot: &recursive::tools::WakeupSlot) -> Option<recursive::tools::WakeupRequest> {
    slot.lock().ok().and_then(|mut s| s.take())
}

/// Goal-173: classify an MCP server's transport from its config fields.
/// Extracted as a pure function so the stdio/http/unknown branching is
/// unit-testable without spinning up `discover_mcp_servers` against a
/// real workspace.
fn mcp_server_transport(s: &recursive::mcp::McpServer) -> String {
    if s.url.is_some() {
        "http".to_string()
    } else if !s.command.is_empty() {
        "stdio".to_string()
    } else {
        "unknown".to_string()
    }
}

/// Goal-230 (weixin): reduce a spawned weixin turn's nested result to the
/// final assistant text (if any). Gated behind `#[cfg(feature = "weixin")]`
/// so the body is dead code under the default / gate test feature set
/// (`--features recursive/test-utils`) and no mutants are generated for it
/// there; under `--all-features` the body is live and its mutants are
/// killable.
#[cfg(feature = "weixin")]
fn weixin_final_text(
    result: std::result::Result<
        recursive::Result<Option<recursive::RuntimeOutcome>>,
        tokio::task::JoinError,
    >,
) -> Option<String> {
    match result {
        Ok(Ok(Some(outcome))) => outcome.final_text,
        _ => None,
    }
}

/// The loop arbiter: select among user actions, bg completions, and wakeups.
///
/// Priority order (biased):
/// 1. User actions (StopLoop, Interrupt, Shutdown, SendMessage, LoopTrigger)
/// 2. Background job completion
/// 3. Scheduled wakeup
async fn loop_arbiter(
    action_rx: &mut mpsc::UnboundedReceiver<UserAction>,
    wakeup_slot: &recursive::tools::WakeupSlot,
    bg_manager: &Arc<tokio::sync::Mutex<recursive::tools::BackgroundJobManager>>,
    queued_messages: &mut std::collections::VecDeque<String>,
) -> ArbiterDecision {
    // Priority 1: drain any pending user actions (non-blocking).
    loop {
        match action_rx.try_recv() {
            Ok(UserAction::StopLoop | UserAction::Interrupt | UserAction::Shutdown) => {
                return ArbiterDecision::Stop;
            }
            Ok(UserAction::SendMessage(text)) => {
                queued_messages.push_back(text);
                // Continue draining in case there's more.
            }
            Ok(UserAction::LoopTrigger { source, prompt }) => {
                return ArbiterDecision::Run {
                    prompt: format!("[trigger:{source}] {prompt}"),
                    source,
                    delay_secs: None,
                };
            }
            Ok(UserAction::StartLoop { .. }) => {
                // Already in loop mode — ignore duplicate.
            }
            Ok(_) => {}      // Other actions not relevant in loop mode.
            Err(_) => break, // Channel closed or empty.
        }
    }

    // Priority 2..N: block on the first trigger.
    let bg_notify = {
        let mgr = bg_manager.lock().await;
        mgr.completed_notify()
    };

    tokio::select! {
        biased;
        // User actions during wait.
        action = action_rx.recv() => {
            match action {
                Some(UserAction::StopLoop | UserAction::Interrupt | UserAction::Shutdown) => {
                    ArbiterDecision::Stop
                }
                Some(UserAction::SendMessage(text)) => {
                    queued_messages.push_back(text);
                    ArbiterDecision::Idle
                }
                Some(UserAction::LoopTrigger { source, prompt }) => {
                    ArbiterDecision::Run {
                        prompt: format!("[trigger:{source}] {prompt}"),
                        source,
                        delay_secs: None,
                    }
                }
                Some(UserAction::StartLoop { .. }) => {
                    // Already in loop mode — ignore duplicate.
                    ArbiterDecision::Idle
                }
                Some(other) => {
                    // Forward unknown actions to the main loop for processing.
                    ArbiterDecision::Forward(other)
                }
                None => ArbiterDecision::Stop,
            }
        }
        // Background job completed.
        _ = bg_notify.notified() => {
            let mut mgr = bg_manager.lock().await;
            if let Some((id, output)) = mgr.take_completed() {
                ArbiterDecision::Run {
                    prompt: format!("Background job '{}' completed:\n{}", id, output),
                    source: "bg-complete".to_string(),
                    delay_secs: None,
                }
            } else {
                // Spurious wakeup — nothing to do.
                ArbiterDecision::Idle
            }
        }
        // Scheduled wakeup.
        req = async {
            // Poll the wakeup slot periodically until something arrives.
            loop {
                if let Some(req) = wait_wakeup(wakeup_slot) {
                    return Some(req);
                }
                // Brief sleep to avoid busy-looping; the other branches
                // in the select! will still preempt this.
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        } => {
            match req {
                Some(req) => {
                    tokio::time::sleep(req.delay).await;
                    ArbiterDecision::Run {
                        prompt: req.prompt.clone(),
                        source: "wakeup".to_string(),
                        delay_secs: Some(req.delay.as_secs()),
                    }
                }
                None => ArbiterDecision::Idle,
            }
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
    wakeup_slot: recursive::tools::WakeupSlot,
    bg_manager: Arc<tokio::sync::Mutex<recursive::tools::BackgroundJobManager>>,
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
    } else if let RuntimeBuild::Offline { reason } = &state {
        // No usable runtime was built (missing API key / preset, or
        // provider construction failed). Tell the UI immediately so the
        // status bar can show `offline` and the transcript can surface an
        // actionable setup hint — otherwise the UI stays stuck at
        // "starting…" with no explanation. The same reason is re-sent as
        // `UiEvent::Error` when the user tries to send a message.
        let _ = event_tx.send(UiEvent::RuntimeOffline {
            reason: reason.clone(),
        });
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

    // Goal-323: event-driven loop state. None = loop not active.
    let mut loop_state: Option<LoopState> = None;

    loop {
        // Goal-323: enforce the loop's max-turns cap (0 = unlimited) before
        // scheduling the next turn — whether it drains from the type-ahead
        // queue or is chosen by the arbiter.
        if let Some(ls) = loop_state.as_ref() {
            if ls.max_turns > 0 && ls.turns_run >= ls.max_turns {
                let _ = event_tx.send(UiEvent::LoopStopped);
                loop_state = None;
                continue;
            }
        }
        // Drain any messages queued during the previous turn before blocking on
        // new input, so type-ahead is processed in submission order.
        let action = if let Some(text) = queued_messages.pop_front() {
            if let Some(ls) = loop_state.as_mut() {
                ls.turns_run += 1;
            }
            Either::Left(UserAction::SendMessage(text))
        } else if loop_state.as_ref().is_some_and(|ls| ls.active) {
            // Goal-323: event-driven loop mode — poll the arbiter for next prompt.
            let decision = loop_arbiter(
                &mut action_rx,
                &wakeup_slot,
                &bg_manager,
                &mut queued_messages,
            )
            .await;
            match decision {
                ArbiterDecision::Run {
                    prompt,
                    source,
                    delay_secs,
                } => {
                    if let Some(ls) = loop_state.as_mut() {
                        let _ = event_tx.send(UiEvent::LoopTurnScheduled { source, delay_secs });
                        ls.turns_run += 1;
                    }
                    // Use the prompt as a SendMessage to drive one turn through
                    // the existing spawn → run_turn_select_loop → recover path.
                    // The SendMessage handler will emit TurnStarted/TurnFinished.
                    Either::Left(UserAction::SendMessage(prompt))
                }
                ArbiterDecision::Stop => {
                    let _ = event_tx.send(UiEvent::LoopStopped);
                    loop_state = None;
                    continue;
                }
                ArbiterDecision::Idle => {
                    let _ = event_tx.send(UiEvent::LoopIdle);
                    continue;
                }
                ArbiterDecision::Forward(action) => {
                    // Forward the action to the main match block for processing.
                    Either::Left(action)
                }
            }
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
                let final_text = weixin_final_text(result);
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
            // ── Goal-323: event-driven loop actions ───────────────────────
            UserAction::StartLoop { goal, max_turns } => {
                if let RuntimeBuild::Ready(rt_opt) = &mut state {
                    let Some(rt_ref) = rt_opt.as_ref() else {
                        tracing::warn!("backend: runtime not initialized in StartLoop");
                        continue;
                    };
                    // Mutual exclusion: if goal loop is active, reject.
                    if rt_ref.current_goal().is_some() {
                        let _ = event_tx.send(UiEvent::Error {
                            message: "Cannot start loop: a goal loop is already active. Use /goal clear first.".into(),
                        });
                        continue;
                    }
                    if loop_state.is_some() {
                        let _ = event_tx.send(UiEvent::Error {
                            message: "Loop is already active. Use /loop stop first.".into(),
                        });
                        continue;
                    }
                    loop_state = Some(LoopState {
                        active: true,
                        turns_run: 0,
                        max_turns,
                    });

                    // Ensure session writer is created so loop turns are persisted.
                    if session_writer.is_none() {
                        let ws = resolve_workspace_root();
                        let goal_slug: String = goal.chars().take(200).collect();
                        let model = crate::cost::detect_model_name();
                        if let Ok(sw) = SessionWriter::create(&ws, &goal_slug, &model, "tui") {
                            let sw_arc = Arc::new(std::sync::Mutex::new(sw));
                            let composite = Arc::new(CompositeSink::new([
                                Box::new(TuiEventSink {
                                    tx: event_tx.clone(),
                                }) as Box<dyn EventSink>,
                                Box::new(SessionPersistenceSink::new(sw_arc.clone())),
                            ]));
                            let Some(rt_mut) = rt_opt.as_mut() else {
                                continue;
                            };
                            rt_mut.set_event_sink(composite);
                            session_writer = Some(sw_arc);
                        }
                    }

                    let _ = event_tx.send(UiEvent::LoopStarted { goal: goal.clone() });

                    // Kick off the first turn by sending the goal as a message.
                    queued_messages.push_back(goal);
                } else if let RuntimeBuild::Offline { reason } = &state {
                    let _ = event_tx.send(UiEvent::Error {
                        message: reason.clone(),
                    });
                }
            }

            UserAction::StopLoop => {
                loop_state = None;
                let _ = event_tx.send(UiEvent::LoopStopped);
            }

            UserAction::LoopTrigger { source, prompt } => {
                if loop_state.is_some() {
                    queued_messages.push_back(format!("[trigger:{source}] {prompt}"));
                } else {
                    let _ = event_tx.send(UiEvent::Error {
                        message: "No active loop. Use /loop start first.".into(),
                    });
                }
            }

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
                // Goal-323: mutual exclusion — reject SetGoal during event loop.
                if loop_state.is_some() {
                    let _ = event_tx.send(UiEvent::Error {
                        message: "Cannot set goal: an event loop is active. Use /loop stop first."
                            .into(),
                    });
                    continue;
                }
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
                        .map(|s| crate::ui::modal::McpEntry {
                            name: s.name.clone(),
                            transport: mcp_server_transport(s),
                            enabled: true,
                        })
                        .collect();
                    let _ = tx.send(UiEvent::McpServersLoaded { entries });
                });
            }

            // /model picker: hot-swap the LLM provider between turns. The
            // runtime is owned outright by the worker here (no turn task is
            // running), so `set_llm` is safe. Errors (unknown preset, no API
            // key) surface as `UiEvent::Error` so the user can react.
            UserAction::SwitchModel { preset_id, model } => match &mut state {
                RuntimeBuild::Ready(rt_opt) => {
                    let Some(rt) = rt_opt.as_mut() else {
                        tracing::warn!("backend: runtime not available in SwitchModel");
                        continue;
                    };
                    match crate::runtime_builder::build_provider_for_model(&preset_id, &model) {
                        Ok(provider) => {
                            rt.set_llm(provider);
                            let _ = event_tx.send(UiEvent::ModelSwitched {
                                preset_id: preset_id.clone(),
                                model: model.clone(),
                            });
                        }
                        Err(e) => {
                            let _ = event_tx.send(UiEvent::Error {
                                message: format!("switch model failed: {e}"),
                            });
                        }
                    }
                }
                RuntimeBuild::Offline { reason } => {
                    let _ = event_tx.send(UiEvent::Error {
                        message: format!("switch model unavailable offline: {reason}"),
                    });
                }
            },

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

    // weixin_final_text extracts the final assistant text from a turn
    // result. `FinishReason` is `#[non_exhaustive]`, so the outcome is built
    // via the core crate's `test_util::runtime_outcome_fixture`. Pins all
    // four mutants cargo-mutants surfaces for this fn (replace return with
    // None / Some("") / Some("xyzzy"), and delete the Ok(Ok(Some(_))) arm).
    #[cfg(feature = "weixin")]
    #[test]
    fn weixin_final_text_extracts_outcome_final_text() {
        let outcome = recursive::test_util::runtime_outcome_fixture(Some("hello".into()));
        let got = weixin_final_text(Ok(Ok(Some(outcome))));
        assert_eq!(got.as_deref(), Some("hello"));
    }

    #[cfg(feature = "weixin")]
    #[test]
    fn weixin_final_text_returns_none_when_no_outcome() {
        // Ok(Ok(None)) — no outcome produced → None. Pins the `_ => None`
        // fallback arm so a mutant that swaps it to `Some(...)` is caught.
        let none_outcome: std::result::Result<
            recursive::Result<Option<recursive::RuntimeOutcome>>,
            tokio::task::JoinError,
        > = Ok(Ok(None));
        assert_eq!(weixin_final_text(none_outcome), None);
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

    // ── Goal-170: real cancel-during-turn aborts the in-flight task,
    //    truncates the transcript, and emits UiEvent::Interrupted. The
    //    earlier `interrupt_action_sets_cancel_flag` only checks the flag
    //    flips; this one drives the full worker_loop path with a Ready
    //    runtime whose tool hangs, so the abort actually has something to
    //    cancel — covering the backend layer the in-process harness can't
    //    reach (it doesn't spin up a backend).
    use recursive::llm::{Completion, MockProvider, ToolCall};
    use recursive::tools::{Tool, ToolRegistry};
    use recursive::AgentRuntime;
    use serde_json::{json, Value};

    /// A tool that never returns — `std::future::pending` parks forever so
    /// the turn task stays in-flight until the worker aborts it.
    struct HangTool;

    #[async_trait::async_trait]
    impl Tool for HangTool {
        fn spec(&self) -> recursive::llm::ToolSpec {
            recursive::llm::ToolSpec {
                name: "hang".into(),
                description: "test tool that never returns".into(),
                parameters: json!({"type":"object","properties":{}}),
            }
        }
        async fn execute(&self, _args: Value) -> recursive::error::Result<String> {
            std::future::pending::<()>().await;
            Ok("never".into())
        }
    }

    #[tokio::test]
    #[cfg_attr(target_os = "windows", ignore)]
    async fn interrupt_aborts_running_turn_and_emits_interrupted() {
        let notify = Arc::new(tokio::sync::Notify::new());
        let llm = Arc::new(
            MockProvider::new(vec![Completion {
                content: "calling hang".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "hang".into(),
                    arguments: json!({}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            }])
            .with_on_complete(notify.clone()),
        );
        let tools = ToolRegistry::local().register(Arc::new(HangTool));
        let rt = AgentRuntime::builder()
            .llm(llm)
            .tools(tools)
            .build()
            .expect("runtime builds");

        let mut backend = Backend::spawn_with_runtime(rt);
        backend
            .action_tx
            .send(UserAction::SendMessage("hi".into()))
            .unwrap();

        // Wait until the mock completion has returned — at that point the
        // agent is dispatching the hang tool and the turn is genuinely
        // in-flight, so an Interrupt won't be cleared by the turn-start
        // `cancel_flag.store(false)` reset (line ~504/814 in worker_loop).
        // `notify.notified()` is single-use but re-entrant: if the mock
        // already fired, the first await returns immediately.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), notify.notified()).await;
        // Give the worker a beat to enter the tool dispatch path.
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        backend.action_tx.send(UserAction::Interrupt).unwrap();

        let mut got_interrupted = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(
                std::time::Duration::from_millis(100),
                backend.event_rx.recv(),
            )
            .await
            {
                Ok(Some(UiEvent::Interrupted)) => {
                    got_interrupted = true;
                    break;
                }
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(_) => continue,
            }
        }
        let _ = backend.action_tx.send(UserAction::Shutdown);
        assert!(
            got_interrupted,
            "Interrupt should abort the running turn and emit UiEvent::Interrupted"
        );
    }

    // ── Goal-323: Loop arbiter tests ──────────────────────────────────

    /// Smoke test: verify the LoopState type compiles and has expected defaults.
    #[test]
    fn loop_state_defaults() {
        let ls = LoopState {
            active: true,
            turns_run: 5,
            max_turns: 10,
        };
        assert!(ls.active);
        assert_eq!(ls.turns_run, 5);
        assert_eq!(ls.max_turns, 10);
    }

    #[tokio::test]
    async fn start_loop_emits_loop_started_and_runs_turn() {
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "working".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let rt = AgentRuntime::builder()
            .llm(llm)
            .build()
            .expect("runtime builds");
        let mut backend = Backend::spawn_with_runtime(rt);

        // Drain RuntimeReady.
        let _ =
            tokio::time::timeout(std::time::Duration::from_secs(1), backend.event_rx.recv()).await;

        backend
            .action_tx
            .send(UserAction::StartLoop {
                goal: "test goal".into(),
                max_turns: 2,
            })
            .unwrap();

        let mut seen_started = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(
                std::time::Duration::from_millis(200),
                backend.event_rx.recv(),
            )
            .await
            {
                Ok(Some(UiEvent::LoopStarted { .. })) => seen_started = true,
                Ok(Some(UiEvent::TurnFinished)) => break,
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        let _ = backend.action_tx.send(UserAction::Shutdown);
        assert!(seen_started, "expected LoopStarted");
    }

    #[tokio::test]
    async fn stop_loop_emits_loop_stopped() {
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "ok".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let rt = AgentRuntime::builder()
            .llm(llm)
            .build()
            .expect("runtime builds");
        let mut backend = Backend::spawn_with_runtime(rt);

        let _ =
            tokio::time::timeout(std::time::Duration::from_secs(1), backend.event_rx.recv()).await;

        // Start loop, let first turn complete.
        backend
            .action_tx
            .send(UserAction::StartLoop {
                goal: "test".into(),
                max_turns: 10,
            })
            .unwrap();

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(
                std::time::Duration::from_millis(100),
                backend.event_rx.recv(),
            )
            .await
            {
                Ok(Some(UiEvent::TurnFinished)) => break,
                Ok(Some(_)) => continue,
                _ => break,
            }
        }

        backend.action_tx.send(UserAction::StopLoop).unwrap();

        let mut seen_stopped = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(
                std::time::Duration::from_millis(200),
                backend.event_rx.recv(),
            )
            .await
            {
                Ok(Some(UiEvent::LoopStopped)) => {
                    seen_stopped = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        let _ = backend.action_tx.send(UserAction::Shutdown);
        assert!(seen_stopped, "expected LoopStopped");
    }

    #[tokio::test]
    async fn set_goal_rejected_during_loop() {
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "ok".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let rt = AgentRuntime::builder()
            .llm(llm)
            .build()
            .expect("runtime builds");
        let mut backend = Backend::spawn_with_runtime(rt);

        let _ =
            tokio::time::timeout(std::time::Duration::from_secs(1), backend.event_rx.recv()).await;

        backend
            .action_tx
            .send(UserAction::StartLoop {
                goal: "test".into(),
                max_turns: 10,
            })
            .unwrap();

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(
                std::time::Duration::from_millis(100),
                backend.event_rx.recv(),
            )
            .await
            {
                Ok(Some(UiEvent::TurnFinished)) => break,
                Ok(Some(_)) => continue,
                _ => break,
            }
        }

        backend
            .action_tx
            .send(UserAction::SetGoal {
                condition: "achieve".into(),
                max_turns: 5,
            })
            .unwrap();

        let mut got_error = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(
                std::time::Duration::from_millis(200),
                backend.event_rx.recv(),
            )
            .await
            {
                Ok(Some(UiEvent::Error { message })) => {
                    assert!(message.contains("event loop is active"));
                    got_error = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        let _ = backend.action_tx.send(UserAction::Shutdown);
        assert!(got_error, "expected Error for SetGoal during loop");
    }

    #[tokio::test]
    async fn loop_trigger_runs_turn() {
        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "first".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "triggered".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));
        let rt = AgentRuntime::builder()
            .llm(llm)
            .build()
            .expect("runtime builds");
        let mut backend = Backend::spawn_with_runtime(rt);

        let _ =
            tokio::time::timeout(std::time::Duration::from_secs(1), backend.event_rx.recv()).await;

        backend
            .action_tx
            .send(UserAction::StartLoop {
                goal: "goal".into(),
                max_turns: 0,
            })
            .unwrap();

        // Wait for the first turn to complete.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(
                std::time::Duration::from_millis(100),
                backend.event_rx.recv(),
            )
            .await
            {
                Ok(Some(UiEvent::TurnFinished)) => break,
                Ok(Some(_)) => continue,
                _ => break,
            }
        }

        // Send the trigger.
        backend
            .action_tx
            .send(UserAction::LoopTrigger {
                source: "manual".into(),
                prompt: "check".into(),
            })
            .unwrap();

        // Collect all events until the second TurnFinished.
        let mut got_triggered = false;
        let mut turn_finished_count = 0u32;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(
                std::time::Duration::from_millis(200),
                backend.event_rx.recv(),
            )
            .await
            {
                Ok(Some(UiEvent::LoopTurnScheduled { source, .. })) => {
                    if source == "manual" {
                        got_triggered = true;
                    }
                }
                Ok(Some(UiEvent::TurnFinished)) => {
                    turn_finished_count += 1;
                    if turn_finished_count >= 2 {
                        break;
                    }
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        let _ = backend.action_tx.send(UserAction::Shutdown);
        assert!(got_triggered, "expected LoopTurnScheduled source=manual");
    }

    // ── Pre-existing coverage: map_agent_event remaining variants, ─────
    //    TuiEventSink::emit, TuiPermissionHook, wait_wakeup.

    #[test]
    fn map_partial_reasoning_to_reasoning_partial() {
        let ev = AgentEvent::PartialReasoning {
            text: "th".into(),
            step: 0,
        };
        assert_eq!(
            map_agent_event(ev),
            Some(UiEvent::ReasoningPartial { text: "th".into() })
        );
    }

    #[test]
    fn map_reasoning_to_reasoning() {
        let ev = AgentEvent::Reasoning {
            text: "think".into(),
            step: 0,
        };
        assert_eq!(
            map_agent_event(ev),
            Some(UiEvent::Reasoning {
                content: "think".into()
            })
        );
    }

    #[test]
    fn map_tool_call_to_tool_call() {
        let ev = AgentEvent::ToolCall {
            name: "Read".into(),
            id: "c1".into(),
            arguments: "{}".into(),
            step: 0,
        };
        assert_eq!(
            map_agent_event(ev),
            Some(UiEvent::ToolCall {
                id: "c1".into(),
                name: "Read".into(),
                arguments: "{}".into(),
            })
        );
    }

    #[test]
    fn map_usage_to_usage() {
        let ev = AgentEvent::Usage {
            input_tokens: 10,
            output_tokens: 20,
            cache_hit_tokens: 30,
            cache_miss_tokens: 40,
            step: 0,
        };
        assert_eq!(
            map_agent_event(ev),
            Some(UiEvent::Usage {
                input_tokens: 10,
                output_tokens: 20,
                cache_hit_tokens: 30,
                cache_miss_tokens: 40,
            })
        );
    }

    #[test]
    fn map_latency_to_latency() {
        let ev = AgentEvent::Latency {
            step: 0,
            llm_ms: 123,
        };
        assert_eq!(map_agent_event(ev), Some(UiEvent::Latency { llm_ms: 123 }));
    }

    #[test]
    fn map_turn_finished_to_turn_finished() {
        let ev = AgentEvent::TurnFinished {
            reason: "done".into(),
            steps: 3,
        };
        assert_eq!(map_agent_event(ev), Some(UiEvent::TurnFinished));
    }

    #[test]
    fn map_plan_mode_requested() {
        let ev = AgentEvent::PlanModeRequested {
            reason: "why".into(),
        };
        assert_eq!(
            map_agent_event(ev),
            Some(UiEvent::PlanModeRequested {
                reason: "why".into()
            })
        );
    }

    #[test]
    fn map_plan_mode_approved() {
        assert_eq!(
            map_agent_event(AgentEvent::PlanModeApproved),
            Some(UiEvent::PlanModeApproved)
        );
    }

    #[test]
    fn map_plan_mode_rejected() {
        let ev = AgentEvent::PlanModeRejected {
            reason: "no".into(),
        };
        assert_eq!(
            map_agent_event(ev),
            Some(UiEvent::PlanModeRejected {
                reason: "no".into()
            })
        );
    }

    #[test]
    fn map_todo_updated() {
        let ev = AgentEvent::TodoUpdated { todos: vec![] };
        assert_eq!(
            map_agent_event(ev),
            Some(UiEvent::TodoUpdated { todos: vec![] })
        );
    }

    #[test]
    fn map_goal_continuing() {
        let ev = AgentEvent::GoalContinuing {
            reason: "not yet".into(),
            turns: 2,
        };
        assert_eq!(
            map_agent_event(ev),
            Some(UiEvent::GoalContinuing {
                reason: "not yet".into(),
                turns: 2
            })
        );
    }

    #[test]
    fn map_goal_achieved() {
        let ev = AgentEvent::GoalAchieved {
            condition: "done".into(),
            turns: 5,
        };
        assert_eq!(
            map_agent_event(ev),
            Some(UiEvent::GoalAchieved {
                condition: "done".into(),
                turns: 5
            })
        );
    }

    #[test]
    fn map_goal_cleared() {
        assert_eq!(
            map_agent_event(AgentEvent::GoalCleared),
            Some(UiEvent::GoalCleared)
        );
    }

    #[test]
    fn map_hook_started() {
        let ev = AgentEvent::HookStarted {
            hook_event: "PreTool".into(),
            hook_name: "fmt".into(),
            status_message: Some("running".into()),
        };
        assert_eq!(
            map_agent_event(ev),
            Some(UiEvent::HookStarted {
                hook_event: "PreTool".into(),
                hook_name: "fmt".into(),
                status_message: Some("running".into()),
            })
        );
    }

    #[test]
    fn map_hook_progress() {
        let ev = AgentEvent::HookProgress {
            hook_event: "PreTool".into(),
            hook_name: "fmt".into(),
            last_line: "ok".into(),
        };
        assert_eq!(
            map_agent_event(ev),
            Some(UiEvent::HookProgress {
                hook_event: "PreTool".into(),
                hook_name: "fmt".into(),
                last_line: "ok".into(),
            })
        );
    }

    #[test]
    fn map_hook_finished() {
        let ev = AgentEvent::HookFinished {
            hook_event: "PreTool".into(),
            hook_name: "fmt".into(),
            outcome: "passed".into(),
            duration_ms: 42,
        };
        assert_eq!(
            map_agent_event(ev),
            Some(UiEvent::HookFinished {
                hook_event: "PreTool".into(),
                hook_name: "fmt".into(),
                outcome: "passed".into(),
                duration_ms: 42,
            })
        );
    }

    #[test]
    fn map_hook_system_message() {
        let ev = AgentEvent::HookSystemMessage { text: "hi".into() };
        assert_eq!(
            map_agent_event(ev),
            Some(UiEvent::HookSystemMessage { text: "hi".into() })
        );
    }

    #[tokio::test]
    async fn tui_event_sink_emit_forwards_mapped_event() {
        let (tx, mut rx) = mpsc::unbounded_channel::<UiEvent>();
        let sink = TuiEventSink { tx };
        sink.emit(AgentEvent::TurnFinished {
            reason: "done".into(),
            steps: 1,
        })
        .await;
        let got = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await;
        assert_eq!(got, Ok(Some(UiEvent::TurnFinished)));
    }

    #[tokio::test]
    async fn tui_event_sink_emit_drops_unmapped_event() {
        let (tx, mut rx) = mpsc::unbounded_channel::<UiEvent>();
        let sink = TuiEventSink { tx };
        // An event not handled by map_agent_event falls through to `_ => None`
        // and must not send anything on the channel.
        sink.emit(AgentEvent::LlmRetry {
            step: 0,
            attempt: 1,
            wait_ms: 10,
            reason: "timeout".into(),
        })
        .await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
                .await
                .is_err(),
            "unmapped event must not be forwarded"
        );
    }

    #[tokio::test]
    async fn permission_hook_disabled_auto_allows() {
        let (tx, _rx) = mpsc::unbounded_channel::<PermissionRequest>();
        let hook = TuiPermissionHook {
            tx,
            enabled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        // Wrap in a timeout so the `delete !` mutant (which would route the
        // disabled hook into the blocking user-request path) fails fast with
        // a timeout error instead of hanging the mutant runner for 35s.
        let dec = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            hook.check("Bash", &serde_json::json!({})),
        )
        .await
        .expect("disabled hook must auto-allow without blocking");
        assert!(matches!(dec, recursive::agent::PermissionDecision::Allow));
    }

    #[tokio::test]
    async fn permission_hook_enabled_requests_user_and_allows_on_true() {
        let (tx, mut rx) = mpsc::unbounded_channel::<PermissionRequest>();
        let hook = TuiPermissionHook {
            tx,
            enabled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let handle =
            tokio::spawn(
                async move { hook.check("Bash", &serde_json::json!({"cmd": "ls"})).await },
            );
        let req = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out")
            .expect("request received");
        assert_eq!(req.tool_name, "Bash");
        let _ = req.reply.send(true);
        let dec = handle.await.expect("join");
        assert!(matches!(dec, recursive::agent::PermissionDecision::Allow));
    }

    #[test]
    fn wait_wakeup_returns_none_for_empty_slot() {
        let slot: recursive::tools::WakeupSlot = Arc::new(std::sync::Mutex::new(None));
        assert!(wait_wakeup(&slot).is_none());
    }

    #[test]
    fn wait_wakeup_takes_pending_request() {
        let slot: recursive::tools::WakeupSlot = Arc::new(std::sync::Mutex::new(Some(
            recursive::tools::WakeupRequest {
                delay: std::time::Duration::from_secs(1),
                reason: "timer".into(),
                prompt: "go".into(),
            },
        )));
        let req = wait_wakeup(&slot).expect("Some");
        assert_eq!(req.prompt, "go");
        // Slot is cleared after take.
        assert!(wait_wakeup(&slot).is_none());
    }

    // ── Goal-323: max_turns cap enforcement ────────────────────────────

    #[tokio::test]
    async fn max_turns_cap_auto_stops_loop() {
        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "first".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "second".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));
        let rt = AgentRuntime::builder().llm(llm).build().expect("rt");
        let mut backend = Backend::spawn_with_runtime(rt);
        let _ =
            tokio::time::timeout(std::time::Duration::from_secs(1), backend.event_rx.recv()).await;

        backend
            .action_tx
            .send(UserAction::StartLoop {
                goal: "g".into(),
                max_turns: 1,
            })
            .unwrap();

        let mut seen_turn = false;
        let mut seen_stopped = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(
                std::time::Duration::from_millis(200),
                backend.event_rx.recv(),
            )
            .await
            {
                Ok(Some(UiEvent::TurnFinished)) => seen_turn = true,
                Ok(Some(UiEvent::LoopStopped)) => {
                    seen_stopped = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => {}
            }
        }
        let _ = backend.action_tx.send(UserAction::Shutdown);
        assert!(seen_turn, "expected the capped turn to run");
        assert!(seen_stopped, "expected LoopStopped after max_turns=1");
    }

    #[tokio::test]
    async fn max_turns_zero_runs_unlimited_without_auto_stop() {
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "first".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let rt = AgentRuntime::builder().llm(llm).build().expect("rt");
        let mut backend = Backend::spawn_with_runtime(rt);
        let _ =
            tokio::time::timeout(std::time::Duration::from_secs(1), backend.event_rx.recv()).await;

        backend
            .action_tx
            .send(UserAction::StartLoop {
                goal: "g".into(),
                max_turns: 0,
            })
            .unwrap();

        let mut seen_turn = false;
        let mut seen_stopped = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(
                std::time::Duration::from_millis(200),
                backend.event_rx.recv(),
            )
            .await
            {
                Ok(Some(UiEvent::TurnFinished)) => seen_turn = true,
                Ok(Some(UiEvent::LoopStopped)) => {
                    seen_stopped = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => {}
            }
        }
        let _ = backend.action_tx.send(UserAction::Shutdown);
        assert!(seen_turn, "expected the first turn to run");
        assert!(
            !seen_stopped,
            "unlimited loop (max_turns=0) must not auto-stop"
        );
    }

    // ── Pre-existing: wait_for_cancel semantics ───────────────────────

    #[tokio::test]
    async fn wait_for_cancel_returns_immediately_when_flag_already_set() {
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let notify = Arc::new(tokio::sync::Notify::new());
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            wait_for_cancel(flag, notify),
        )
        .await
        .expect("should return immediately when flag is set");
    }

    #[tokio::test]
    async fn wait_for_cancel_blocks_until_flag_set_and_notified() {
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notify = Arc::new(tokio::sync::Notify::new());
        let flag_clone = flag.clone();
        let notify_clone = notify.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            flag_clone.store(true, std::sync::atomic::Ordering::SeqCst);
            notify_clone.notify_one();
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            wait_for_cancel(flag, notify),
        )
        .await
        .expect("timed out waiting for cancel");
    }

    #[tokio::test]
    async fn wait_for_cancel_must_block_when_flag_false_and_no_notify() {
        // If wait_for_cancel is replaced with `()` it returns immediately and
        // this timeout succeeds — failing the assertion. The real impl blocks.
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notify = Arc::new(tokio::sync::Notify::new());
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            wait_for_cancel(flag, notify),
        )
        .await;
        assert!(
            result.is_err(),
            "wait_for_cancel must block while flag is false and no notify fires"
        );
    }

    // ── Pre-existing: mcp_server_transport classification ─────────────

    #[test]
    fn mcp_server_transport_classifies_http_url() {
        let s = recursive::mcp::McpServer {
            name: "h".into(),
            command: String::new(),
            args: vec![],
            url: Some("http://x".into()),
            env: None,
        };
        assert_eq!(mcp_server_transport(&s), "http");
    }

    #[test]
    fn mcp_server_transport_classifies_stdio_command() {
        // Non-empty command with no URL → stdio. This kills the
        // `delete !` mutant, which would fall through to "unknown".
        let s = recursive::mcp::McpServer {
            name: "s".into(),
            command: "node".into(),
            args: vec![],
            url: None,
            env: None,
        };
        assert_eq!(mcp_server_transport(&s), "stdio");
    }

    #[test]
    fn mcp_server_transport_classifies_unknown_when_empty() {
        let s = recursive::mcp::McpServer {
            name: "u".into(),
            command: String::new(),
            args: vec![],
            url: None,
            env: None,
        };
        assert_eq!(mcp_server_transport(&s), "unknown");
    }

    // ── Goal-323: max_turns counts arbiter-driven Run turns (582 path) ──

    #[tokio::test]
    async fn max_turns_cap_counts_arbiter_run_turns() {
        // Drive a second turn via LoopTrigger (the arbiter Run path that
        // increments turns_run at the `ls.turns_run += 1` line inside the
        // ArbiterDecision::Run arm). The trigger is sent right after
        // StartLoop so the arbiter's try_recv drains it on the second
        // iteration (after the goal turn drains from the type-ahead queue)
        // — sending it mid-turn would let run_turn_select_loop discard it.
        // With max_turns=2 the loop must stop after the triggered turn.
        // The `+=`→`-=`/`*=` mutants never reach the cap, so LoopStopped
        // never fires and this test times out without seeing it.
        let llm = Arc::new(MockProvider::new(vec![
            Completion {
                content: "first".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
            Completion {
                content: "second".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ]));
        let rt = AgentRuntime::builder().llm(llm).build().expect("rt");
        let mut backend = Backend::spawn_with_runtime(rt);
        let _ =
            tokio::time::timeout(std::time::Duration::from_secs(1), backend.event_rx.recv()).await;

        backend
            .action_tx
            .send(UserAction::StartLoop {
                goal: "g".into(),
                max_turns: 2,
            })
            .unwrap();

        // Each turn emits TurnStarted once, but TurnFinished twice: once
        // from the runtime event sink (during the turn) and once from the
        // backend after run_turn_select_loop returns. The second
        // TurnFinished is the safe "worker is looping back" signal — a
        // trigger sent earlier would be discarded by run_turn_select_loop.
        let mut turns = 0;
        let mut finished = 0;
        let mut sent_trigger = false;
        let mut seen_stopped = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(6);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(
                std::time::Duration::from_millis(200),
                backend.event_rx.recv(),
            )
            .await
            {
                Ok(Some(UiEvent::TurnStarted)) => turns += 1,
                Ok(Some(UiEvent::TurnFinished)) => {
                    finished += 1;
                    // After turn 1's second TurnFinished, the worker is back
                    // in the arbiter — safe to inject the Run trigger.
                    if finished == 2 && !sent_trigger {
                        let _ = backend.action_tx.send(UserAction::LoopTrigger {
                            source: "test".into(),
                            prompt: "p".into(),
                        });
                        sent_trigger = true;
                    }
                }
                Ok(Some(UiEvent::LoopStopped)) => {
                    seen_stopped = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => {}
            }
        }
        let _ = backend.action_tx.send(UserAction::Shutdown);
        assert_eq!(turns, 2, "expected exactly two turns before the cap");
        assert!(seen_stopped, "expected LoopStopped after max_turns=2");
    }

    // ── /model picker: SwitchModel hot-swaps the provider ───────────────────

    /// `UserAction::SwitchModel` must build a fresh provider for the chosen
    /// `(preset, model)` and emit `UiEvent::ModelSwitched` so the UI can mirror
    /// the new active model. The provider is built from env config; we seed a
    /// config api_key so the fallback path in `build_provider_for_model`
    /// succeeds without a real network call (provider construction is offline).
    #[tokio::test]
    #[cfg_attr(target_os = "windows", ignore)]
    async fn switch_model_emits_model_switched() {
        let empty_home = tempfile::tempdir().expect("tempdir");
        let _pin = recursive::test_util::PinnedRecursiveHome::new(empty_home.path());

        // Save/restore the env vars we touch. PinnedRecursiveHome already
        // holds env_lock(), serialising this against other env-mutating tests.
        let prev_recursive = std::env::var("RECURSIVE_API_KEY").ok();
        let prev_openai = std::env::var("OPENAI_API_KEY").ok();
        let prev_deepseek = std::env::var("DEEPSEEK_API_KEY").ok();
        std::env::remove_var("RECURSIVE_API_KEY");
        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("DEEPSEEK_API_KEY");

        // Config api_key drives the fallback in build_provider_for_model.
        let cfg_dir = empty_home.path().join(".recursive");
        std::fs::create_dir_all(&cfg_dir).expect("mkdir");
        std::fs::write(
            cfg_dir.join("config.toml"),
            "[provider]\napi_key = \"sk-shared\"\nmodel = \"x\"\ntype = \"openai\"\n",
        )
        .expect("write config");

        // Start from a Ready mock runtime; SwitchModel swaps its provider.
        let llm = Arc::new(MockProvider::new(vec![Completion {
            content: "ok".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let rt = AgentRuntime::builder().llm(llm).build().expect("rt builds");
        let mut backend = Backend::spawn_with_runtime(rt);

        // Drain RuntimeReady.
        let _ =
            tokio::time::timeout(std::time::Duration::from_secs(1), backend.event_rx.recv()).await;

        backend
            .action_tx
            .send(UserAction::SwitchModel {
                preset_id: "deepseek".into(),
                model: "deepseek-chat".into(),
            })
            .unwrap();

        let mut got_switched = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(
                std::time::Duration::from_millis(200),
                backend.event_rx.recv(),
            )
            .await
            {
                Ok(Some(UiEvent::ModelSwitched { preset_id, model })) => {
                    assert_eq!(preset_id, "deepseek");
                    assert_eq!(model, "deepseek-chat");
                    got_switched = true;
                    break;
                }
                Ok(Some(UiEvent::Error { message })) => {
                    panic!("switch model failed: {message}");
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        let _ = backend.action_tx.send(UserAction::Shutdown);

        // Restore env vars.
        match prev_recursive {
            Some(v) => std::env::set_var("RECURSIVE_API_KEY", v),
            None => std::env::remove_var("RECURSIVE_API_KEY"),
        }
        match prev_openai {
            Some(v) => std::env::set_var("OPENAI_API_KEY", v),
            None => std::env::remove_var("OPENAI_API_KEY"),
        }
        match prev_deepseek {
            Some(v) => std::env::set_var("DEEPSEEK_API_KEY", v),
            None => std::env::remove_var("DEEPSEEK_API_KEY"),
        }
        assert!(got_switched, "expected UiEvent::ModelSwitched");
    }

    /// `SwitchModel` while offline (no usable runtime) must surface a clear
    /// error rather than panicking or silently dropping the request.
    #[tokio::test]
    async fn switch_model_offline_emits_error() {
        let empty_home = tempfile::tempdir().expect("tempdir");
        let _pin = recursive::test_util::PinnedRecursiveHome::new(empty_home.path());
        let prev_recursive = std::env::var("RECURSIVE_API_KEY").ok();
        let prev_openai = std::env::var("OPENAI_API_KEY").ok();
        std::env::remove_var("RECURSIVE_API_KEY");
        std::env::remove_var("OPENAI_API_KEY");

        let mut backend = Backend::spawn();
        // Drain RuntimeOffline.
        let _ =
            tokio::time::timeout(std::time::Duration::from_secs(1), backend.event_rx.recv()).await;

        backend
            .action_tx
            .send(UserAction::SwitchModel {
                preset_id: "deepseek".into(),
                model: "deepseek-chat".into(),
            })
            .unwrap();

        let mut got_error = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(
                std::time::Duration::from_millis(200),
                backend.event_rx.recv(),
            )
            .await
            {
                Ok(Some(UiEvent::Error { message })) => {
                    assert!(message.contains("offline"), "got: {message}");
                    got_error = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        let _ = backend.action_tx.send(UserAction::Shutdown);
        match prev_recursive {
            Some(v) => std::env::set_var("RECURSIVE_API_KEY", v),
            None => std::env::remove_var("RECURSIVE_API_KEY"),
        }
        match prev_openai {
            Some(v) => std::env::set_var("OPENAI_API_KEY", v),
            None => std::env::remove_var("OPENAI_API_KEY"),
        }
        assert!(got_error, "expected an offline error for SwitchModel");
    }
}
