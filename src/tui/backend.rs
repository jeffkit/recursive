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
use crate::{AgentEvent, AgentRuntime, EventSink};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::tui::bash::{build_bash_registry, resolve_workspace_root, run_bash_command};
use crate::tui::events::{UiEvent, UserAction};
use crate::tui::runtime_builder::{build_runtime, RuntimeBuild};

/// A handle to the agent worker task.
pub struct Backend {
    pub action_tx: mpsc::UnboundedSender<UserAction>,
    pub event_rx: mpsc::UnboundedReceiver<UiEvent>,
    /// Shared cancel flag: the UI flips this to `true` to interrupt an
    /// in-flight turn; the worker's `tokio::select!` wakes and aborts.
    pub cancel_flag: Arc<AtomicBool>,
    _worker: JoinHandle<()>,
}

impl Backend {
    pub fn spawn() -> Self {
        Self::spawn_with_state(build_runtime())
    }

    pub fn spawn_with_runtime(rt: AgentRuntime) -> Self {
        Self::spawn_with_state(RuntimeBuild::Ready(rt))
    }

    fn spawn_with_state(state: RuntimeBuild) -> Self {
        let (action_tx, action_rx) = mpsc::unbounded_channel::<UserAction>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<UiEvent>();
        let cancel_flag = Arc::new(AtomicBool::new(false));

        let worker = tokio::spawn(worker_loop(state, action_rx, event_tx, cancel_flag.clone()));

        Self {
            action_tx,
            event_rx,
            cancel_flag,
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
            id, name, output, ..
        } => {
            let success = !output.starts_with("ERROR: ");
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
            ..
        } => Some(UiEvent::Usage {
            input_tokens: input_tokens as u64,
            output_tokens: output_tokens as u64,
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
        _ => None,
    }
}

async fn worker_loop(
    mut state: RuntimeBuild,
    mut action_rx: mpsc::UnboundedReceiver<UserAction>,
    event_tx: mpsc::UnboundedSender<UiEvent>,
    cancel_flag: Arc<AtomicBool>,
) {
    if let RuntimeBuild::Ready(ref mut rt) = state {
        rt.set_event_sink(Arc::new(TuiEventSink {
            tx: event_tx.clone(),
        }));
    }

    let bash_registry = build_bash_registry(&resolve_workspace_root());
    let bash_seq = AtomicU64::new(0);

    while let Some(action) = action_rx.recv().await {
        match action {
            UserAction::Shutdown => break,
            UserAction::SendMessage(text) => match &mut state {
                RuntimeBuild::Ready(rt) => {
                    cancel_flag.store(false, Ordering::SeqCst);
                    let cancel_for_select = cancel_flag.clone();
                    let result: Result<(), crate::Error> = tokio::select! {
                        r = rt.run(text) => r.map(|_| ()),
                        _ = wait_for_cancel(cancel_for_select) => {
                            let _ = event_tx.send(UiEvent::Error {
                                message: "interrupted".into(),
                            });
                            Ok(())
                        }
                    };
                    if let Err(e) = result {
                        let _ = event_tx.send(UiEvent::Error {
                            message: e.to_string(),
                        });
                    }
                    let _ = event_tx.send(UiEvent::TurnFinished);
                    cancel_flag.store(false, Ordering::SeqCst);
                }
                RuntimeBuild::Offline { reason } => {
                    let _ = event_tx.send(UiEvent::Error {
                        message: reason.clone(),
                    });
                    let _ = event_tx.send(UiEvent::TurnFinished);
                }
            },
            UserAction::RunShell(cmd) => {
                run_bash_command(&bash_registry, &bash_seq, cmd, &event_tx).await;
            }
            UserAction::ConfirmPlan => {
                if let RuntimeBuild::Ready(rt) = &mut state {
                    rt.confirm_plan();
                    cancel_flag.store(false, Ordering::SeqCst);
                    let cancel_for_select = cancel_flag.clone();
                    let result: Result<(), crate::Error> = tokio::select! {
                        r = rt.run("") => r.map(|_| ()),
                        _ = wait_for_cancel(cancel_for_select) => {
                            let _ = event_tx.send(UiEvent::Error {
                                message: "interrupted".into(),
                            });
                            Ok(())
                        }
                    };
                    if let Err(e) = result {
                        let _ = event_tx.send(UiEvent::Error {
                            message: e.to_string(),
                        });
                    }
                    let _ = event_tx.send(UiEvent::TurnFinished);
                    cancel_flag.store(false, Ordering::SeqCst);
                }
            }
            UserAction::RejectPlan(reason) => {
                if let RuntimeBuild::Ready(rt) = &mut state {
                    rt.reject_plan(&reason);
                }
            }
            UserAction::Compact => match &mut state {
                RuntimeBuild::Ready(rt) => {
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
            UserAction::SetPlanningMode(on) => {
                if let RuntimeBuild::Ready(rt) = &mut state {
                    let mode = if on {
                        crate::PlanningMode::PlanFirst
                    } else {
                        crate::PlanningMode::Immediate
                    };
                    rt.set_planning_mode(mode);
                }
            }
            UserAction::Interrupt => {
                cancel_flag.store(true, Ordering::SeqCst);
            }
        }
    }
}

pub async fn wait_for_cancel(flag: Arc<AtomicBool>) {
    loop {
        if flag.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::bash::build_bash_registry;

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
            name: "read_file".into(),
            output: "ERROR: missing".into(),
            step: 0,
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
                assert_eq!(name, "run_shell");
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
                assert_eq!(name, "run_shell");
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
        let started = std::time::Instant::now();
        let timed =
            tokio::time::timeout(std::time::Duration::from_millis(500), wait_for_cancel(flag.clone())).await;
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
