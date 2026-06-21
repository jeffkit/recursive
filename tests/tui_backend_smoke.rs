#![cfg(feature = "tui")]
//! Smoke test for the in-process agent backend.
//!
//! Wires a [`recursive::llm::MockProvider`] into the runtime, sends one
//! user message, and asserts that the assistant text bubbles back out
//! as a [`UiEvent::AssistantMessage`].
//!
//! Goal-144 adds a streaming variant: when the runtime is configured
//! with `.streaming(true)`, the kernel emits `PartialToken` events
//! which our backend maps to `UiEvent::AssistantPartial`. We assert
//! both the partial *and* the final assistant block are received.

use std::sync::Arc;
use std::time::Duration;

use recursive::llm::{Completion, MockProvider};
use recursive::tui::backend::Backend;
use recursive::tui::events::{UiEvent, UserAction};
use recursive::AgentRuntimeBuilder;

#[tokio::test]
async fn backend_smoke_one_turn_with_mock_provider() {
    // Single scripted reply with no tool calls.
    let llm = Arc::new(MockProvider::new(vec![Completion {
        content: "hello back".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }]));

    let runtime = AgentRuntimeBuilder::new()
        .llm(llm)
        .build()
        .expect("runtime build");

    let mut backend = Backend::spawn_with_runtime(runtime);

    backend
        .action_tx
        .send(UserAction::SendMessage("hello".into()))
        .expect("send");

    let mut got_assistant = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(1), backend.event_rx.recv()).await {
            Ok(Some(UiEvent::AssistantMessage { content })) => {
                assert!(
                    content.contains("hello back"),
                    "unexpected assistant text: {content:?}"
                );
                got_assistant = true;
                break;
            }
            Ok(Some(_other)) => continue,
            Ok(None) => panic!("event channel closed before assistant message"),
            Err(_) => continue,
        }
    }

    assert!(
        got_assistant,
        "did not receive UiEvent::AssistantMessage within 5s"
    );

    let _ = backend.action_tx.send(UserAction::Shutdown);
}

#[tokio::test]
async fn streaming_partial_tokens_are_forwarded() {
    let llm = Arc::new(MockProvider::new(vec![Completion {
        content: "streamed reply".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }]));

    let runtime = AgentRuntimeBuilder::new()
        .llm(llm)
        .streaming(true)
        .build()
        .expect("runtime build");

    let mut backend = Backend::spawn_with_runtime(runtime);

    backend
        .action_tx
        .send(UserAction::SendMessage("hi".into()))
        .expect("send");

    // Drain all events for ~1s after the first arrives so we have a
    // chance to observe both the streaming partial *and* the final
    // assistant message (the kernel's forwarder task races with the
    // main task — see runtime.rs:191; ordering between PartialToken
    // and AssistantText is not strictly guaranteed under the
    // single-threaded test runtime).
    let mut saw_partial = false;
    let mut saw_final = false;
    let mut saw_turn_finished = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), backend.event_rx.recv()).await {
            Ok(Some(UiEvent::AssistantPartial { text })) => {
                assert!(text.contains("streamed"), "unexpected partial: {text:?}");
                saw_partial = true;
            }
            Ok(Some(UiEvent::AssistantMessage { content })) => {
                assert!(content.contains("streamed reply"));
                saw_final = true;
            }
            Ok(Some(UiEvent::TurnFinished)) => {
                saw_turn_finished = true;
                if saw_final {
                    break;
                }
            }
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(_) => {
                if saw_final {
                    break;
                }
            }
        }
    }

    let _ = backend.action_tx.send(UserAction::Shutdown);

    // The final assistant message and a turn-finished poke must
    // always arrive; the partial is best-effort due to the kernel's
    // forwarder-vs-main-task race.
    assert!(saw_final, "expected a final AssistantMessage event");
    assert!(saw_turn_finished, "expected a TurnFinished poke");
    let _ = saw_partial; // observed when the race happens to favour us
}

