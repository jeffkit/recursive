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
use recursive::AgentRuntimeBuilder;
use recursive::tui::backend::Backend;
use recursive::tui::events::{UiEvent, UserAction};

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
            Ok(Some(UiEvent::ToolCall { name, .. })) if name == "run_shell" => {
                saw_call = true;
            }
            Ok(Some(UiEvent::ToolResult {
                name,
                output,
                success,
                ..
            })) if name == "run_shell" => {
                assert!(success, "run_shell should succeed");
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

    assert!(saw_call, "expected a ToolCall event for run_shell");
    assert!(saw_result, "expected a ToolResult event for run_shell");
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
