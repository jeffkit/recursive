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
use recursive::multi::{AgentPool, AgentRole};
use recursive::tools::send_message::{SendMessageTool, WorkerMailbox, WorkerRegistry};
use recursive::tools::team_manage::{TeamAddRole, TeamListRoles, TeamRemoveRole};
use recursive::tools::Tool;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::RwLock;

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

    let tool = SendMessageTool::new(reg);
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

    let tool = SendMessageTool::new(reg);
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
    let reg = WorkerRegistry::new();
    let tool = SendMessageTool::new(reg);
    let spec = tool.spec();

    assert_eq!(spec.name, "send_message");
    let required = spec.parameters["required"]
        .as_array()
        .expect("required array");
    let required_strs: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
    assert!(required_strs.contains(&"worker_id"));
    assert!(required_strs.contains(&"message"));
}

// ---------------------------------------------------------------------------
// Mailbox drain in RunCore (kernel integration) — mock round-trip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn worker_receives_coordinator_message_via_mailbox() {
    use recursive::agent::PlanningMode;
    use recursive::event::NullSink;
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
        event_sink: Some(Box::new(NullSink)),
        step_events_tx: None,
        plan_confirmed: false,
        plan_buffer: None,
        tool_specs: kernel.tools().specs(),
        streaming: false,
        permission_hook: None,
        planning_mode: PlanningMode::default(),
        exploring_plan_mode: Arc::new(AtomicBool::new(false)),
        permission_mode: PermissionMode::Allow,
        mailbox: Some(mailbox.clone()),
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
// Dynamic team management (TeamAddRole / TeamRemoveRole / TeamListRoles)
// ---------------------------------------------------------------------------

fn test_config() -> recursive::Config {
    recursive::Config {
        workspace: std::path::PathBuf::from("."),
        api_base: "http://localhost:4010/v1".into(),
        api_key: Some("test-key".into()),
        model: "mock-model".into(),
        provider_type: "openai".into(),
        max_steps: 10,
        temperature: 0.2,
        system_prompt: "You are a helpful assistant.".into(),
        retry_max: 0,
        retry_initial_backoff_secs: 1,
        retry_max_backoff_secs: 10,
        shell_timeout_secs: 30,
        memory_summary_limit: 5,
    }
}

fn make_pool() -> Arc<RwLock<AgentPool>> {
    let provider: Arc<dyn recursive::LlmProvider> = mock_provider(vec![completion("ok")]);

    let mut pool = AgentPool::new(provider, test_config());
    pool.add_role(AgentRole {
        name: "analyst".into(),
        system_prompt: "Analyse data.".into(),
        allowed_tools: vec![],
        max_steps: 10,
    });
    Arc::new(RwLock::new(pool))
}

#[tokio::test]
async fn team_add_role_creates_new_role() {
    let pool = make_pool();
    let tool = TeamAddRole::new(pool.clone());

    let result = tool
        .execute(json!({
            "name": "reviewer",
            "system_prompt": "Review code carefully.",
            "max_steps": 15
        }))
        .await
        .unwrap();

    assert!(result.contains("reviewer"), "result: {result}");

    let p = pool.read().await;
    assert!(p.get_role("reviewer").is_some(), "reviewer should exist");
}

#[tokio::test]
async fn team_add_role_updates_existing_role() {
    let pool = make_pool();
    let tool = TeamAddRole::new(pool.clone());

    // Add analyst with new system prompt
    let result = tool
        .execute(json!({
            "name": "analyst",
            "system_prompt": "Updated analysis prompt.",
            "max_steps": 20
        }))
        .await
        .unwrap();

    assert!(result.contains("analyst"), "result: {result}");

    let p = pool.read().await;
    assert!(p.get_role("analyst").is_some());
    let role = p.get_role("analyst").unwrap();
    assert_eq!(role.system_prompt, "Updated analysis prompt.");
    assert_eq!(role.max_steps, 20);
}

#[tokio::test]
async fn team_remove_role_removes_existing_role() {
    let pool = make_pool();
    let tool = TeamRemoveRole::new(pool.clone());

    let result = tool.execute(json!({"name": "analyst"})).await.unwrap();

    assert!(result.contains("analyst"), "result: {result}");
    assert!(pool.read().await.get_role("analyst").is_none());
}

#[tokio::test]
async fn team_remove_role_nonexistent_returns_message() {
    let pool = make_pool();
    let tool = TeamRemoveRole::new(pool.clone());

    let result = tool
        .execute(json!({"name": "nonexistent-role"}))
        .await
        .unwrap();

    assert!(result.contains("nonexistent-role"), "result: {result}");
}

#[tokio::test]
async fn team_list_roles_shows_all_roles() {
    let pool = make_pool();
    {
        let mut p = pool.write().await;
        p.add_role(AgentRole {
            name: "coder".into(),
            system_prompt: "Write code.".into(),
            allowed_tools: vec![],
            max_steps: 30,
        });
    }

    let tool = TeamListRoles::new(pool.clone());
    let result = tool.execute(json!({})).await.unwrap();

    assert!(result.contains("analyst"), "should list analyst: {result}");
    assert!(result.contains("coder"), "should list coder: {result}");
}

#[tokio::test]
async fn team_list_roles_empty_pool() {
    let provider: Arc<dyn recursive::LlmProvider> = mock_provider(vec![completion("ok")]);
    let pool = Arc::new(RwLock::new(AgentPool::new(provider, test_config())));
    let tool = TeamListRoles::new(pool);

    let result = tool.execute(json!({})).await.unwrap();
    assert!(
        result.contains("No roles")
            || result.contains("empty")
            || result.is_empty()
            || result.len() < 50,
        "result should indicate no roles: {result}"
    );
}

// ---------------------------------------------------------------------------
// AgentPool dynamic role management (lower-level API)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn agent_pool_add_and_remove_role() {
    let provider: Arc<dyn recursive::LlmProvider> = mock_provider(vec![completion("ok")]);
    let mut pool = AgentPool::new(provider, test_config());

    pool.add_role(AgentRole {
        name: "tester".into(),
        system_prompt: "Write tests.".into(),
        allowed_tools: vec![],
        max_steps: 20,
    });

    assert!(
        pool.get_role("tester").is_some(),
        "role should be present after add"
    );
    let removed = pool.remove_role("tester");
    assert!(removed, "remove_role should return true for existing role");
    assert!(
        pool.get_role("tester").is_none(),
        "role should be gone after remove"
    );
}

#[tokio::test]
async fn agent_pool_remove_nonexistent_returns_false() {
    let provider: Arc<dyn recursive::LlmProvider> = mock_provider(vec![completion("ok")]);
    let mut pool = AgentPool::new(provider, test_config());
    assert!(!pool.remove_role("phantom"));
}
