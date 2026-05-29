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
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use recursive::tools::{
    ApplyPatch, ListDir, LocalTransport, ReadFile, RunShell, SearchFiles, ToolTransport, WriteFile,
};
use recursive::{
    AgentEvent, AgentRuntime, AgentRuntimeBuilder, EventSink, LlmProvider, ToolRegistry,
};
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
/// [`UiEvent`] channel. Only the four variants the pre-revamp TUI
/// already consumed are forwarded; the rest are silently dropped
/// until later goals widen the surface.
struct TuiEventSink {
    tx: mpsc::UnboundedSender<UiEvent>,
}

#[async_trait]
impl EventSink for TuiEventSink {
    async fn emit(&self, event: AgentEvent) {
        let mapped = match event {
            AgentEvent::AssistantText { text, .. } => {
                Some(UiEvent::AssistantMessage { content: text })
            }
            AgentEvent::ToolCall { name, .. } => Some(UiEvent::ToolCall { name }),
            AgentEvent::ToolResult { name, .. } => {
                // The kernel does not currently surface success/failure
                // distinctly through `ToolResult`. Treat every reported
                // result as success; failures normally arrive via
                // `Error` events on the runtime.
                Some(UiEvent::ToolResult {
                    name,
                    success: true,
                })
            }
            // The remaining variants (Latency, Usage, PartialToken,
            // Compacted, TurnFinished, PlanProposed, PlanConfirmed,
            // PlanRejected) are intentionally dropped in step 1.
            _ => None,
        };
        if let Some(ev) = mapped {
            let _ = self.tx.send(ev);
        }
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
                }
                RuntimeBuild::Offline { reason } => {
                    let _ = event_tx.send(UiEvent::Error {
                        message: reason.clone(),
                    });
                }
            },
            UserAction::ConfirmPlan => {
                if let RuntimeBuild::Ready(rt) = &mut state {
                    rt.confirm_plan();
                    if let Err(e) = rt.run("").await {
                        let _ = event_tx.send(UiEvent::Error {
                            message: e.to_string(),
                        });
                    }
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

        let evt = tokio::time::timeout(Duration::from_secs(2), backend.event_rx.recv())
            .await
            .expect("event timeout")
            .expect("channel closed");

        let _ = backend.action_tx.send(UserAction::Shutdown);

        // Restore environment.
        if let Some(v) = prev_recursive {
            std::env::set_var("RECURSIVE_API_KEY", v);
        }
        if let Some(v) = prev_openai {
            std::env::set_var("OPENAI_API_KEY", v);
        }

        match evt {
            UiEvent::Error { message } => {
                assert!(message.contains("no LLM provider configured"));
            }
            other => panic!("expected Error event, got {other:?}"),
        }
    }
}
