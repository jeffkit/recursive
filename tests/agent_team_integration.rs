//! Integration tests for the Agent Team multi-agent coordination features.
//!
//! Covers:
//! - `WorkerMailbox` / `WorkerRegistry` FIFO semantics
//! - `SendMessageTool` delivering messages through the registry
//! - `SpawnWorkerTool` registering/deregistering workers
//! - `WorkerMailbox` drain integration — messages injected into RunCore
//! - `TeamAddRole`, `TeamRemoveRole`, `TeamListRoles` dynamic management
//! - `AgentPool::remove_role` basic operation
//! - Coordinator → worker mid-run message injection via a mock round-trip

use recursive::llm::{mock::MockProvider, Completion};
use recursive::tasks::TaskRegistry;
use recursive::tools::send_message::{SendMessageTool, WorkerMailbox, WorkerRegistry};
use recursive::tools::Tool;
use serde_json::json;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn completion(text: &str) -> Completion {
    Completion {
        content: text.to_string(),
        tool_calls: vec![],
        finish_reason: Some("stop".to_string()),
        usage: None,
        reasoning_content: None,
    }
}

fn mock_provider(completions: Vec<Completion>) -> Arc<MockProvider> {
    Arc::new(MockProvider::new(completions))
}

// ---------------------------------------------------------------------------
// WorkerMailbox unit tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mailbox_fifo_ordering() {
    let mb = WorkerMailbox::new();
    mb.push("first".into()).await;
    mb.push("second".into()).await;
    mb.push("third".into()).await;

    let all = mb.drain_all().await;
    assert_eq!(all, vec!["first", "second", "third"]);
    assert!(mb.is_empty().await);
}

#[tokio::test]
async fn mailbox_drain_is_destructive() {
    let mb = WorkerMailbox::new();
    mb.push("msg".into()).await;
    let _ = mb.drain_all().await;
    // Second drain should return empty
    assert!(mb.drain_all().await.is_empty());
}

#[tokio::test]
async fn mailbox_pop_while_empty_returns_none() {
    let mb = WorkerMailbox::new();
    assert_eq!(mb.pop().await, None);
}

// ---------------------------------------------------------------------------
// WorkerRegistry unit tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn registry_register_and_send() {
    let reg = WorkerRegistry::new();
    let mailbox = reg.register("worker-1").await;

    // Push via registry lookup
    if let Some(mb) = reg.get("worker-1").await {
        mb.push("hello from coordinator".into()).await;
    }

    assert_eq!(
        mailbox.pop().await.as_deref(),
        Some("hello from coordinator")
    );
}

#[tokio::test]
async fn registry_deregister_removes_worker() {
    let reg = WorkerRegistry::new();
    reg.register("w1").await;
    assert!(reg.get("w1").await.is_some());

    reg.deregister("w1").await;
    assert!(reg.get("w1").await.is_none());
}

#[tokio::test]
async fn registry_active_workers_list() {
    let reg = WorkerRegistry::new();
    reg.register("alpha").await;
    reg.register("beta").await;

    let mut active = reg.active_workers().await;
    active.sort();
    assert_eq!(active, vec!["alpha", "beta"]);
}

#[tokio::test]
async fn registry_concurrent_push_and_drain() {
    let reg = WorkerRegistry::new();
    let mailbox = reg.register("concurrent-worker").await;

    let mb_clone = mailbox.clone();
    let push_task = tokio::spawn(async move {
        for i in 0..10 {
            mb_clone.push(format!("msg-{i}")).await;
        }
    });
    push_task.await.unwrap();

    let msgs = mailbox.drain_all().await;
    assert_eq!(msgs.len(), 10);
    // Messages should arrive in insertion order (FIFO).
    for (i, msg) in msgs.iter().enumerate() {
        assert_eq!(msg, &format!("msg-{i}"));
    }
}

// ---------------------------------------------------------------------------
// SendMessageTool integration tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn send_message_tool_delivers_to_registered_worker() {
    let reg = WorkerRegistry::new();
    let mailbox = reg.register("target-worker").await;

    let tool = SendMessageTool::new(reg, Arc::new(TaskRegistry::new()));
    let result = tool
        .execute(json!({
            "worker_id": "target-worker",
            "message": "check the tests pass"
        }))
        .await
        .unwrap();

    assert!(result.contains("delivered"), "unexpected result: {result}");
    assert_eq!(mailbox.pop().await.as_deref(), Some("check the tests pass"));
}

#[tokio::test]
async fn send_message_tool_unknown_worker_returns_helpful_error() {
    let reg = WorkerRegistry::new();
    reg.register("active-worker").await;

    let tool = SendMessageTool::new(reg, Arc::new(TaskRegistry::new()));
    let result = tool
        .execute(json!({
            "worker_id": "nonexistent",
            "message": "hello"
        }))
        .await
        .unwrap();

    assert!(result.contains("not found"), "unexpected: {result}");
    assert!(
        result.contains("active-worker"),
        "should list active: {result}"
    );
}

#[tokio::test]
async fn send_message_tool_spec_has_required_fields() {
    // Phase D: only `message` is strictly required; `task_id` (preferred) and
    // `worker_id` (legacy fallback) are alternative routing parameters.
    let reg = WorkerRegistry::new();
    let tool = SendMessageTool::new(reg, Arc::new(TaskRegistry::new()));
    let spec = tool.spec();

    assert_eq!(spec.name, "send_message");
    let required = spec.parameters["required"]
        .as_array()
        .expect("required array");
    let required_strs: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        required_strs.contains(&"message"),
        "expected `message` in required: {required_strs:?}"
    );

    let props = spec.parameters["properties"]
        .as_object()
        .expect("properties object");
    assert!(props.contains_key("task_id"), "spec should mention task_id");
    assert!(
        props.contains_key("worker_id"),
        "spec should still mention worker_id (legacy)"
    );
}

// ---------------------------------------------------------------------------
// Mailbox drain in RunCore (kernel integration) — mock round-trip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn worker_receives_coordinator_message_via_mailbox() {
    use recursive::kernel::{AgentKernel, TurnContext};
    use recursive::message::Message;
    use recursive::permissions::PermissionMode;
    use recursive::tools::ToolRegistry;
    use std::sync::atomic::AtomicBool;

    let mailbox = WorkerMailbox::new();
    mailbox
        .push("coordinator says: finish quickly".into())
        .await;

    // The mock LLM returns a single stop completion.
    let provider = mock_provider(vec![completion("Done, coordinator message received.")]);

    let kernel = AgentKernel::builder()
        .llm(provider)
        .tools(ToolRegistry::local())
        .max_steps(5)
        .build()
        .unwrap();

    let ctx = TurnContext {
        messages: vec![
            Message::system("You are a test worker."),
            Message::user("Do the task."),
        ],
        step_events_tx: None,
        tool_specs: kernel.tools().specs(),
        streaming: false,
        permission_hook: None,
        exploring_plan_mode: Arc::new(AtomicBool::new(false)),
        permission_mode: PermissionMode::Default,
        mailbox: Some(mailbox.clone()),
        turn: 0,
    };

    let outcome = kernel.run(ctx).await.unwrap();

    // The run should complete cleanly.
    assert!(outcome.final_text.is_some(), "should have a final message");

    // After the run the mailbox should be drained (messages consumed by kernel).
    assert!(
        mailbox.is_empty().await,
        "mailbox should be empty after drain"
    );
}

// ---------------------------------------------------------------------------
