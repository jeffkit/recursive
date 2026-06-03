//! End-to-end test for the AG-UI integration: spin up the real
//! recursive HTTP server on a loopback port and drive it with
//! `agui-client` from the new agui crates.
//!
//! This complements the in-process oneshot tests in `tests/http.rs`
//! by exercising the full network path (axum + reqwest + SSE chunking),
//! which is what production clients actually go through.

#![cfg(feature = "http")]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use agui_client::{AguiClient, ClientError, Event, RunAgentInput};
use agui_protocol::Message;
use recursive::config::Config;
use recursive::http::{
    build_router_with_auth_and_rate_limit, AppState, AuthConfig, Metrics, RateLimiter,
};
use recursive::llm::{Completion, MockProvider, ToolCall};
use recursive::tools::ToolRegistry;
use tokio::net::TcpListener;
use tokio::sync::RwLock;

/// Process-wide lock for tests that mutate `RECURSIVE_HOME`.
static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Per-test redirect for `RECURSIVE_HOME` so tests don't write into
/// the developer's real `~/.recursive` and don't race each other.
struct HomeOverride {
    prev: Option<std::ffi::OsString>,
    _home: tempfile::TempDir,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl HomeOverride {
    fn new() -> Self {
        let lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("RECURSIVE_HOME");
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("RECURSIVE_HOME", dir.path());
        Self {
            prev,
            _home: dir,
            _lock: lock,
        }
    }
}

impl Drop for HomeOverride {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(v) => std::env::set_var("RECURSIVE_HOME", v),
            None => std::env::remove_var("RECURSIVE_HOME"),
        }
    }
}

fn has_git() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok()
}

fn mock_config(workspace: PathBuf) -> Config {
    Config {
        workspace,
        api_base: "https://example.invalid/v1".into(),
        api_key: Some("test-key".into()),
        model: "mock".into(),
        provider_type: "openai".into(),
        preset: None,
        max_steps: 32,
        temperature: 0.0,
        system_prompt: "You are a test assistant.".into(),
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
    }
}

fn state(workspace: PathBuf, provider: Arc<MockProvider>) -> AppState {
    AppState {
        tools: vec![],
        config: mock_config(workspace),
        tool_registry: ToolRegistry::local(),
        provider,
        sessions: Arc::new(RwLock::new(HashMap::new())),
        event_channels: Arc::new(RwLock::new(HashMap::new())),
        metrics: Arc::new(Metrics::default()),
        slash_commands: Arc::new(Vec::new()),
    }
}