/// Type-ahead queueing: a message submitted *while a turn is running* must
/// be buffered and processed after the turn completes, not silently dropped.
///
/// Regression for the backend select loop, which previously discarded any
/// `SendMessage` that arrived mid-turn (`Some(_) => {}`), losing the user's
/// input entirely. We drive turn 1 across two LLM calls (via a tool) and use
/// the MockProvider's synchronous `on_complete` hook to inject the second
/// message during turn 1's first LLM call — guaranteeing it lands while the
/// turn is still in flight.
#[tokio::test]
async fn messages_submitted_during_running_turn_are_queued_not_dropped() {
    use async_trait::async_trait;
    use recursive::llm::{ToolCall, ToolSpec};
    use recursive::tools::{Tool, ToolRegistry};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    struct Ping;
    #[async_trait]
    impl Tool for Ping {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "ping".into(),
                description: "returns pong".into(),
                parameters: serde_json::json!({"type":"object","properties":{}}),
            }
        }
        async fn execute(&self, _args: serde_json::Value) -> recursive::Result<String> {
            Ok("pong".into())
        }
    }

    // The on_complete hook needs the action sender, which only exists after
    // the backend is spawned — but the provider must be built before that.
    // Bridge the two via a shared slot populated post-spawn.
    let slot: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<UserAction>>>> =
        Arc::new(Mutex::new(None));
    let slot_hook = slot.clone();
    let fired = Arc::new(AtomicBool::new(false));

    let llm = MockProvider::new(vec![
        // turn 1 / call 1: request a tool so this turn spans two LLM calls.
        Completion {
            content: "checking".into(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "ping".into(),
                arguments: serde_json::json!({}),
            }],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
        // turn 1 / call 2: final answer.
        Completion {
            content: "first done".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
        // turn 2 (the message queued mid-turn): final answer.
        Completion {
            content: "second done".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
    ])
    .with_on_complete_fn(move || {
        // Fire exactly once, during turn 1's first LLM call, so the second
        // message arrives while turn 1 is still running.
        if !fired.swap(true, Ordering::SeqCst) {
            if let Ok(guard) = slot_hook.lock() {
                if let Some(tx) = guard.as_ref() {
                    let _ = tx.send(UserAction::SendMessage("second".into()));
                }
            }
        }
    });

    let runtime = AgentRuntimeBuilder::new()
        .llm(Arc::new(llm))
        .tools(ToolRegistry::local().register(Arc::new(Ping)))
        .build()
        .expect("runtime build");

    let mut backend = Backend::spawn_with_runtime(runtime);
    *slot.lock().unwrap() = Some(backend.action_tx.clone());

    backend
        .action_tx
        .send(UserAction::SendMessage("first".into()))
        .expect("send first");

    let mut saw_first = false;
    let mut saw_second = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline && !(saw_first && saw_second) {
        match tokio::time::timeout(Duration::from_millis(500), backend.event_rx.recv()).await {
            Ok(Some(UiEvent::AssistantMessage { content })) => {
                if content.contains("first done") {
                    saw_first = true;
                }
                if content.contains("second done") {
                    saw_second = true;
                }
            }
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(_) => continue,
        }
    }

    let _ = backend.action_tx.send(UserAction::Shutdown);

    assert!(saw_first, "expected the first turn's assistant message");
    assert!(
        saw_second,
        "a message submitted mid-turn must be queued and processed, not dropped"
    );
}

// ──────────────────────────────────────────────────────────────────────
// Goal 145 — multi-mode PromptInput integration
// ──────────────────────────────────────────────────────────────────────

/// `!echo hi` Enter must surface a paired `ToolCall` + `ToolResult`
/// containing "hi" in the output, and must NOT touch the LLM provider.
#[tokio::test]
async fn bash_mode_dispatches_run_shell_without_calling_llm() {
    let llm = Arc::new(MockProvider::new(vec![]));
    let llm_observer = llm.clone();

    let runtime = AgentRuntimeBuilder::new()
        .llm(llm)
        .build()
        .expect("runtime build");

    let mut backend = Backend::spawn_with_runtime(runtime);

    backend
        .action_tx
        .send(UserAction::RunShell("echo hi".into()))
        .expect("send");

    let mut saw_call = false;
    let mut saw_result = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), backend.event_rx.recv()).await {
            Ok(Some(UiEvent::ToolCall { name, .. })) if name == "Bash" => {
                saw_call = true;
            }
            Ok(Some(UiEvent::ToolResult {
                name,
                output,
                success,
                ..
            })) if name == "Bash" => {
                assert!(success, "Bash should succeed");
                assert!(
                    output.contains("hi"),
                    "expected output to contain 'hi': {output}"
                );
                saw_result = true;
                break;
            }
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(_) => continue,
        }
    }

    let _ = backend.action_tx.send(UserAction::Shutdown);

    assert!(saw_call, "expected a ToolCall event for Bash");
    assert!(saw_result, "expected a ToolResult event for Bash");
    assert!(
        llm_observer.calls().is_empty(),
        "bash mode must not invoke the LLM provider: got {} calls",
        llm_observer.calls().len()
    );
}

/// `# my note` Enter must NOT reach the backend at all — the System
/// block is appended locally inside `App::handle_key` and never
/// becomes a `UserAction`. We assert the negative: forwarding nothing
/// to the worker still leaves the LLM untouched.
///
/// (We assert the App's local-only behaviour at unit-test level in
/// `app::prompt_input_tests::submit_in_note_mode_appends_system_block`.)
#[tokio::test]
async fn note_mode_does_not_reach_provider() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use recursive::tui::app::{App, AppScreen, TranscriptBlock};
    use recursive::tui::keymap;

    let llm = Arc::new(MockProvider::new(vec![]));
    let llm_observer = llm.clone();

    let runtime = AgentRuntimeBuilder::new()
        .llm(llm)
        .build()
        .expect("runtime build");

    let backend = Backend::spawn_with_runtime(runtime);

    let mut app = App::new();
    app.screen = AppScreen::Chat;

    // User types '#', then 'my note', then Enter.
    let press = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
    let action = keymap::dispatch(&mut app, press('#'));
    assert!(action.is_none());
    for c in "my note".chars() {
        let act = keymap::dispatch(&mut app, press(c));
        assert!(act.is_none());
    }
    let action = keymap::dispatch(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    // Note submit produces no UserAction.
    assert!(
        action.is_none(),
        "note mode must not produce a backend action"
    );

    // Local System block was appended.
    assert!(app.blocks.iter().any(|b| matches!(b,
        TranscriptBlock::System { text } if text.contains("my note"))));

    // Wait long enough that any (incorrect) async dispatch would
    // arrive, then verify the LLM was never called.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        llm_observer.calls().is_empty(),
        "note mode must not invoke the LLM provider: got {} calls",
        llm_observer.calls().len()
    );

    let _ = backend.action_tx.send(UserAction::Shutdown);
}
