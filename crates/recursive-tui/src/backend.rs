//! In-process agent backend for the TUI.
//!
//! [`Backend`] owns one tokio task that holds an
//! [`recursive::AgentRuntime`]. The UI thread sends [`UserAction`]s
//! (typed messages, plan confirmations, shutdown) into the worker via
//! `action_tx` and the worker pushes [`UiEvent`]s back via `event_rx`.
//!
//! When no LLM provider is configured (no `RECURSIVE_API_KEY`/
//! `OPENAI_API_KEY` and `RECURSIVE_TUI_MOCK` is unset), the worker
//! still spins up — every `SendMessage` is answered with a
//! `UiEvent::Error` describing the missing config. This keeps the
//! TUI itself bootable for layout/keybinding work even without
//! credentials.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use recursive::tools::{
    ApplyPatch, ListDir, LocalTransport, ReadFile, RunShell, SearchFiles, ToolTransport, WriteFile,
};
use recursive::{
    AgentEvent, AgentRuntime, AgentRuntimeBuilder, EventSink, LlmProvider, ToolRegistry,
};
use serde_json::json;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::events::{UiEvent, UserAction};

/// A handle to the agent worker task.
///
/// `Backend` is owned by `main` and lives for the duration of the
/// terminal session. Drop the handle to abandon the worker (it
/// exits gracefully when its action channel closes).
pub struct Backend {
    pub action_tx: mpsc::UnboundedSender<UserAction>,
    pub event_rx: mpsc::UnboundedReceiver<UiEvent>,
    _worker: JoinHandle<()>,
}

impl Backend {
    /// Spawn a fresh backend worker against the current process env.
    pub fn spawn() -> Self {
        Self::spawn_with_state(build_runtime())
    }

    /// Spawn a backend with a pre-built runtime. Primarily intended
    /// for integration tests that wire a [`recursive::llm::MockProvider`].
    pub fn spawn_with_runtime(rt: AgentRuntime) -> Self {
        Self::spawn_with_state(RuntimeBuild::Ready(rt))
    }

    fn spawn_with_state(state: RuntimeBuild) -> Self {
        let (action_tx, action_rx) = mpsc::unbounded_channel::<UserAction>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<UiEvent>();

        let worker = tokio::spawn(worker_loop(state, action_rx, event_tx));

        Self {
            action_tx,
            event_rx,
            _worker: worker,
        }
    }
}

/// EventSink implementation that funnels [`AgentEvent`]s into a
/// [`UiEvent`] channel.
///
/// Goal-144 broadens the mapping from goal-143's four variants to
/// seven: streaming `PartialToken`, finalised `AssistantText`,
/// id-paired `ToolCall`/`ToolResult` (with success inferred from the
/// `ERROR: ` prefix the kernel uses for failures — see
/// `src/http.rs:1388`), token `Usage`, request `Latency`, transcript
/// `Compacted` notifications and `TurnFinished` for spinner reset.
struct TuiEventSink {
    tx: mpsc::UnboundedSender<UiEvent>,
}

#[async_trait]
impl EventSink for TuiEventSink {
    async fn emit(&self, event: AgentEvent) {
        let mapped = map_agent_event(event);
        if let Some(ev) = mapped {
            let _ = self.tx.send(ev);
        }
    }
}

/// Pure mapping helper exposed for tests.
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
        // PlanProposed / PlanConfirmed / PlanRejected are intentionally
        // dropped here — Goal 147 will consume them.
        _ => None,
    }
}

/// Outcome of attempting to construct a runtime from the environment.
enum RuntimeBuild {
    Ready(AgentRuntime),
    /// No usable LLM provider; the worker enters offline mode and
    /// answers every `SendMessage` with an `Error` event carrying
    /// `reason`.
    Offline {
        reason: String,
    },
}

