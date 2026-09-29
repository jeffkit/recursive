//! Issue #40 — regression tests: `agent(mode="parallel")` must terminate with
//! a FinishReason (as data, not an Err) even when a worker's LLM call stalls,
//! and must be cancellable via the parent shutdown token while preserving
//! tool-call ↔ result pairing (invariants #7 / #8).
//!
//! Root cause fixed: `AgentTool::build_worker_runtime` did not propagate
//! `wall_timeout_secs` / `shutdown_token` into worker runtimes, and
//! `execute_parallel`'s `join_all` had no deadline/abort path — one stalled
//! provider call parked the whole parent turn forever.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use recursive::agent::FinishReason;
use recursive::llm::{ChatProvider, Completion, ToolSpec};
use recursive::message::Message;
use recursive::tools::agent::AgentTool;
use recursive::tools::{LocalTransport, ReadFile, Tool, ToolRegistry};
use serde_json::json;

/// A provider that HANGS: `complete()` never resolves (models the stalled
/// gateway / dead connection observed in the wild).
struct HangingProvider;

#[async_trait]
impl ChatProvider for HangingProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
    ) -> recursive::Result<Completion> {
        tokio::time::sleep(Duration::from_secs(3600)).await;
        unreachable!("hung forever")
    }

    async fn stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _tx: Option<recursive::llm::StreamSender>,
        _cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> recursive::Result<Completion> {
        unreachable!("stream not used")
    }
}

fn tool_registry(workspace: &std::path::Path) -> ToolRegistry {
    ToolRegistry::new(Arc::new(LocalTransport)).register(Arc::new(ReadFile::new(workspace)))
}

fn manifest_for(n: usize) -> serde_json::Map<String, serde_json::Value> {
    let mut manifest = serde_json::Map::new();
    for i in 0..n {
        manifest.insert(
            format!("worker-{i}"),
            json!({
                "system_prompt": "You are a researcher.",
                "allowed_tools": ["Read"]
            }),
        );
    }
    manifest
}

/// N workers × max_steps=45 with a never-resolving provider + a 5s wall
/// budget: `execute` must return `Ok` within 15s, carrying a
/// `WallClockExceeded` label for every dispatched worker. Parameterized over
/// N ∈ {2, 4} (acceptance: N worker × M step coverage).
async fn run_hanging_case(n: usize) {
    let tmp = tempfile::tempdir().unwrap();
    let provider: Arc<dyn ChatProvider> = Arc::new(HangingProvider);
    let all_tools = tool_registry(tmp.path());
    let agent =
        AgentTool::new(tmp.path(), provider, all_tools, 2, 0, None).with_wall_timeout_secs(5);

    let fut = agent.execute(json!({
        "mode": "parallel",
        "manifest": manifest_for(n),
        "prompt": "deeply research this repo",
        "max_steps": 45
    }));

    let result = tokio::time::timeout(Duration::from_secs(15), fut)
        .await
        .expect("parallel agent must terminate via wall budget (issue #40)")
        .expect("execute must return Ok (finish is data, not Err)");

    assert!(
        result.contains("WallClockExceeded"),
        "result must carry WallClockExceeded label, got: {result}"
    );
    for i in 0..n {
        assert!(
            result.contains(&format!("=== worker-{i} ===")),
            "every dispatched worker needs a paired result, got: {result}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_workers_with_hanging_llm_terminate_n2() {
    run_hanging_case(2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_workers_with_hanging_llm_terminate_n4() {
    run_hanging_case(4).await;
}

/// Cancelling the parent token mid-parallel-run: `Ok` + `Cancelled` label and
/// every worker id present (pairing preserved).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_workers_cancel_mid_run_returns_paired_results() {
    let tmp = tempfile::tempdir().unwrap();
    let provider: Arc<dyn ChatProvider> = Arc::new(HangingProvider);
    let all_tools = tool_registry(tmp.path());
    let token = tokio_util::sync::CancellationToken::new();
    let agent = AgentTool::new(tmp.path(), provider, all_tools, 2, 0, None)
        .with_shutdown_token(token.clone());

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        token.cancel();
    });

    let result = tokio::time::timeout(
        Duration::from_secs(15),
        agent.execute(json!({
            "mode": "parallel",
            "manifest": manifest_for(2),
            "prompt": "go",
            "max_steps": 45
        })),
    )
    .await
    .expect("must terminate via cancellation")
    .expect("execute must return Ok");

    assert!(result.contains("Cancelled"), "got: {result}");
    for i in 0..2 {
        assert!(
            result.contains(&format!("=== worker-{i} ===")),
            "got: {result}"
        );
    }
}

/// Token AND wall budget both present (production CLI/HTTP-serve combo): a
/// stalled run hitting the aggregate wall deadline must be labeled
/// `WallClockExceeded`, NOT `Cancelled` — `cancel()` is called on the child
/// token after the deadline fires, and the label must not be derived from the
/// post-hoc `is_cancelled()` state (regression for the issue-40 review fix).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_workers_token_present_wall_deadline_labels_wall_clock_exceeded() {
    let tmp = tempfile::tempdir().unwrap();
    let provider: Arc<dyn ChatProvider> = Arc::new(HangingProvider);
    let all_tools = tool_registry(tmp.path());
    // Token exists and is NEVER cancelled — only the wall budget fires.
    let token = tokio_util::sync::CancellationToken::new();
    let agent = AgentTool::new(tmp.path(), provider, all_tools, 2, 0, None)
        .with_shutdown_token(token.clone())
        .with_wall_timeout_secs(5);

    let result = tokio::time::timeout(
        Duration::from_secs(15),
        agent.execute(json!({
            "mode": "parallel",
            "manifest": manifest_for(2),
            "prompt": "go",
            "max_steps": 45
        })),
    )
    .await
    .expect("must terminate via wall budget even with token present")
    .expect("execute must return Ok");

    assert!(
        result.contains("WallClockExceeded"),
        "wall-deadline hit with an (uncancelled) shutdown token must be labeled WallClockExceeded, got: {result}"
    );
    assert!(
        !result.contains("Cancelled"),
        "must not be mislabeled Cancelled when the token was never cancelled, got: {result}"
    );
    for i in 0..2 {
        assert!(
            result.contains(&format!("=== worker-{i} ===")),
            "got: {result}"
        );
    }
}