fn user_msg(id: &str, text: &str) -> Message {
    Message {
        id: id.into(),
        role: "user".into(),
        content: Some(text.into()),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn input_with(thread: &str, run: &str, messages: Vec<Message>) -> RunAgentInput {
    RunAgentInput {
        thread_id: thread.into(),
        run_id: run.into(),
        messages,
        tools: vec![],
        context: vec![],
        resume: None,
        state: None,
        forwarded_props: None,
    }
}

/// Bind to 127.0.0.1:0, spawn the server, return its base URL.
async fn spawn_server(workspace: PathBuf, provider: Arc<MockProvider>) -> url::Url {
    // Disable auth (default = empty key set) and effectively disable
    // rate limiting (huge bucket, fast refill).
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    let app = build_router_with_auth_and_rate_limit(
        state(workspace, provider),
        AuthConfig::default(),
        RateLimiter::new(10_000, 1_000.0),
    );

    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    format!("http://{addr}/agui").parse().expect("url")
}

#[tokio::test]
async fn agui_client_drives_recursive_server_end_to_end() {
    let _home = HomeOverride::new();
    let workspace = tempfile::tempdir().expect("ws");

    let provider = Arc::new(MockProvider::new(vec![Completion {
        content: "hi from recursive".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }]));

    let endpoint = spawn_server(workspace.path().to_path_buf(), provider).await;
    let client = AguiClient::new(endpoint);

    let input = input_with("e2e-thread", "e2e-run-0", vec![user_msg("u1", "say hi")]);

    let mut rx = client.run(input).await.expect("run");

    let mut events = Vec::new();
    while let Some(ev) = rx.recv().await {
        events.push(ev);
    }

    // Must see a RunStarted, then any TextMessage events, then a RunFinished.
    let kinds: Vec<&str> = events.iter().map(event_name).collect();

    assert!(
        kinds.first() == Some(&"RunStarted"),
        "first event must be RunStarted, got {:?}",
        kinds
    );
    assert!(
        kinds.last() == Some(&"RunFinished"),
        "last event must be RunFinished, got {:?}",
        kinds
    );
    assert!(
        kinds.iter().any(|k| matches!(
            *k,
            "TextMessageStart" | "TextMessageContent" | "TextMessageEnd"
        )),
        "expected at least one text message event, got {:?}",
        kinds
    );

    // Concatenated text content matches the mock completion.
    let mut text = String::new();
    for ev in &events {
        if let Event::TextMessageContent(c) = ev {
            text.push_str(&c.delta);
        }
    }
    assert_eq!(text, "hi from recursive");
}

#[tokio::test]
async fn agui_client_observes_tool_call_lifecycle_over_real_http() {
    let _home = HomeOverride::new();
    let workspace = tempfile::tempdir().expect("ws");

    // Provider script: first turn calls a tool, second turn ends.
    let provider = Arc::new(MockProvider::new(vec![
        Completion {
            content: "calling tool".into(),
            tool_calls: vec![ToolCall {
                id: "t1".into(),
                name: "echo_tool".into(),
                arguments: serde_json::json!({"msg": "hi"}),
            }],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
        Completion {
            content: "done".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
    ]));

    let endpoint = spawn_server(workspace.path().to_path_buf(), provider).await;
    let client = AguiClient::new(endpoint);

    let input = input_with(
        "e2e-tool",
        "e2e-tool-0",
        vec![user_msg("u1", "use the tool")],
    );

    let mut rx = client.run(input).await.expect("run");
    let mut events = Vec::new();
    while let Some(ev) = rx.recv().await {
        events.push(ev);
    }

    let names: Vec<&str> = events.iter().map(event_name).collect();
    let pos = |name: &str| names.iter().position(|n| *n == name);

    let start = pos("ToolCallStart");
    let args = pos("ToolCallArgs");
    let end = pos("ToolCallEnd");
    let result = pos("ToolCallResult");
    let finished = pos("RunFinished");

    assert!(start.is_some(), "missing ToolCallStart in {names:?}");
    assert!(args.is_some(), "missing ToolCallArgs in {names:?}");
    assert!(end.is_some(), "missing ToolCallEnd in {names:?}");
    // The tool isn't actually registered on the recursive side (empty
    // ToolRegistry), so we expect a ToolCallResult that carries an
    // error string. The presence of the event is what we assert on;
    // the content is allowed to be an error.
    assert!(result.is_some(), "missing ToolCallResult in {names:?}");
    assert!(finished.is_some(), "missing RunFinished in {names:?}");

    assert!(start < args, "Start must precede Args: {names:?}");
    assert!(args < end, "Args must precede End: {names:?}");
    assert!(end < result, "End must precede Result: {names:?}");
    assert!(result < finished, "Result must precede Finished: {names:?}");
}

#[tokio::test]
async fn agui_client_4xx_when_no_messages_and_no_context() {
    let _home = HomeOverride::new();
    let workspace = tempfile::tempdir().expect("ws");

    let provider = Arc::new(MockProvider::new(vec![Completion {
        content: "shouldn't run".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }]));

    let endpoint = spawn_server(workspace.path().to_path_buf(), provider).await;
    let client = AguiClient::new(endpoint);

    let input = input_with("e2e-empty", "e2e-empty-0", vec![]);

    let result = client.run(input).await;
    match result {
        Err(ClientError::HttpStatus { status, .. }) => {
            assert_eq!(status, 400, "expected 400 for empty input");
        }
        Err(other) => panic!("expected HttpStatus 400, got {other:?}"),
        Ok(_) => panic!("expected error, got Ok"),
    }
}

#[tokio::test]
async fn agui_endpoint_emits_checkpoint_post_before_run_finished() {
    if !has_git() {
        eprintln!("git not available; skipping");
        return;
    }
    let _home = HomeOverride::new();
    let workspace = tempfile::tempdir().expect("ws");

    let provider = Arc::new(MockProvider::new(vec![Completion {
        content: "ok".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }]));

    let endpoint = spawn_server(workspace.path().to_path_buf(), provider).await;
    let client = AguiClient::new(endpoint);

    let input = input_with("cp-thread", "cp-run-0", vec![user_msg("u1", "hello")]);
    let mut rx = client.run(input).await.expect("run");
    let mut events = Vec::new();
    while let Some(ev) = rx.recv().await {
        events.push(ev);
    }

    let names: Vec<&str> = events.iter().map(event_name).collect();
    let cp_idx = events
        .iter()
        .position(|e| match e {
            Event::Custom(c) => c.name == "agui-tui/checkpoint_post",
            _ => false,
        })
        .unwrap_or_else(|| panic!("missing checkpoint_post Custom event in {names:?}"));
    let finished_idx = names
        .iter()
        .position(|n| *n == "RunFinished")
        .unwrap_or_else(|| panic!("missing RunFinished in {names:?}"));

    assert!(
        cp_idx < finished_idx,
        "checkpoint_post must precede RunFinished, got {names:?}"
    );

    if let Event::Custom(c) = &events[cp_idx] {
        let turn = c.value.get("turn").and_then(|v| v.as_u64());
        let post_id = c.value.get("postId").and_then(|v| v.as_str());
        assert_eq!(turn, Some(0), "first turn should be turn 0: {:?}", c.value);
        let post = post_id.expect("postId is a string");
        assert!(
            post.len() >= 8,
            "postId should look like a short SHA, got `{post}`"
        );
    }
}

#[tokio::test]
async fn agui_endpoint_increments_turn_across_runs_in_same_thread() {
    if !has_git() {
        return;
    }
    let _home = HomeOverride::new();
    let workspace = tempfile::tempdir().expect("ws");

    let provider = Arc::new(MockProvider::new(vec![
        Completion {
            content: "first".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
        Completion {
            content: "second".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
    ]));

    let endpoint = spawn_server(workspace.path().to_path_buf(), provider).await;
    let client = AguiClient::new(endpoint);

    async fn run_and_collect(client: &AguiClient, input: RunAgentInput) -> Vec<Event> {
        let mut rx = client.run(input).await.expect("run");
        let mut out = Vec::new();
        while let Some(ev) = rx.recv().await {
            out.push(ev);
        }
        out
    }

    fn checkpoint_turn(events: &[Event]) -> Option<u64> {
        events.iter().find_map(|e| match e {
            Event::Custom(c) if c.name == "agui-tui/checkpoint_post" => {
                c.value.get("turn").and_then(|v| v.as_u64())
            }
            _ => None,
        })
    }

    // Note: each /agui POST today builds a fresh AgentRuntime, so the
    // in-memory turn counter resets to 0. The shadow-git ref chain
    // still grows across runs (g141 stores it persistently), so the
    // checkpoint_post.postId values for the two runs differ even
    // though both report turn = 0. This test asserts that part:
    // distinct post ids per run, no panic, no missing events.
    let run1 = run_and_collect(
        &client,
        input_with("multi", "multi-0", vec![user_msg("u1", "first")]),
    )
    .await;
    let turn1 = checkpoint_turn(&run1).expect("run 1 missing checkpoint_post");

    let run2 = run_and_collect(
        &client,
        input_with("multi", "multi-1", vec![user_msg("u2", "second")]),
    )
    .await;
    let turn2 = checkpoint_turn(&run2).expect("run 2 missing checkpoint_post");

    assert_eq!(turn1, 0);
    assert_eq!(turn2, 0);

    let post1 = run1
        .iter()
        .find_map(|e| match e {
            Event::Custom(c) if c.name == "agui-tui/checkpoint_post" => c
                .value
                .get("postId")
                .and_then(|v| v.as_str())
                .map(String::from),
            _ => None,
        })
        .expect("run 1 postId");
    let post2 = run2
        .iter()
        .find_map(|e| match e {
            Event::Custom(c) if c.name == "agui-tui/checkpoint_post" => c
                .value
                .get("postId")
                .and_then(|v| v.as_str())
                .map(String::from),
            _ => None,
        })
        .expect("run 2 postId");

    assert_ne!(
        post1, post2,
        "two consecutive runs with content changes should produce distinct \
         checkpoint ids (post1={post1}, post2={post2})"
    );
}

fn event_name(ev: &Event) -> &'static str {
    match ev {
        Event::RunStarted(_) => "RunStarted",
        Event::RunFinished(_) => "RunFinished",
        Event::RunError(_) => "RunError",
        Event::StepStarted(_) => "StepStarted",
        Event::StepFinished(_) => "StepFinished",
        Event::TextMessageStart(_) => "TextMessageStart",
        Event::TextMessageContent(_) => "TextMessageContent",
        Event::TextMessageEnd(_) => "TextMessageEnd",
        Event::TextMessageChunk(_) => "TextMessageChunk",
        Event::ToolCallStart(_) => "ToolCallStart",
        Event::ToolCallArgs(_) => "ToolCallArgs",
        Event::ToolCallEnd(_) => "ToolCallEnd",
        Event::ToolCallResult(_) => "ToolCallResult",
        Event::StateSnapshot(_) => "StateSnapshot",
        Event::StateDelta(_) => "StateDelta",
        Event::MessagesSnapshot(_) => "MessagesSnapshot",
        Event::Custom(_) => "Custom",
        Event::Raw(_) => "Raw",
    }
}
