//! In-process agent backend for the TUI.
//!
//! [`Backend`] owns one tokio task that holds an
//! [`recursive::AgentRuntime`]. The UI thread sends [`UserAction`]s
//! (typed messages, plan confirmations, shutdown) into the worker via
//! `action_tx` and the worker pushes [`UiEvent`]s back via `event_rx`.
//!
//! Provider configuration follows the same priority chain the CLI
//! uses (`recursive::config::Config::from_env()`):
//!
//! 1. env vars (`RECURSIVE_API_KEY` / `OPENAI_API_KEY`,
//!    `RECURSIVE_API_BASE`, `RECURSIVE_MODEL`, ...)
//! 2. `~/.recursive/config.toml` (written by `recursive config set ...`)
//! 3. hardcoded defaults
//!
//! When no API key can be resolved through any of these, the worker
//! still spins up — every `SendMessage` is answered with a
//! `UiEvent::Error` describing the missing config. This keeps the
//! TUI itself bootable for layout/keybinding work even without
//! credentials.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    /// Goal-147: shared cancel flag the worker watches via
    /// [`wait_for_cancel`]. The UI flips this to `true` when the
    /// user presses Ctrl+C / Esc during a running turn; the
    /// `tokio::select!` around `runtime.run` returns immediately and
    /// the worker resets the flag before accepting the next action.
    ///
    /// Exposed publicly so integration tests in this crate can
    /// inspect / mutate it without going through the action channel.
    pub cancel_flag: Arc<AtomicBool>,
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
        // Goal-147: forward the structured plan-mode protocol to the
        // UI. The UI opens / closes a `Modal::PlanReview` in response.
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

/// Outcome of attempting to construct a runtime from the environment.
#[allow(clippy::large_enum_variant)]
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
/// Delegates to [`recursive::config::Config::from_env`] so the TUI
/// honours the same priority chain as the CLI:
///
/// 1. env vars (`RECURSIVE_API_KEY` / `OPENAI_API_KEY`,
///    `RECURSIVE_API_BASE`, `RECURSIVE_MODEL`, ...)
/// 2. `~/.recursive/config.toml`
/// 3. hardcoded defaults
///
/// If `Config::from_env` fails (malformed `config.toml`) or yields
/// no API key, the worker enters offline mode with a helpful reason.
fn build_runtime() -> RuntimeBuild {
    let config = match recursive::config::Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            return RuntimeBuild::Offline {
                reason: format!("failed to load configuration: {e}"),
            };
        }
    };

    let api_key = match config.api_key.as_deref().filter(|k| !k.is_empty()) {
        Some(k) => k.to_string(),
        None => {
            return RuntimeBuild::Offline {
                reason: "no LLM provider configured. Set RECURSIVE_API_KEY / \
                         OPENAI_API_KEY, or run `recursive config set \
                         provider.api_key <KEY>` to populate \
                         ~/.recursive/config.toml."
                    .to_string(),
            };
        }
    };

    let provider: Arc<dyn LlmProvider> = Arc::new(
        recursive::llm::OpenAiProvider::new(&config.api_base, api_key, &config.model)
            .with_temperature(config.temperature),
    );

    let tools = build_default_tools(&config.workspace);

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
    cancel_flag: Arc<AtomicBool>,
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
                    // Goal-147: pre-flight reset of cancel_flag so a
                    // stale Interrupt from before the turn doesn't
                    // immediately abort us.
                    cancel_flag.store(false, Ordering::SeqCst);

                    let cancel_for_select = cancel_flag.clone();
                    let result: Result<(), recursive::Error> = tokio::select! {
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
                    // Always poke the UI to clear its spinner, even if
                    // the runtime didn't emit `TurnFinished` on its own
                    // (e.g. offline transitions, error short-circuit,
                    // interrupt).
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
                    let result: Result<(), recursive::Error> = tokio::select! {
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
                        recursive::PlanningMode::PlanFirst
                    } else {
                        recursive::PlanningMode::Immediate
                    };
                    rt.set_planning_mode(mode);
                }
                // Offline mode is allowed: the App-side System block
                // already echoed the new state to the user.
            }
            UserAction::Interrupt => {
                // Goal-147: flip the flag and let the in-flight
                // tokio::select! arm in `SendMessage` / `ConfirmPlan`
                // wake up via wait_for_cancel.
                cancel_flag.store(true, Ordering::SeqCst);
            }
        }
    }
}

