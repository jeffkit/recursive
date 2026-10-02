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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use agui_client::{AguiClient, ClientError, Event, RunAgentInput};
use agui_protocol::Message;
use recursive::config::Config;
use recursive::http::{
    build_router_with_auth_and_rate_limit, AppState, AuthConfig, Metrics, RateLimiter,
};
use recursive::llm::{ChatProvider, Completion, MockProvider, TokenUsage, ToolCall};
use recursive::session::{SessionReader, SessionStatus};
use recursive::tools::ToolRegistry;
use tokio::net::TcpListener;
use tokio::sync::{Notify, RwLock};

// Goal 396: reuse the shared HTTP fixtures (in-memory storage backend) so
// AppState keeps compiling without touching the real filesystem.
#[path = "http_common/mod.rs"]
mod common;

/// Process-wide lock for tests that mutate `RECURSIVE_HOME`.
static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Per-test redirect for `RECURSIVE_HOME` so tests don't write into
/// the developer's real `~/.recursive` and don't race each other.
///
/// `RECURSIVE_SESSIONS_DIR` (Goal-H J1) is a hard override that beats
/// `RECURSIVE_HOME`; a value inherited from the surrounding environment
/// (self-improve pipelines, e2e harnesses) would redirect every
/// `persist_run` into one shared root, making the session-count and
/// transcript-shape assertions below see unrelated sessions. It is
/// therefore pinned to `<home>/sessions` together with `RECURSIVE_HOME`.
struct HomeOverride {
    prev: Option<std::ffi::OsString>,
    prev_sessions: Option<std::ffi::OsString>,
    _home: tempfile::TempDir,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl HomeOverride {
    fn new() -> Self {
        let lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("RECURSIVE_HOME");
        let prev_sessions = std::env::var_os("RECURSIVE_SESSIONS_DIR");
        let dir = tempfile::tempdir().expect("tempdir");
        // SAFETY: process-global env mutation; protected by HOME_LOCK.
        unsafe {
            std::env::set_var("RECURSIVE_HOME", dir.path());
            std::env::set_var("RECURSIVE_SESSIONS_DIR", dir.path().join("sessions"));
        }
        Self {
            prev,
            prev_sessions,
            _home: dir,
            _lock: lock,
        }
    }
}

impl Drop for HomeOverride {
    fn drop(&mut self) {
        // SAFETY: still under HOME_LOCK held by `_lock`.
        unsafe {
            match self.prev.take() {
                Some(v) => std::env::set_var("RECURSIVE_HOME", v),
                None => std::env::remove_var("RECURSIVE_HOME"),
            }
            match self.prev_sessions.take() {
                Some(v) => std::env::set_var("RECURSIVE_SESSIONS_DIR", v),
                None => std::env::remove_var("RECURSIVE_SESSIONS_DIR"),
            }
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
        max_tokens: 65536,
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

fn state(workspace: PathBuf, provider: Arc<dyn ChatProvider>) -> AppState {
    AppState {
        tools: vec![],
        config: mock_config(workspace),
        tool_registry: ToolRegistry::local(),
        provider,
        event_channels: Arc::new(RwLock::new(HashMap::new())),
        metrics: Arc::new(Metrics::default()),
        slash_commands: Arc::new(Vec::new()),
        host: std::sync::Arc::new(recursive::session_host::SessionHost::new(
            std::time::Duration::from_secs(0),
            recursive::http::AdmissionGate::new(
                8,
                std::time::Duration::ZERO,
                std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            ),
        )),
        rate_limiter: RateLimiter::new(10, 1.0),
        skills: vec![],
        storage: Arc::new(recursive::storage::LocalStorageBackend::new(
            std::env::temp_dir().join(format!("recursive-agui-test-{}", std::process::id())),
        )),
        agui_active_runs: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
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
        interrupt_before: None,
        forwarded_props: None,
        system_prompt: None,
        append_system_prompt: None,
    }
}

/// Bind to 127.0.0.1:0, spawn the server, return its base URL.
async fn spawn_server(workspace: PathBuf, provider: Arc<dyn ChatProvider>) -> url::Url {
    // Since Goal 277, the HTTP server refuses requests when auth is
    // not configured unless INSECURE_OK=1 is set. These e2e tests
    // don't need auth — they talk to loopback.
    std::env::set_var("RECURSIVE_HTTP_AUTH_INSECURE_OK", "1");

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

/// Goal 284: with on-demand checkpoints, no `checkpoint_post` event
/// is emitted automatically. Verify that the stream completes without
/// it (no panic, RunFinished arrives cleanly).
#[tokio::test]
async fn agui_endpoint_no_checkpoint_post_without_agent_save() {
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
    // Goal 284: no checkpoint_post without agent calling checkpoint_save.
    let has_cp_post = events.iter().any(|e| match e {
        Event::Custom(c) => c.name == "agui-tui/checkpoint_post",
        _ => false,
    });
    assert!(
        !has_cp_post,
        "checkpoint_post should NOT be emitted automatically (Goal 284); got {names:?}"
    );

    // RunFinished should still arrive.
    assert!(
        names.contains(&"RunFinished"),
        "RunFinished must still be emitted; got {names:?}"
    );
}

/// Goal 284: with on-demand checkpoints, no `checkpoint_post` events
/// are emitted automatically. Verify that multiple runs still complete
/// cleanly with RunStarted / RunFinished.
#[tokio::test]
async fn agui_endpoint_multiple_runs_no_checkpoint_post() {
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

    let run1 = run_and_collect(
        &client,
        input_with("multi", "multi-0", vec![user_msg("u1", "first")]),
    )
    .await;
    let run2 = run_and_collect(
        &client,
        input_with("multi", "multi-1", vec![user_msg("u2", "second")]),
    )
    .await;

    // Both runs should complete with RunFinished and no errors.
    for (i, events) in [&run1, &run2].iter().enumerate() {
        let names: Vec<&str> = events.iter().map(event_name).collect();
        assert!(
            names.contains(&"RunFinished"),
            "run {i} missing RunFinished in {names:?}"
        );
        let has_cp = events.iter().any(|e| match e {
            Event::Custom(c) => c.name == "agui-tui/checkpoint_post",
            _ => false,
        });
        assert!(
            !has_cp,
            "run {i} should NOT have checkpoint_post (Goal 284)"
        );
    }
}

/// Issue #68 acceptance: two threads on the same process, each sending a
/// different per-request `system_prompt`, must each see their own prompt
/// in the provider request (per-request isolation, no cross-talk).
#[tokio::test]
async fn agui_per_request_system_prompt_isolated_per_thread() {
    let _home = HomeOverride::new();
    let workspace = tempfile::tempdir().expect("ws");

    let provider = Arc::new(MockProvider::new(vec![
        Completion {
            content: "alpha ack".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
        Completion {
            content: "beta ack".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
    ]));

    let endpoint = spawn_server(workspace.path().to_path_buf(), provider.clone()).await;
    let client = AguiClient::new(endpoint);

    let mut alpha = input_with("tenant-alpha", "a-0", vec![user_msg("u1", "hi")]);
    alpha.system_prompt = Some("ALPHA PROMPT".into());
    let mut beta = input_with("tenant-beta", "b-0", vec![user_msg("u1", "hi")]);
    // forwardedProps alias carries the beta prompt.
    beta.forwarded_props = Some(serde_json::json!({"systemPrompt": "BETA PROMPT"}));

    for input in [alpha, beta] {
        let mut rx = client.run(input).await.expect("run");
        while rx.recv().await.is_some() {}
    }

    let calls = provider.calls();
    assert_eq!(calls.len(), 2, "expected two provider calls");
    let sys_of = |call: &[recursive::message::Message]| {
        call.iter()
            .find(|m| m.role == recursive::message::Role::System)
            .map(|m| m.content.clone())
            .expect("system message present")
    };
    let (sys_a, sys_b) = (sys_of(&calls[0]), sys_of(&calls[1]));
    assert!(
        sys_a.contains("ALPHA PROMPT") && !sys_a.contains("BETA PROMPT"),
        "thread alpha must see only its own prompt; got: {sys_a}"
    );
    assert!(
        sys_b.contains("BETA PROMPT") && !sys_b.contains("ALPHA PROMPT"),
        "thread beta must see only its own prompt; got: {sys_b}"
    );
    // Server-owned default prompt text from the fixture ("You are a test
    // assistant.") must NOT leak into either override.
    assert!(
        !sys_a.contains("You are a test assistant.")
            && !sys_b.contains("You are a test assistant."),
        "per-request prompt replaces the process-level one; got: {sys_a} / {sys_b}"
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

// ── Issue #57: an AG-UI thread IS a session ─────────────────────────────────

#[tokio::test]
async fn agui_run_persists_a_listable_native_session() {
    let _home = HomeOverride::new();
    let workspace = tempfile::tempdir().expect("ws");

    let provider = Arc::new(MockProvider::new(vec![Completion {
        content: "hi from recursive".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: Some(TokenUsage {
            prompt_tokens: 100,
            completion_tokens: 20,
            total_tokens: 120,
            cache_hit_tokens: 0,
            cache_miss_tokens: 100,
            reasoning_tokens: 0,
        }),
        reasoning_content: None,
    }]));
    let endpoint = spawn_server(workspace.path().to_path_buf(), provider).await;
    let client = AguiClient::new(endpoint);

    let input = input_with("vis-thread", "vis-run-0", vec![user_msg("u1", "say hi")]);
    let mut rx = client.run(input).await.expect("run");
    // persist_run happens BEFORE RunFinished is emitted, so seeing
    // RunFinished guarantees the session is on disk.
    while let Some(ev) = rx.recv().await {
        if matches!(ev, Event::RunFinished(_)) {
            break;
        }
    }

    // 1. The consumer chain from the issue report: SessionReader::list_sessions
    //    (sessions list, episodic_recall, resume picker) must see the thread.
    //    Match the exact thread key: a bare `agui-` prefix would happily
    //    pick up a sibling thread's session slug sharing the store.
    let listed = SessionReader::list_sessions(workspace.path()).expect("list sessions");
    let dir = listed
        .iter()
        .find(|p| {
            p.file_name()
                .map(|n| {
                    n.to_string_lossy() == recursive::agui_session::thread_session_key("vis-thread")
                })
                .unwrap_or(false)
        })
        .expect("AG-UI thread must be visible to SessionReader::list_sessions")
        .clone();

    // 2. `.meta.json` exists and is fully populated.
    let meta = SessionReader::load_meta(&dir).expect("load meta");
    assert_eq!(meta.status, SessionStatus::Completed);
    assert_eq!(meta.message_count, 2, "user + assistant");
    assert_eq!(meta.first_prompt.as_deref(), Some("say hi"));
    assert_eq!(meta.last_prompt.as_deref(), Some("say hi"));

    // 3. Cost lands in meta (tokens) + cost.json (tracker block).
    let cost = meta.cost.expect("run usage must reach .meta.json cost");
    assert_eq!(cost.total_input_tokens, 100);
    assert_eq!(cost.total_output_tokens, 20);
    let cost_json: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.join("cost.json")).expect("cost.json must exist"),
    )
    .expect("cost.json parses");
    assert_eq!(cost_json["total_usage"]["total_tokens"], 120);

    // 4. The transcript is native-format — readable by the exact tool
    //    (`episodic_recall` → load_transcript) that used to see nothing.
    let entries = SessionReader::load_transcript(&dir).expect("load transcript");
    assert!(
        entries
            .iter()
            .any(|e| e.role == "assistant" && e.content == "hi from recursive"),
        "assistant reply must be retrievable via load_transcript"
    );
}

/// Provider that holds the first `complete()` call open until released —
/// lets the test hold run #1 in flight while probing the fence.
struct BarrierProvider {
    entered: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
    notified: Arc<Notify>,
}

#[async_trait::async_trait]
impl ChatProvider for BarrierProvider {
    async fn complete(
        &self,
        _messages: &[recursive::message::Message],
        _tools: &[recursive::llm::ToolSpec],
    ) -> recursive::error::Result<Completion> {
        self.entered.store(true, Ordering::SeqCst);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while !self.release.load(Ordering::SeqCst) {
            if tokio::time::Instant::now() >= deadline {
                break; // fail the test below by finishing late, not hanging forever
            }
            let _ = tokio::time::timeout(
                std::time::Duration::from_millis(50),
                self.notified.notified(),
            )
            .await;
        }
        Ok(Completion {
            content: "released".into(),
            finish_reason: Some("stop".into()),
            ..Default::default()
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn agui_run_rejects_a_second_concurrent_run_for_the_same_thread() {
    let _home = HomeOverride::new();
    let workspace = tempfile::tempdir().expect("ws");

    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let notify = Arc::new(Notify::new());
    let provider = Arc::new(BarrierProvider {
        entered: entered.clone(),
        release: release.clone(),
        notified: notify.clone(),
    });
    let endpoint = spawn_server(workspace.path().to_path_buf(), provider).await;
    let client = AguiClient::new(endpoint);

    // Run #1 for the thread: parks inside the provider.
    let mut rx1 = client
        .run(input_with(
            "fence-thread",
            "run-1",
            vec![user_msg("u1", "slow please")],
        ))
        .await
        .expect("first run starts");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !entered.load(Ordering::SeqCst) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "run #1 never reached the provider"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // A second run for the SAME thread while #1 is in flight → 409 Conflict,
    // not a queued duplicate and not a silent transcript race.
    let second = client
        .run(input_with(
            "fence-thread",
            "run-2",
            vec![user_msg("u1", "duplicate")],
        ))
        .await;
    match second {
        Err(ClientError::HttpStatus { status, body }) => {
            assert_eq!(status, 409, "expected 409, got {status}: {body}");
        }
        Err(other) => panic!("expected HTTP 409, got error: {other}"),
        Ok(_) => panic!("second concurrent run for the same thread must be refused"),
    }

    // Let run #1 finish and drain its stream to RunFinished (guard drops
    // with the driver task).
    release.store(true, Ordering::SeqCst);
    notify.notify_waiters();
    while let Some(ev) = rx1.recv().await {
        if matches!(ev, Event::RunFinished(_)) {
            break;
        }
    }

    // The fence re-opens: the same thread runs again...
    let mut rx_again = client
        .run(input_with(
            "fence-thread",
            "run-3",
            vec![user_msg("u1", "again")],
        ))
        .await
        .expect("same thread must run again once the previous run finished");
    while let Some(ev) = rx_again.recv().await {
        if matches!(ev, Event::RunFinished(_)) {
            break;
        }
    }
    // ...and the transcript accumulated all three runs (append, not overwrite).
    let listed = SessionReader::list_sessions(workspace.path()).expect("list sessions");
    let dir = listed
        .iter()
        .find(|p| {
            p.file_name()
                .map(|n| {
                    n.to_string_lossy()
                        == recursive::agui_session::thread_session_key("fence-thread")
                })
                .unwrap_or(false)
        })
        .expect("fence-thread session dir");
    let entries = SessionReader::load_transcript(dir).expect("transcript");
    let user_texts: Vec<&str> = entries
        .iter()
        .filter(|e| e.role == "user")
        .map(|e| e.content.as_str())
        .collect();
    assert_eq!(
        user_texts,
        vec!["slow please", "again"],
        "second run must be appended, first-run history preserved; got {user_texts:?}"
    );
}

#[tokio::test]
async fn agui_interrupt_resume_round_trips_through_the_native_session() {
    let _home = HomeOverride::new();
    let workspace = tempfile::tempdir().expect("ws");

    // Run 1 makes the model call a client tool; run 2 (resume) completes.
    // Completion 2 answers the post-deny turn (the interrupt is detected
    // after the loop stops), completion 3 answers the resumed turn.
    let provider = Arc::new(MockProvider::new(vec![
        Completion {
            content: "checking weather".into(),
            tool_calls: vec![ToolCall {
                id: "t1".into(),
                name: "get_weather".into(),
                arguments: serde_json::json!({"city": "SF"}),
            }],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
        Completion {
            content: "waiting for the weather service".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
        Completion {
            content: "It is sunny.".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
    ]));
    let endpoint = spawn_server(workspace.path().to_path_buf(), provider).await;
    let client = AguiClient::new(endpoint);

    let input = RunAgentInput {
        thread_id: "round-trip".into(),
        run_id: "rt-run-0".into(),
        messages: vec![user_msg("u1", "weather in SF?")],
        tools: vec![agui_protocol::Tool {
            name: "get_weather".into(),
            description: "client-side weather lookup".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }],
        context: vec![],
        resume: None,
        state: None,
        interrupt_before: None,
        forwarded_props: None,
        system_prompt: None,
        append_system_prompt: None,
    };

    let mut rx = client.run(input).await.expect("run 1");
    let mut interrupt_id = String::new();
    while let Some(ev) = rx.recv().await {
        if let Event::RunFinished(f) = ev {
            match f.outcome {
                Some(agui_protocol::RunFinishedOutcome::Interrupt { interrupts }) => {
                    interrupt_id = interrupts[0].id.clone();
                }
                other => panic!("run 1 must end in Interrupt, got {other:?}"),
            }
            break;
        }
    }
    assert!(!interrupt_id.is_empty(), "interrupt must carry an id");

    // The interrupted run is persisted as a session with status
    // Interrupted and the open interrupt next to the transcript.
    // (Exact-key match so a sibling thread's slug can't satisfy the find.)
    let listed = SessionReader::list_sessions(workspace.path()).expect("list sessions");
    let dir = listed
        .iter()
        .find(|p| {
            p.file_name()
                .map(|n| {
                    n.to_string_lossy() == recursive::agui_session::thread_session_key("round-trip")
                })
                .unwrap_or(false)
        })
        .expect("round-trip session dir")
        .clone();
    let meta = SessionReader::load_meta(&dir).expect("meta after run 1");
    assert_eq!(
        meta.status,
        SessionStatus::Interrupted,
        "run 1 must persist as Interrupted"
    );
    let interrupts: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join(".interrupts.json")).expect("interrupts"))
            .expect("interrupts parse");
    assert_eq!(interrupts[0]["interrupt_id"], interrupt_id.as_str());

    // Run 2: resume with the client-tool result.
    let resume_input = RunAgentInput {
        thread_id: "round-trip".into(),
        run_id: "rt-run-1".into(),
        messages: vec![],
        tools: vec![],
        context: vec![],
        resume: Some(vec![agui_protocol::Resume {
            interrupt_id: interrupt_id.clone(),
            status: agui_protocol::ResumeStatus::Resolved,
            payload: Some(serde_json::json!({"forecast": "sunny"})),
        }]),
        state: None,
        interrupt_before: None,
        forwarded_props: None,
        system_prompt: None,
        append_system_prompt: None,
    };

    let mut rx2 = client.run(resume_input).await.expect("resume run");
    while let Some(ev) = rx2.recv().await {
        if let Event::RunFinished(f) = ev {
            assert!(
                matches!(f.outcome, Some(agui_protocol::RunFinishedOutcome::Success)),
                "resume must end in Success, got {:?}",
                f.outcome
            );
            break;
        }
    }

    // The native session now carries BOTH runs with the tool pairing
    // intact across the persist ↔ resume boundary (invariant #8).
    let entries = SessionReader::load_transcript(&dir).expect("transcript");
    let roles: Vec<&str> = entries.iter().map(|e| e.role.as_str()).collect();
    assert_eq!(
        roles,
        vec![
            "user",
            "assistant",
            "tool",
            "assistant",
            "user",
            "assistant"
        ],
        "both runs must be appended; got {roles:?}"
    );
    assert_eq!(entries[1].tool_calls[0].id, "t1");
    assert_eq!(entries[2].tool_call_id.as_deref(), Some("t1"));
    // The resume payload replaced the deny marker ON DISK — a later resume
    // of this thread re-seeds the real client-tool result, not the deny text.
    assert!(
        !entries[2].content.contains("[frontend tool]"),
        "deny marker must not survive on disk after resume; got {}",
        entries[2].content
    );
    assert!(
        entries[2].content.contains("sunny"),
        "disk must carry the resume payload; got {}",
        entries[2].content
    );
    assert_eq!(entries[5].content, "It is sunny.");

    // Meta reflects the finished run; interrupt marker cleared.
    let meta2 = SessionReader::load_meta(&dir).expect("meta after resume");
    assert_eq!(meta2.status, SessionStatus::Completed);
    assert!(
        !dir.join(".interrupts.json").exists(),
        "interrupt marker must be cleared after resume"
    );
}
