//! Issue #40 blocker 1 / Goal 407 — the two remaining cancellation surfaces
//! (REPL and the weixin headless daemon).
//!
//! The REPL builds its runtime **once** and serves many turns. A static
//! shutdown token is unusable there (irrevocable: the first Ctrl-C would leave
//! every later turn pre-cancelled), so it installs a per-turn child token and
//! mirrors it into the `agent` tool's `SharedTokenSlot`. These tests exercise
//! exactly that wiring — the same `register_subagent_if_enabled` call
//! `cli::builder::build_runtime` makes — against a provider that parks inside
//! `complete()`:
//!
//! 1. `repl_per_turn_token_cancels_parallel_run_then_next_turn_succeeds`
//!    — turn 1 is cancelled mid-parallel-run and is recorded as `Cancelled`
//!    (not a hang), turn 2 runs to completion on the same runtime.
//! 2. `static_token_poisons_the_next_turn` — the contrast case that motivates
//!    the per-turn design: with a static token, turn 2 never reaches the
//!    provider.
//! 3. `weixin_static_token_drains_in_flight_turn` — the unattended daemon's
//!    shape: one process-lifetime token cancels the in-flight turn.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use recursive::agent::FinishReason;
use recursive::llm::{ChatProvider, Completion, ToolSpec};
use recursive::message::{Message, Role};
use recursive::multi::register_subagent_if_enabled;
use recursive::tools::{LocalTransport, ReadFile, ToolRegistry};
use recursive::{AgentRuntime, Config};
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// Marker placed in the worker system prompt; completions whose transcript
/// carries it belong to a sub-agent worker (they park, like a stalled gateway).
const WORKER_MARKER: &str = "SLOW-WORKER";

/// Parent prompt that makes the scripted provider delegate to two workers.
const DELEGATE_PROMPT: &str = "delegate to two workers";

/// Provider that scripts a REPL-like transcript:
/// * worker turns (`WORKER_MARKER` present) park forever — the issue-40 stall;
/// * the first parent turn asks for a `parallel` `agent` call;
/// * every later parent turn answers normally.
struct ReplScriptedProvider {
    parent_calls: AtomicUsize,
}

impl ReplScriptedProvider {
    fn new() -> Self {
        Self {
            parent_calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl ChatProvider for ReplScriptedProvider {
    async fn complete(
        &self,
        messages: &[Message],
        _tools: &[ToolSpec],
    ) -> recursive::Result<Completion> {
        if messages.iter().any(|m| m.content.contains(WORKER_MARKER)) {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            unreachable!("stalled worker call must be cancelled, not awaited out")
        }
        let last_user = messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| m.content.clone())
            .unwrap_or_default();
        let call_index = self.parent_calls.fetch_add(1, Ordering::SeqCst);
        if call_index == 0 && last_user.contains(DELEGATE_PROMPT) {
            return Ok(Completion {
                content: "dispatching workers".into(),
                tool_calls: vec![recursive::llm::ToolCall {
                    id: "call-agent".into(),
                    name: "agent".into(),
                    arguments: json!({
                        "mode": "parallel",
                        "manifest": {
                            "worker-a": {
                                "system_prompt": format!("You are a researcher. {WORKER_MARKER}"),
                                "allowed_tools": ["Read"]
                            },
                            "worker-b": {
                                "system_prompt": format!("You are a researcher. {WORKER_MARKER}"),
                                "allowed_tools": ["Read"]
                            }
                        },
                        "prompt": "deeply research this repo",
                        "max_steps": 45
                    }),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
                reasoning_content: None,
            });
        }
        Ok(Completion {
            content: "done".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        })
    }

    async fn stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _tx: Option<recursive::llm::StreamSender>,
        _cancel: Option<CancellationToken>,
    ) -> recursive::Result<Completion> {
        unreachable!("stream not used by these tests")
    }
}

fn test_config(workspace: PathBuf) -> Config {
    Config {
        workspace,
        api_base: "http://127.0.0.1:1/v1".into(),
        api_key: Some("test-key".into()),
        model: "test-model".into(),
        provider_type: "openai".into(),
        preset: None,
        max_steps: 10,
        max_tokens: 65536,
        temperature: 0.0,
        system_prompt: "You are a helpful assistant.".into(),
        retry_max: 0,
        retry_initial_backoff_secs: 1,
        retry_max_backoff_secs: 1,
        shell_timeout_secs: 5,
        headless: false,
        memory_summary_limit: 5,
        thinking_budget: None,
        session_name: None,
        max_budget_usd: None,
        extra_dirs: Vec::new(),
        extra_readonly_dirs: Vec::new(),
        allow_tools: Vec::new(),
        context_window_override: None,
        subagent_max_depth: 2,
        subagent_enabled: true,
        allow_bypass_permissions: false,
        max_search_rounds: 3,
        stuck_window: 10,
        stuck_error_rate: 0.8,
        max_concurrent_runs: 8,
        goal_eval_transcript_tail: 12,
        web_search_provider: None,
        web_search_api_key: None,
        web_search_jina_key: None,
        wall_timeout_secs: 0,
    }
}

fn tool_registry(workspace: &std::path::Path) -> ToolRegistry {
    ToolRegistry::new(Arc::new(LocalTransport)).register(Arc::new(ReadFile::new(workspace)))
}

/// Build the runtime the way `cli::builder::build_runtime` does for the REPL:
/// the `agent` tool is wired to a caller-owned per-turn token slot, and the
/// kernel token is installed per turn by the host (`set_interrupt_token`).
async fn repl_runtime(
    slot: Option<recursive::SharedTokenSlot>,
) -> (AgentRuntime, Arc<ReplScriptedProvider>) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspace = tmp.path().to_path_buf();
    std::fs::write(workspace.join("Cargo.toml"), "[package]\n").expect("seed file");
    // Keep the temp dir alive for the runtime's lifetime.
    std::mem::forget(tmp);

    let provider = Arc::new(ReplScriptedProvider::new());
    let provider_dyn: Arc<dyn ChatProvider> = provider.clone();
    let tools = register_subagent_if_enabled(
        tool_registry(&workspace),
        &test_config(workspace.clone()),
        provider_dyn.clone(),
        slot,
    );
    let runtime = AgentRuntime::builder()
        .llm(provider_dyn)
        .tools(tools)
        .build()
        .expect("runtime builds");
    (runtime, provider)
}

fn slot_with(token: CancellationToken) -> recursive::SharedTokenSlot {
    Arc::new(Mutex::new(Some(token)))
}

fn clear_slot(slot: &recursive::SharedTokenSlot) {
    *slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Acceptance #1: a Ctrl-C during a parallel sub-agent run cancels *that turn*
/// (`FinishReason::Cancelled`, every worker paired) and the next turn on the
/// same runtime runs normally — no static-token poisoning.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repl_per_turn_token_cancels_parallel_run_then_next_turn_succeeds() {
    let turn1 = CancellationToken::new();
    let slot = slot_with(turn1.clone());
    let (mut runtime, _provider) = repl_runtime(Some(slot.clone())).await;

    // Turn 1 — the host installs the turn token on the kernel and mirrors it
    // into the agent tool's slot (exactly what `repl()` does).
    runtime.set_interrupt_token(turn1.clone());
    let cancel = turn1.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        cancel.cancel();
    });