/// Goal-147: poll the cancel flag every 100ms until it flips to
/// `true`. This is the simplest possible implementation; the
/// alternative (a `tokio::sync::Notify` per turn) wires more cleanly
/// but adds a moving part — atomic+poll is fine for a 1-Hz user
/// gesture.
pub async fn wait_for_cancel(flag: Arc<AtomicBool>) {
    loop {
        if flag.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
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

    /// Without `RECURSIVE_API_KEY` / `OPENAI_API_KEY` and without a
    /// `~/.recursive/config.toml`, sending a message produces a
    /// graceful `UiEvent::Error` rather than a panic.
    ///
    /// Also covers the Goal-149 case where a populated config file
    /// _does_ provide an API key — the worker must build the runtime
    /// successfully and not enter offline mode.
    ///
    /// Both checks live in one test on purpose: env mutations
    /// (`HOME` / `RECURSIVE_API_KEY` / `OPENAI_API_KEY`) are
    /// process-global. We hold the `PinnedHome` lock for the whole
    /// body so other tests can't observe a torn-down state
    /// (cf. lesson 17 in `.dev/AGENTS.md`).
    #[tokio::test]
    async fn offline_mode_and_config_file_resolution() {
        // ── Part A: empty HOME, no env vars → offline ──────────────
        let empty_home = tempfile::tempdir().expect("tempdir");
        let _pin = recursive::test_util::PinnedHome::new(empty_home.path());

        let prev_recursive = std::env::var("RECURSIVE_API_KEY").ok();
        let prev_openai = std::env::var("OPENAI_API_KEY").ok();
        std::env::remove_var("RECURSIVE_API_KEY");
        std::env::remove_var("OPENAI_API_KEY");

        let mut backend = Backend::spawn();
        backend
            .action_tx
            .send(UserAction::SendMessage("hi".into()))
            .unwrap();

        let mut got_error = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(500), backend.event_rx.recv()).await {
                Ok(Some(UiEvent::Error { message })) => {
                    assert!(
                        message.contains("no LLM provider configured"),
                        "expected offline reason, got {message:?}"
                    );
                    assert!(
                        message.contains("recursive config set"),
                        "offline reason should mention CLI config helper, got {message:?}"
                    );
                    got_error = true;
                    break;
                }
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(_) => continue,
            }
        }
        let _ = backend.action_tx.send(UserAction::Shutdown);
        assert!(got_error, "expected an offline-mode UiEvent::Error");
        drop(backend);

        // ── Part B: write a config.toml under HOME, expect Ready ───
        let cfg_dir = empty_home.path().join(".recursive");
        std::fs::create_dir_all(&cfg_dir).expect("mkdir");
        std::fs::write(
            cfg_dir.join("config.toml"),
            r#"[provider]
api_key = "sk-test-from-config"
api_base = "https://api.example.invalid"
model = "test-model-from-config"
type = "openai"
"#,
        )
        .expect("write config");

        // Env still cleared from Part A — config.toml is the only
        // source of credentials.
        let build = build_runtime();
        match build {
            RuntimeBuild::Ready(_) => {} // pass
            RuntimeBuild::Offline { reason } => {
                panic!("expected Ready when config.toml has api_key, got Offline: {reason}");
            }
        }

        // ── Restore env ────────────────────────────────────────────
        if let Some(v) = prev_recursive {
            std::env::set_var("RECURSIVE_API_KEY", v);
        }
        if let Some(v) = prev_openai {
            std::env::set_var("OPENAI_API_KEY", v);
        }
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
    fn map_plan_proposed_is_forwarded() {
        // Goal-147: PlanProposed used to be dropped; it now flows
        // through to the UI as a `UiEvent::PlanProposed`.
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

    /// Goal-147: an `Interrupt` action sets the cancel flag so the
    /// worker's `tokio::select!` arm can wake up and abort the turn.
    /// We assert the public flag flips without asserting on the
    /// turn outcome (the worker may be in offline mode here).
    #[tokio::test]
    async fn interrupt_action_sets_cancel_flag() {
        let prev_recursive = std::env::var("RECURSIVE_API_KEY").ok();
        let prev_openai = std::env::var("OPENAI_API_KEY").ok();
        std::env::remove_var("RECURSIVE_API_KEY");
        std::env::remove_var("OPENAI_API_KEY");

        let backend = Backend::spawn();
        assert!(!backend.cancel_flag.load(Ordering::SeqCst));
        backend.action_tx.send(UserAction::Interrupt).unwrap();

        // Spin until the worker observes the action (it processes
        // actions one at a time, so a brief poll is enough).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            if backend.cancel_flag.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
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

    /// Goal-147 §"Notes for the agent": the `wait_for_cancel` poll
    /// returns within ~100ms when the flag is already true. This is
    /// the kernel of the interrupt mechanism; we assert it directly
    /// rather than spinning up a full backend with a long-running
    /// MockProvider, which is fragile under workspace sandboxing.
    #[tokio::test]
    async fn run_with_cancel_flag_true_returns_quickly() {
        let flag = Arc::new(AtomicBool::new(true));
        let started = std::time::Instant::now();
        let timed =
            tokio::time::timeout(Duration::from_millis(500), wait_for_cancel(flag.clone())).await;
        let elapsed = started.elapsed();
        assert!(timed.is_ok(), "wait_for_cancel didn't return in time");
        assert!(
            elapsed < Duration::from_millis(500),
            "wait_for_cancel was too slow: {elapsed:?}"
        );
    }
}