/// Build the runtime that backs this worker.
///
/// Order of resolution:
///   1. `RECURSIVE_TUI_MOCK=1` → an empty mock provider (mostly
///      useful for tests; the integration test wires a richer mock
///      via the `recursive-agent/test-utils` feature).
///   2. `RECURSIVE_API_KEY` / `OPENAI_API_KEY` set → real provider.
///   3. Otherwise → offline mode.
fn build_runtime() -> RuntimeBuild {
    let workspace: PathBuf = std::env::var("RECURSIVE_WORKSPACE")
        .map(PathBuf::from)
        .ok()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));

    let api_key = std::env::var("RECURSIVE_API_KEY")
        .or_else(|_| std::env::var("OPENAI_API_KEY"))
        .ok();

    if api_key.is_none() {
        return RuntimeBuild::Offline {
            reason: "no LLM provider configured (set OPENAI_API_KEY or RECURSIVE_API_KEY)"
                .to_string(),
        };
    }

    let api_key = api_key.unwrap();
    let api_base = std::env::var("RECURSIVE_API_BASE")
        .or_else(|_| std::env::var("OPENAI_API_BASE"))
        .unwrap_or_else(|_| "https://api.openai.com/v1".into());
    let model = std::env::var("RECURSIVE_MODEL")
        .or_else(|_| std::env::var("OPENAI_MODEL"))
        .unwrap_or_else(|_| "gpt-4o-mini".into());

    let provider: Arc<dyn LlmProvider> = Arc::new(recursive::llm::OpenAiProvider::new(
        &api_base, api_key, &model,
    ));

    let tools = build_default_tools(&workspace);

    match AgentRuntimeBuilder::new()
        .llm(provider)
        .tools(tools)
        .build()
    {
        Ok(rt) => RuntimeBuild::Ready(rt),
        Err(e) => RuntimeBuild::Offline {
            reason: format!("failed to build agent runtime: {e}"),
        },
    }
}

/// Minimal tool set the TUI exposes by default. Mirrors the goal
/// scope (read_file/write_file/apply_patch/list_dir/run_shell/
/// search_files) — richer tools (memory, MCP, skills, …) are out of
/// scope for step 1.
fn build_default_tools(root: &std::path::Path) -> ToolRegistry {
    let transport: Arc<dyn ToolTransport> = Arc::new(LocalTransport);
    ToolRegistry::new(transport)
        .register(Arc::new(ReadFile::new(root)))
        .register(Arc::new(WriteFile::new(root)))
        .register(Arc::new(ApplyPatch::new(root)))
        .register(Arc::new(ListDir::new(root)))
        .register(Arc::new(
            RunShell::new(root).with_timeout(Duration::from_secs(300)),
        ))
        .register(Arc::new(SearchFiles::new(root)))
}

/// Build a standalone `ToolRegistry` containing only `run_shell`,
/// rooted at the workspace. Used by [`worker_loop`] to service the
/// Goal-145 `UserAction::RunShell` requests **without** going through
/// the agent runtime — bash mode bypasses the LLM entirely and must
/// keep working even in offline mode.
fn build_bash_registry(root: &std::path::Path) -> ToolRegistry {
    let transport: Arc<dyn ToolTransport> = Arc::new(LocalTransport);
    ToolRegistry::new(transport).register(Arc::new(
        RunShell::new(root).with_timeout(Duration::from_secs(300)),
    ))
}

