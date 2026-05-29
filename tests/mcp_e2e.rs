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

/// Spawn the pure-Rust mock MCP server binary in the given behavior mode.
async fn spawn_mock(mode: &str) -> Result<McpClient> {
    let server = McpServer {
        name: "mock".to_string(),
        command: env!("CARGO_BIN_EXE_mock_mcp_server").to_string(),
        args: vec![mode.to_string()],
        url: None,
    };
    McpClient::spawn_with_timeout(&server, TEST_READ_TIMEOUT).await
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