    let outcome = tokio::time::timeout(Duration::from_secs(20), runtime.run(DELEGATE_PROMPT))
        .await
        .expect("a cancelled parallel agent run must not park the turn (issue #40)")
        .expect("finish is data, not Err");
    assert!(
        matches!(outcome.finish_reason, FinishReason::Cancelled),
        "turn 1 must finish as Cancelled, got {:?}",
        outcome.finish_reason
    );

    // Turn boundary: the host clears the slot (end_turn).
    clear_slot(&slot);

    // Turn 2 — a FRESH token; the runtime/provider are otherwise untouched.
    let turn2 = CancellationToken::new();
    assert!(
        !turn2.is_cancelled(),
        "a fresh turn token must not inherit the previous cancellation"
    );
    runtime.set_interrupt_token(turn2.clone());
    *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(turn2.clone());
    let outcome = tokio::time::timeout(Duration::from_secs(20), runtime.run("second turn"))
        .await
        .expect("the second turn must not hang")
        .expect("finish is data, not Err");
    assert!(
        matches!(outcome.finish_reason, FinishReason::NoMoreToolCalls),
        "the turn after an interrupt must run normally, got {:?}",
        outcome.finish_reason
    );
    assert_eq!(
        outcome.final_text.as_deref(),
        Some("done"),
        "turn 2 must reach the provider and return its text"
    );
    clear_slot(&slot);
}

/// The motivating contrast: with a static kernel token (the anti-pattern for a
/// multi-turn surface) turn 2 comes back `Cancelled` before the provider is
/// ever consulted. Keeps the per-turn design honest.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn static_token_poisons_the_next_turn() {
    let (mut runtime, provider) = repl_runtime(None).await;

    let static_token = CancellationToken::new();
    runtime.set_interrupt_token(static_token.clone());

    // Turn 1 — cancel before it starts: an immediately-cancelled turn.
    static_token.cancel();
    let first = tokio::time::timeout(Duration::from_secs(20), runtime.run("first turn"))
        .await
        .expect("cancelled turn returns promptly")
        .expect("finish is data, not Err");
    assert!(matches!(first.finish_reason, FinishReason::Cancelled));

    // Turn 2 — same (never replaced) token ⇒ poisoned, provider never reached.
    let calls_before = provider.parent_calls.load(Ordering::SeqCst);
    let second = tokio::time::timeout(Duration::from_secs(20), runtime.run("second turn"))
        .await
        .expect("poisoned turn returns promptly")
        .expect("finish is data, not Err");
    assert!(
        matches!(second.finish_reason, FinishReason::Cancelled),
        "a static token stays cancelled forever (that is why the REPL mints \
         a fresh child per turn), got {:?}",
        second.finish_reason
    );
    assert_eq!(
        provider.parent_calls.load(Ordering::SeqCst),
        calls_before,
        "the poisoned turn must not reach the provider"
    );
}

/// Acceptance #2 (runtime half): the weixin daemon's static token drains an
/// in-flight turn as `Cancelled`; the CLI-side loop-break half is covered by
/// `weixin_loop_stops_on_shutdown_but_serves_pending_requests`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn weixin_static_token_drains_in_flight_turn() {
    let shutdown = CancellationToken::new();
    let slot = slot_with(shutdown.clone()); // one-shot filled slot, as build_runtime mints
    let (mut runtime, _provider) = repl_runtime(Some(slot)).await;
    runtime.set_interrupt_token(shutdown.clone());

    let cancel = shutdown.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        cancel.cancel();
    });

    let outcome = tokio::time::timeout(Duration::from_secs(20), runtime.run(DELEGATE_PROMPT))
        .await
        .expect("SIGTERM must not park the daemon's in-flight turn")
        .expect("finish is data, not Err");
    assert!(
        matches!(outcome.finish_reason, FinishReason::Cancelled),
        "the daemon's in-flight turn must be drained as Cancelled, got {:?}",
        outcome.finish_reason
    );
}
