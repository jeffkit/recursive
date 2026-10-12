//! Integration tests for the MCP client against a Rust mock server.
//!
//! These tests live in `tests/` (rather than as `#[cfg(test)] mod tests`
//! inside `src/mcp.rs`) so they can resolve the path of the mock server
//! binary via the cargo-set `CARGO_BIN_EXE_mock_mcp_server` env var.
//! That variable is only injected for integration tests, not unit tests.
//!
//! See `tests/bin/mock_mcp_server.rs` for the mock implementation and
//! its supported behaviour modes.

#![cfg(feature = "mcp")]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use recursive::error::{Error, Result};
use recursive::mcp::{McpClient, McpServer, McpTool, McpToolSpec};

/// Read timeout for tests. Has to cover:
/// 1. The genuine "server is hung" case (`timeout` mock mode) — we want
///    this to give up well before the 10s production default.
/// 2. The cold-start cost of spawning the mock binary in parallel with
///    16 other tests on a busy CI box. Empirically 500ms is too tight
///    on macOS when many test harness threads compete; 2s is a safe
///    middle ground that still cuts the timeout test from ~10s to ~2s.
const TEST_READ_TIMEOUT: Duration = Duration::from_secs(2);

/// Cold-start retries for the mock binary (modes that must succeed).
///
/// Under heavy parallel load — e.g. cargo-mutants `--jobs N` compiling
/// and running test binaries concurrently (2026-08-03: 14 jobs crashed the
/// mcp_e2e baseline with "server timed out (no response within 2s)") — a
/// single spawn+handshake can exceed TEST_READ_TIMEOUT transiently even
/// though the server is healthy. Retrying separates that from a genuinely
/// hung server. The `timeout` mode is the one case that must NOT retry:
/// its whole purpose is to exercise the read-timeout path and fail fast.
const MOCK_SPAWN_RETRIES: u32 = 3;

/// Spawn the pure-Rust mock MCP server binary in the given behavior mode.
async fn spawn_mock(mode: &str) -> Result<McpClient> {
    spawn_mock_bounded(mode, true).await
}

/// `short_tool_timeout = true` keeps the historical behaviour of every
/// pre-#181 test: the short read timeout is the only timeout, so it also
/// governs `tools/call`. `false` leaves the *production default* tool
/// timeout in place (`RECURSIVE_MCP_TOOL_TIMEOUT_SECS` is not set in the
/// test environment), which is what a plainly-spawned client gets.
async fn spawn_mock_bounded(mode: &str, short_tool_timeout: bool) -> Result<McpClient> {
    let tool_timeout = if short_tool_timeout {
        TEST_READ_TIMEOUT
    } else {
        recursive::mcp::DEFAULT_MCP_TOOL_TIMEOUT
    };
    spawn_mock_with(mode, TEST_READ_TIMEOUT, tool_timeout).await
}

/// Spawn the mock server with independent control over the stdio read
/// timeout (`initialize` / `tools/list`) and the per-`tools/call` timeout.
async fn spawn_mock_with(
    mode: &str,
    read_timeout: Duration,
    tool_timeout: Duration,
) -> Result<McpClient> {
    let server = McpServer {
        name: "mock".to_string(),
        command: env!("CARGO_BIN_EXE_mock_mcp_server").to_string(),
        args: vec![mode.to_string()],
        url: None,
        env: None,
        headers: None,
        transport: None,
    };
    let attempts = if mode == "timeout" {
        1
    } else {
        MOCK_SPAWN_RETRIES
    };
    let mut last_err = None;
    for attempt in 0..attempts {
        match McpClient::spawn_with_timeout(&server, read_timeout).await {
            Ok(client) => return Ok(client.with_tool_timeout(tool_timeout)),
            Err(e) => {
                last_err = Some(e);
                // Back off between retries: cold start under load is
                // transient, but give the scheduler a moment to settle.
                tokio::time::sleep(Duration::from_millis(200 * (attempt as u64 + 1))).await;
            }
        }
    }
    Err(last_err.expect("at least one spawn attempt"))
}

