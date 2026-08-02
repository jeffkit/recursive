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
use recursive::message::Role;
use recursive::multi::{AgentMessage, AgentPool, AgentRole, MessageBus, MessageType};
use recursive::tasks::TaskRegistry;
use recursive::tools::send_message::{SendMessageTool, WorkerMailbox, WorkerRegistry};
use recursive::tools::Tool;
use recursive::{Config, FinishReason};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::broadcast::error::TryRecvError;

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

/// Minimal `Config` for constructing an `AgentPool`. `AgentPool::new`
/// ignores the config (the pool only needs the provider + roles), but the
/// struct has no `Default`, so tests must supply every field.
fn test_config() -> Config {
    Config {
        workspace: PathBuf::from("."),
        api_base: "http://localhost:11434/v1".into(),
        api_key: Some("test-key".into()),
        model: "test-model".into(),
        provider_type: "openai".into(),
        preset: None,
        max_steps: 10,
        max_tokens: 65536,
        temperature: 0.2,
        system_prompt: "You are a helpful assistant.".into(),
        retry_max: 0,
        retry_initial_backoff_secs: 1,
        retry_max_backoff_secs: 10,
        shell_timeout_secs: 30,
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
        subagent_enabled: false,
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

/// Build an `AgentMessage` addressed from one role to another (Task type).
fn bus_message(from: &str, to: &str, content: &str) -> AgentMessage {
    AgentMessage {
        id: format!("msg-{from}->{to}"),
        from: from.to_string(),
        to: to.to_string(),
        content: content.to_string(),
        msg_type: MessageType::Task,
        timestamp: 1,
    }
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

    let tool = SendMessageTool::new(
        reg,
        Arc::new(TaskRegistry::new()),
        <recursive::tools::agent::WorkerTable as Default>::default(),
    );
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

    let tool = SendMessageTool::new(
        reg,
        Arc::new(TaskRegistry::new()),
        <recursive::tools::agent::WorkerTable as Default>::default(),
    );
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
    let tool = SendMessageTool::new(
        reg,
        Arc::new(TaskRegistry::new()),
        <recursive::tools::agent::WorkerTable as Default>::default(),
    );
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
        messages: Arc::new(vec![
            Message::system("You are a test worker."),
            Message::user("Do the task."),
        ]),
        step_events_tx: None,
        tool_specs: kernel.tools().specs(),
        streaming: false,
        permission_hook: None,
        exploring_plan_mode: Arc::new(AtomicBool::new(false)),
        permission_mode: PermissionMode::Default,
        mailbox: Some(mailbox.clone()),
        turn: 0,
        prompt_segments: None,
        wall_timeout_secs: 0,
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
// MessageBus inter-role routing (Goal 362)
// ---------------------------------------------------------------------------
//
// The `MessageBus` (src/multi.rs) is the pub/sub layer routing `AgentMessage`s
// between roles in multi-agent mode. The tests below pin its send→subscribe
// contract, its (non-)replay behaviour for late subscribers, and — the
// load-bearing one — whether a message posted on the bus surfaces in an actual
// `AgentPool::run_with_role` turn.

#[tokio::test]
async fn message_bus_routes_message_to_subscribed_role() {
    let bus = MessageBus::new();
    let mut worker_rx = bus.subscribe("worker").await;
    let mut reviewer_rx = bus.subscribe("reviewer").await;

    let msg = bus_message("coordinator", "worker", "implement module Z");
    bus.send(msg.clone()).await;

    // The subscribed worker role receives the message with the payload intact.
    let received = worker_rx
        .recv()
        .await
        .expect("subscribed worker must receive the bus message");
    assert_eq!(received.id, msg.id);
    assert_eq!(received.from, "coordinator");
    assert_eq!(received.to, "worker");
    assert_eq!(received.content, "implement module Z");
    assert_eq!(received.msg_type, MessageType::Task);

    // A different subscribed role must NOT receive a message addressed to
    // "worker" (routing is role-keyed, not a broadcast to everyone).
    assert!(
        matches!(reviewer_rx.try_recv(), Err(TryRecvError::Empty)),
        "reviewer must not receive a message routed to worker"
    );
}

#[tokio::test]
async fn message_bus_history_replay_surfaces_past_messages() {
    let bus = MessageBus::new();

    // Send BEFORE any subscriber exists.
    let msg = bus_message("coordinator", "worker", "MESSAGE_FROM_COORDINATOR");
    bus.send(msg.clone()).await;

    // Subscribe AFTER the send.
    let mut rx = bus.subscribe("worker").await;

    // Contract pinned: tokio `broadcast` receivers only observe messages sent
    // after subscription — a late subscriber does NOT get pre-subscription
    // messages replayed through the broadcast channel (`subscribe()` simply
    // calls `tx.subscribe()`, which starts at the current channel position).
    assert!(
        matches!(rx.try_recv(), Err(TryRecvError::Empty)),
        "late subscriber must not receive pre-subscription messages via broadcast rx"
    );

    // The same message IS retained in the bus history and remains retrievable
    // for the target role via `inbox()`.
    let inbox = bus.inbox("worker").await;
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].content, "MESSAGE_FROM_COORDINATOR");
    assert_eq!(inbox[0].from, "coordinator");
    assert_eq!(inbox[0].to, "worker");
}

#[tokio::test]
async fn run_with_role_includes_bus_message_in_context() {
    let provider = mock_provider(vec![completion("Worker done.")]);
    let mut pool = AgentPool::new(provider.clone(), test_config());
    pool.add_role(AgentRole {
        name: "worker".into(),
        system_prompt: "You are a worker.".into(),
        max_steps: 5,
        allowed_tools: vec![],
    });

    // Subscribe the worker role and post a coordinator message on the bus.
    let _worker_rx = pool.bus().subscribe("worker").await;
    pool.bus()
        .send(bus_message(
            "coordinator",
            "worker",
            "MESSAGE_FROM_COORDINATOR",
        ))
        .await;

    // Sanity: the message is on the bus and routable to the worker role.
    let inbox = pool.bus().inbox("worker").await;
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].content, "MESSAGE_FROM_COORDINATOR");

    // One-shot turn: the mock returns a stop completion immediately, so the
    // agent makes exactly one LLM call and no tool calls.
    let outcome = pool.run_with_role("worker", "continue work").await.unwrap();
    assert_eq!(
        outcome.finish_reason,
        FinishReason::NoMoreToolCalls,
        "one-shot mock turn must finish without tool calls"
    );

    // The mock provider records the exact transcript it was sent; the first
    // message is the system prompt `run_with_role` injected.
    let calls = provider.calls();
    assert_eq!(
        calls.len(),
        1,
        "one-shot turn must make exactly one LLM call"
    );
    let system = &calls[0][0];
    assert_eq!(system.role, Role::System);
    assert!(
        system.content.contains("You are a worker."),
        "role system prompt must be present in the turn"
    );

    // KNOWN GAP (pinned by this test — see journal
    // `.dev/journal/manual-20260802-goal362-messagebus-tests.md`):
    // `AgentPool::run_with_role` does NOT inject MessageBus state into the
    // agent turn today. The bus is a standalone pub/sub; nothing reads it
    // when the turn's context is built (unlike `SharedMemory`, which IS
    // appended to the system prompt). The message lives on the bus
    // (`inbox("worker")` above) but never reaches the worker's transcript.
    //
    // This assertion pins the CURRENT observable behaviour so the gap cannot
    // change silently. When a follow-up goal wires the bus into the turn
    // (the intended fix), flip this to `assert!(system.content.contains(...))`.
    assert!(
        !system.content.contains("MESSAGE_FROM_COORDINATOR"),
        "bus messages are not yet injected into run_with_role context (pinned gap)"
    );
}