/// Resolve the workspace root the same way [`build_runtime`] does,
/// for the bash-mode registry.
fn resolve_workspace_root() -> PathBuf {
    std::env::var("RECURSIVE_WORKSPACE")
        .map(PathBuf::from)
        .ok()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Long-lived worker loop. Consumes [`UserAction`]s from the UI and
/// drives the [`AgentRuntime`].
async fn worker_loop(
    mut state: RuntimeBuild,
    mut action_rx: mpsc::UnboundedReceiver<UserAction>,
    event_tx: mpsc::UnboundedSender<UiEvent>,
) {
    // Install our event sink once when the runtime is ready. The sink
    // is reused across turns; runtime.run() drains it per turn
    // internally via a forwarder task.
    if let RuntimeBuild::Ready(ref mut rt) = state {
        rt.set_event_sink(Arc::new(TuiEventSink {
            tx: event_tx.clone(),
        }));
    }

    // Goal-145: a dedicated registry for `!`-prefixed bash commands.
    // Built once and reused; works regardless of runtime readiness.
    let bash_registry = build_bash_registry(&resolve_workspace_root());
    let bash_seq = AtomicU64::new(0);

    while let Some(action) = action_rx.recv().await {
        match action {
            UserAction::Shutdown => break,
            UserAction::SendMessage(text) => match &mut state {
                RuntimeBuild::Ready(rt) => {
                    if let Err(e) = rt.run(text).await {
                        let _ = event_tx.send(UiEvent::Error {
                            message: e.to_string(),
                        });
                    }
                    // Always poke the UI to clear its spinner, even if
                    // the runtime didn't emit `TurnFinished` on its own
                    // (e.g. offline transitions, error short-circuit).
                    let _ = event_tx.send(UiEvent::TurnFinished);
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
                    if let Err(e) = rt.run("").await {
                        let _ = event_tx.send(UiEvent::Error {
                            message: e.to_string(),
                        });
                    }
                    let _ = event_tx.send(UiEvent::TurnFinished);
                }
            }
            UserAction::RejectPlan(reason) => {
                if let RuntimeBuild::Ready(rt) = &mut state {
                    rt.reject_plan(&reason);
                }
            }
        }
    }
}

/// Dispatch one `!`-prefixed bash command directly via the standalone
/// bash registry and stream the result back as `ToolCall` +
/// `ToolResult` events. The command does **not** enter the runtime
/// transcript and does **not** count as an LLM turn.
async fn run_bash_command(
    registry: &ToolRegistry,
    seq: &AtomicU64,
    cmd: String,
    event_tx: &mpsc::UnboundedSender<UiEvent>,
) {
    let n = seq.fetch_add(1, Ordering::Relaxed);
    let id = format!("ui-bash-{n}");
    let arguments = json!({ "command": cmd });
    let arguments_str = arguments.to_string();

    // Surface the call up-front so the spinner verb / preview render
    // immediately, even on slow commands.
    let _ = event_tx.send(UiEvent::ToolCall {
        id: id.clone(),
        name: "run_shell".into(),
        arguments: arguments_str,
    });

    let (output, success) = match registry.invoke("run_shell", arguments).await {
        Ok(out) => (out, true),
        Err(e) => (format!("ERROR: {e}"), false),
    };

    let _ = event_tx.send(UiEvent::ToolResult {
        id,
        name: "run_shell".into(),
        output,
        success,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without `RECURSIVE_API_KEY` / `OPENAI_API_KEY`, sending a
    /// message produces a graceful `UiEvent::Error` rather than a
    /// panic.
    ///
    /// This test mutates process-global env vars; we keep all env
    /// touches inside one test to avoid races with other tests in
    /// this crate (cf. lesson 17 in `.dev/AGENTS.md`).
    #[tokio::test]
    async fn offline_mode_returns_error_without_panic() {
        // Snapshot and clear any keys the user happens to have set.
        let prev_recursive = std::env::var("RECURSIVE_API_KEY").ok();
        let prev_openai = std::env::var("OPENAI_API_KEY").ok();
        std::env::remove_var("RECURSIVE_API_KEY");
        std::env::remove_var("OPENAI_API_KEY");

        let mut backend = Backend::spawn();
        backend
            .action_tx
            .send(UserAction::SendMessage("hi".into()))
            .unwrap();

        // Drain events until we see the offline Error (a
        // TurnFinished may precede or follow it).
        let mut got_error = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(500), backend.event_rx.recv()).await {
                Ok(Some(UiEvent::Error { message })) => {
                    assert!(message.contains("no LLM provider configured"));
                    got_error = true;
                    break;
                }
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(_) => continue,
            }
        }

        let _ = backend.action_tx.send(UserAction::Shutdown);

        // Restore environment.
        if let Some(v) = prev_recursive {
            std::env::set_var("RECURSIVE_API_KEY", v);
        }
        if let Some(v) = prev_openai {
            std::env::set_var("OPENAI_API_KEY", v);
        }

        assert!(got_error, "expected an offline-mode UiEvent::Error");
    }

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
    fn map_plan_proposed_is_dropped() {
        let ev = AgentEvent::PlanProposed {
            plan_text: "p".into(),
            tool_calls: vec![],
        };
        assert!(map_agent_event(ev).is_none());
    }

    /// Goal-145: a `UserAction::RunShell` must dispatch the
    /// `run_shell` tool directly and emit a paired
    /// `ToolCall` + `ToolResult` regardless of LLM availability.
    /// We exercise this without spawning a real `Backend` (which
    /// would require workspace-rooting and env juggling) by
    /// driving `run_bash_command` directly.
    #[tokio::test]
    async fn run_shell_action_dispatches_tool_and_emits_events() {
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = build_bash_registry(tmp.path());
        let (tx, mut rx) = mpsc::unbounded_channel::<UiEvent>();
        let seq = AtomicU64::new(0);
        run_bash_command(&registry, &seq, "echo bash-mode-works".into(), &tx).await;

        // First event: ToolCall.
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

        // Second event: ToolResult.
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

    /// Bash mode survives offline mode: there is no LLM call, only a
    /// direct `ToolRegistry::invoke`.
    #[tokio::test]
    async fn run_shell_action_works_when_runtime_offline() {
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = build_bash_registry(tmp.path());
        let (tx, mut rx) = mpsc::unbounded_channel::<UiEvent>();
        let seq = AtomicU64::new(42);
        run_bash_command(&registry, &seq, "echo offline".into(), &tx).await;

        // The first event should carry id ui-bash-42 (we seeded the
        // counter to verify the sequence is honoured).
        let call = rx.recv().await.expect("ToolCall event");
        if let UiEvent::ToolCall { id, .. } = call {
            assert_eq!(id, "ui-bash-42");
        } else {
            panic!("expected ToolCall, got {call:?}");
        }
        let _ = rx.recv().await; // consume the ToolResult
    }
}