/// Start a mock in one of the server-initiated-request modes, attach a
/// real [`McpClient`], and return the mock's verdict marker file.
///
/// The mock validates the client's reply and appends `SERVER_PING_OK` /
/// `SERVER_UNKNOWN_OK` (or the `_BAD` variants) to `marker-file`; the
/// client's very first read — the `initialize` handshake — is the one
/// that has to deal with the probe.
///
/// Returns the live client, the tempdir, and the marker path. The tempdir
/// has to outlive the *mock's* write (it creates the marker lazily), and the
/// client is handed back so the caller keeps the mock alive until it has
/// read the verdict — dropping the client kills the child.
async fn spawn_probe_and_marker(mode: &str) -> (McpClient, tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let marker = dir.path().join("marker");
    let server = McpServer {
        name: "mock".to_string(),
        command: env!("CARGO_BIN_EXE_mock_mcp_server").to_string(),
        args: vec![
            mode.to_string(),
            "--marker-file".to_string(),
            marker.to_string_lossy().into_owned(),
        ],
        url: None,
        env: None,
        headers: None,
        transport: None,
    };
    let client = McpClient::spawn_with_timeout(&server, TEST_READ_TIMEOUT)
        .await
        .expect("client must complete the handshake despite the server probe");
    (client, dir, marker)
}

/// Wait until `path` contains `needle` (or ~10s elapses) and return its
/// contents either way, so the caller can assert on what was seen.
///
/// Callers must pass a full verdict marker (`SERVER_PING_OK`), never a
/// prefix that could match an earlier line — polling on a prefix returns
/// before the verdict is written and silently turns the assertion into a
/// no-op (this bit an earlier revision of these tests).
fn probe_marker(path: &std::path::Path, needle: &str) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let seen = std::fs::read_to_string(path).unwrap_or_default();
        if seen.contains(needle) || std::time::Instant::now() >= deadline {
            return seen;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

#[tokio::test]
async fn initialize_handshake_and_list_tools() {
    let mut client = spawn_mock("echo").await.expect("spawn mock server");

    let tools = client.list_tools().await.expect("list_tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
    assert!(tools[0].description.contains("Echo"));
}

#[tokio::test]
async fn call_tool_returns_text() {
    let mut client = spawn_mock("echo").await.expect("spawn mock server");

    let result = client
        .call_tool("echo", serde_json::json!({"message": "hello"}))
        .await
        .expect("call_tool");
    assert!(result.contains("Echo: hello"));
}

#[tokio::test]
async fn malformed_server_errors_cleanly() {
    let result = spawn_mock("malformed").await;
    assert!(result.is_err(), "should fail on non-JSON response");
}

#[tokio::test]
async fn mcp_tool_roundtrip() {
    let client = spawn_mock("echo").await.expect("spawn mock server");
    let client = Arc::new(Mutex::new(client));

    let spec = McpToolSpec {
        name: "echo".to_string(),
        description: "Echo back input".to_string(),
        input_schema: serde_json::json!({"type":"object","properties":{"message":{"type":"string"}}}),
        annotations: None,
    };

    // McpTool::new + spec/execute are exercised through the public API.
    use recursive::tools::Tool;
    let tool = McpTool::new(client, spec, "mock");
    let tool_spec = tool.spec();
    assert_eq!(tool_spec.name, "mcp__mock__echo");
    assert!(tool_spec.description.contains("[mcp:mock]"));

    let result = tool
        .execute(serde_json::json!({"message": "hello"}))
        .await
        .expect("tool execute");
    assert!(result.contains("Echo: hello"));
}

#[tokio::test]
async fn server_timeout_errors_cleanly() {
    let result = spawn_mock("timeout").await;
    assert!(result.is_err(), "should fail on timeout");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("timed out") || err.contains("timeout"),
        "error should mention timeout: {err}"
    );
}

#[tokio::test]
async fn call_tool_with_error_response() {
    let mut client = spawn_mock("error-tool").await.expect("spawn mock server");
    let err = client
        .call_tool("failing", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Tool { .. }));
    let msg = err.to_string();
    assert!(msg.contains("Something went wrong"), "error: {msg}");
}

// ---------------------------------------------------------------------------
// Resources
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resources_list_resources() {
    let mut client = spawn_mock("echo").await.expect("spawn mock server");

    let resources = client.list_resources().await.expect("list_resources");
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].uri, "file:///tmp/test.txt");
    assert_eq!(resources[0].name, "Test File");
    assert_eq!(resources[0].description.as_deref(), Some("A test file"));
    assert_eq!(resources[0].mime_type.as_deref(), Some("text/plain"));
}

#[tokio::test]
async fn resources_read_resource() {
    let mut client = spawn_mock("echo").await.expect("spawn mock server");

    let contents = client
        .read_resource("file:///tmp/test.txt")
        .await
        .expect("read_resource");
    assert_eq!(contents.len(), 1);
    assert_eq!(contents[0].uri, "file:///tmp/test.txt");
    assert_eq!(contents[0].text.as_deref(), Some("Hello, world!"));
    assert_eq!(contents[0].mime_type.as_deref(), Some("text/plain"));
}

