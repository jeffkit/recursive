//! Smoke test for the in-process agent backend.
//!
//! Wires a [`recursive::llm::MockProvider`] into the runtime, sends one
//! user message, and asserts that the assistant text bubbles back out
//! as a [`UiEvent::AssistantMessage`].
//!
//! This is the only integration test for goal-143: per-module unit
//! tests cover the rest. The point is to catch regressions in the
//! `AgentEvent → UiEvent` mapping inside `backend::TuiEventSink`.

use std::sync::Arc;
use std::time::Duration;

use recursive::llm::{Completion, MockProvider};
use recursive::AgentRuntimeBuilder;
use recursive_tui::backend::Backend;
use recursive_tui::events::{UiEvent, UserAction};

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

    // Drain events until we see the assistant message or time out.
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
            Ok(Some(_other)) => {
                // ToolCall / ToolResult / Error — not expected in this
                // scripted run, but tolerate them to avoid flakiness
                // if the kernel ever adds passthrough events.
                continue;
            }
            Ok(None) => panic!("event channel closed before assistant message"),
            Err(_) => continue, // 1s slice elapsed; loop until deadline
        }
    }

    assert!(
        got_assistant,
        "did not receive UiEvent::AssistantMessage within 5s"
    );

    let _ = backend.action_tx.send(UserAction::Shutdown);
}