/// Healthy path: with a well-behaved provider, parallel workers × many steps
/// complete within 30s (guards against the fix breaking the normal path).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_workers_healthy_provider_completes() {
    use recursive::llm::MockProvider;

    // Each worker: 44 tool-call steps + 1 final text = 45 completions.
    let mut script = Vec::new();
    for step in 0..44 {
        script.push(Completion {
            content: String::new(),
            tool_calls: vec![recursive::llm::ToolCall {
                id: format!("c-{step}"),
                name: "Read".into(),
                arguments: json!({ "path": "Cargo.toml" }),
            }],
            finish_reason: None,
            usage: None,
            reasoning_content: None,
        });
    }
    script.push(Completion {
        content: "done".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    });

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("Cargo.toml"), "[package]\n").unwrap();
    let provider = Arc::new(MockProvider::new(script));
    let all_tools = tool_registry(tmp.path());
    let agent =
        AgentTool::new(tmp.path(), provider, all_tools, 2, 0, None).with_wall_timeout_secs(30);

    let result = tokio::time::timeout(
        Duration::from_secs(30),
        agent.execute(json!({
            "mode": "parallel",
            "manifest": manifest_for(2),
            "prompt": "read files",
            "max_steps": 45
        })),
    )
    .await
    .expect("healthy parallel run must not hang")
    .expect("execute ok");

    assert!(result.contains("NoMoreToolCalls"), "got: {result}");
    let _ = FinishReason::NoMoreToolCalls; // symbol anchor
}

/// Slot-path combo (production TUI wiring): a per-turn `SharedTokenSlot`
/// (no static token) whose token is cancelled mid-run must terminate with
/// Ok + `Cancelled` placeholders for every worker — the same behaviour as
/// the static-token path, exercised through the new slot resolution.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_workers_static_token_and_slot_both_resolve() {
    let tmp = tempfile::tempdir().unwrap();
    let provider: Arc<dyn ChatProvider> = Arc::new(HangingProvider);
    let all_tools = tool_registry(tmp.path());
    let token = tokio_util::sync::CancellationToken::new();
    let slot: recursive::SharedTokenSlot = Arc::new(std::sync::Mutex::new(Some(token.clone())));
    let agent =
        AgentTool::new(tmp.path(), provider, all_tools, 2, 0, None).with_shutdown_token_slot(slot);

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        token.cancel();
    });

    let result = tokio::time::timeout(
        Duration::from_secs(15),
        agent.execute(json!({
            "mode": "parallel",
            "manifest": manifest_for(2),
            "prompt": "go",
            "max_steps": 45
        })),
    )
    .await
    .expect("must terminate via slot-token cancellation")
    .expect("execute must return Ok");

    assert!(result.contains("Cancelled"), "got: {result}");
    for i in 0..2 {
        assert!(
            result.contains(&format!("=== worker-{i} ===")),
            "got: {result}"
        );
    }
}