#[tokio::test]
async fn resources_read_resource_with_blob() {
    let mut client = spawn_mock("read-blob").await.expect("spawn mock server");

    let contents = client
        .read_resource("file:///tmp/image.png")
        .await
        .expect("read_resource");
    assert_eq!(contents.len(), 1);
    assert_eq!(contents[0].uri, "file:///tmp/image.png");
    assert_eq!(contents[0].mime_type.as_deref(), Some("image/png"));
    assert!(contents[0].blob.is_some());
    assert!(contents[0].text.is_none());
}

// ---------------------------------------------------------------------------
// Prompts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn prompts_list_prompts() {
    let mut client = spawn_mock("echo").await.expect("spawn mock server");

    let prompts = client.list_prompts().await.expect("list_prompts");
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].name, "greet");
    assert_eq!(prompts[0].description.as_deref(), Some("Greet someone"));
    let args = prompts[0]
        .arguments
        .as_ref()
        .expect("arguments should be present");
    assert_eq!(args.len(), 1);
    assert_eq!(args[0].name, "name");
    assert!(args[0].required);
}

#[tokio::test]
async fn prompts_get_prompt() {
    let mut client = spawn_mock("echo").await.expect("spawn mock server");

    let messages = client.get_prompt("greet", None).await.expect("get_prompt");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[0].content, "Hello, world!");
}

#[tokio::test]
async fn prompts_get_prompt_with_arguments() {
    let mut client = spawn_mock("prompt-args").await.expect("spawn mock server");

    let mut args = HashMap::new();
    args.insert("name".to_string(), "Alice".to_string());
    let messages = client
        .get_prompt("greet", Some(args))
        .await
        .expect("get_prompt");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[0].content, "Hello, Alice!");
}

// ---------------------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resources_capability_not_advertised() {
    let mut client = spawn_mock("tools-only").await.expect("spawn mock server");

    let err = client.list_resources().await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("does not advertise"),
        "expected capability error, got: {msg}"
    );
}

#[tokio::test]
async fn prompts_get_with_missing_name() {
    let mut client = spawn_mock("prompt-default")
        .await
        .expect("spawn mock server");

    let messages = client
        .get_prompt("", None)
        .await
        .expect("get_prompt with empty name");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role, "assistant");
    assert_eq!(messages[0].content, "default prompt");
}

#[tokio::test]
async fn resources_read_empty_content() {
    let mut client = spawn_mock("empty-resources")
        .await
        .expect("spawn mock server");

    let contents = client
        .read_resource("file:///tmp/nonexistent.txt")
        .await
        .expect("read_resource with empty contents");
    assert!(
        contents.is_empty(),
        "expected empty contents vec, got {} items",
        contents.len()
    );
}

#[tokio::test]
async fn capability_not_advertised_returns_error_for_resources() {
    let mut client = spawn_mock("tools-only").await.expect("spawn mock server");

    let err = client.list_resources().await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("does not advertise"),
        "error should mention capability: {msg}"
    );

    let err = client
        .read_resource("file:///tmp/test.txt")
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("does not advertise"),
        "error should mention capability: {msg}"
    );
}

#[tokio::test]
async fn capability_not_advertised_returns_error_for_prompts() {
    let mut client = spawn_mock("tools-only").await.expect("spawn mock server");

    let err = client.list_prompts().await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("does not advertise"),
        "error should mention capability: {msg}"
    );

    let err = client.get_prompt("greet", None).await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("does not advertise"),
        "error should mention capability: {msg}"
    );
}

// ---------------------------------------------------------------------------
// issue #181: stdio lifecycle — per-call timeout, server-initiated traffic,
// capability shape, non-JSON stdout lines
// ---------------------------------------------------------------------------

/// Long-running `tools/call` must be governed by the *tool* timeout, not by
/// the short read timeout used for the handshake and `tools/list`.
///
/// The mock sleeps 12s — longer than both the pre-#181 hard-coded 10s
/// default and the deliberately tight 200ms read timeout — so this test
/// can only pass if `call_tool` switched to the configured tool timeout.
///
/// ~12s wall clock, so it is `#[ignore]`d by default; run it explicitly:
/// `cargo test --test mcp_e2e -- --ignored tool_call_respects_configured_timeout`
#[tokio::test]
#[ignore]
async fn tool_call_respects_configured_timeout() {
    let started = std::time::Instant::now();
    let mut client = spawn_mock_with(
        "delay-call",
        Duration::from_millis(200),
        Duration::from_secs(30),
    )
    .await
    .expect("spawn mock server");

    let out = client
        .call_tool("slow", serde_json::json!({}))
        .await
        .expect("a 12s tools/call must survive when the tool timeout is 30s");
    assert!(out.contains("Slow: done"), "unexpected tool output: {out}");
    assert!(
        started.elapsed() >= Duration::from_secs(12),
        "the mock must actually have delayed the call; elapsed {:?}",
        started.elapsed()
    );
}

/// The same mock call, but with no tool-timeout override: it must still be
/// governed by the *default* tool timeout (300s per the #181 design), so a
/// 12s call succeeds on a plainly-spawned client.
#[tokio::test]
#[ignore]
async fn tool_call_default_timeout_exceeds_ten_seconds() {
    // `false`: no tool-timeout override, so the production default governs
    // the call. The mock still needs a short *read* timeout to keep the
    // handshake fast.
    let mut client = spawn_mock_bounded("delay-call", false)
        .await
        .expect("spawn mock server");

    let out = client
        .call_tool("slow", serde_json::json!({}))
        .await
        .expect("default tool timeout must be well above the old hard-coded 10s");
    assert!(out.contains("Slow: done"), "unexpected tool output: {out}");
}

/// A server-initiated `ping` must get a reply (`{}` result), not be
/// silently discarded. The mock validates the reply shape and reports it
/// as `SERVER_PING_OK`.
#[tokio::test]
async fn server_ping_is_answered() {
    let (_client, _dir, marker_path) = spawn_probe_and_marker("server-ping").await;
    let marker = probe_marker(&marker_path, "SERVER_PING_OK");
    assert!(
        marker.contains("SERVER_PING_OK"),
        "client must answer a server `ping` with a `{{}}` result; server recorded: {marker:?}"
    );
    assert!(
        !marker.contains("SERVER_PING_BAD"),
        "client answered `ping` with the wrong shape: {marker:?}"
    );
}

/// An unknown server-initiated method must get a JSON-RPC `-32601` error
/// rather than being dropped (the peer would otherwise wait).
#[tokio::test]
async fn unknown_server_request_gets_method_not_found() {
    let (_client, _dir, marker_path) = spawn_probe_and_marker("server-unknown").await;
    let marker = probe_marker(&marker_path, "SERVER_UNKNOWN_OK");
    assert!(
        marker.contains("SERVER_UNKNOWN_OK"),
        "client must answer an unknown server request with -32601; server recorded: {marker:?}"
    );
    assert!(
        !marker.contains("SERVER_UNKNOWN_BAD"),
        "client answered an unknown server request with the wrong shape: {marker:?}"
    );
}

/// Capabilities advertised in the spec's object form (`{"resources": {}}`)
/// must count as advertised — `as_bool()` alone reported "does not
/// advertise" for every real server.
#[tokio::test]
async fn object_capability_advertises_resources_and_prompts() {
    let mut client = spawn_mock("object-caps").await.expect("spawn mock server");

    let resources = client
        .list_resources()
        .await
        .expect("object-form `resources` capability must be advertised");
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].uri, "file:///tmp/caps.txt");

    let prompts = client
        .list_prompts()
        .await
        .expect("object-form `prompts` capability must be advertised");
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].name, "caps-prompt");
}

/// A `true` (legacy) capability must keep working — the fixture has to
/// cover both shapes.
#[tokio::test]
async fn boolean_capability_still_advertises() {
    let mut client = spawn_mock("echo").await.expect("spawn mock server");

    // `echo` advertises `{"tools": true, "resources": true, "prompts": true}`.
    assert_eq!(
        client.list_resources().await.expect("list_resources").len(),
        1
    );
    assert_eq!(client.list_prompts().await.expect("list_prompts").len(), 1);
}

/// A non-JSON line on stdout (server logging) must be skipped, not abort
/// the pending call. The mock prints one before *every* reply, including
/// the initialize handshake and `tools/list`.
#[tokio::test]
async fn non_json_stdout_line_is_skipped() {
    let mut client = spawn_mock("nonjson").await.expect("spawn mock server");

    let tools = client
        .list_tools()
        .await
        .expect("a stdout log line must not abort `tools/list`");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");

    let out = client
        .call_tool("echo", serde_json::json!({"message": "hi"}))
        .await
        .expect("a stdout log line must not abort `tools/call`");
    assert!(out.contains("NonJson: done"), "unexpected output: {out}");
}
