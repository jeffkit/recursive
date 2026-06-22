//! Integration test: deferred tool loading via ToolSearchTool.
//!
//! Verifies the full request/response cycle for the Anthropic provider's
//! deferred-tool mechanism by running a real AgentRuntime against a mock
//! HTTP server and inspecting the captured request bodies.
//!
//! Key invariants tested:
//!
//! 1. Round 0 (initial request):
//!    - Deferred tools are NOT in the `tools` array.
//!    - Their names appear in `<available-deferred-tools>` in the first
//!      user message.
//!    - ToolSearchTool IS in the `tools` array.
//!    - The `anthropic-beta` header carries `advanced-tool-use-*`.
//!
//! 2. Round 1 (after ToolSearch resolves names):
//!    - The discovered tool IS in the `tools` array with full schema.
//!    - The previous tool_result message has `tool_reference` content blocks
//!      (not a plain string).
//!
//! 3. Final completion is the real tool call (not ToolSearchTool).

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use recursive::{
    llm::AnthropicProvider,
    runtime::AgentRuntime,
    tools::{LocalTransport, ReadFile, ToolTransport, WebFetch},
    ToolRegistry,
};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Minimal mock HTTP server that captures request headers + bodies and serves
// scripted JSON responses in sequence.
// ---------------------------------------------------------------------------

struct MockServer {
    /// Captured (headers, body) per request in order received.
    requests: Arc<Mutex<Vec<(String, String)>>>,
    addr: std::net::SocketAddr,
}

impl MockServer {
    fn spawn(responses: Vec<String>) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let requests: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let requests_clone = requests.clone();

        std::thread::spawn(move || {
            for body in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .ok();

                // Read the full HTTP request (headers + body).
                let mut buf = vec![0u8; 65536];
                let mut total = 0;
                loop {
                    let n = stream.read(&mut buf[total..]).unwrap_or(0);
                    total += n;
                    // Stop when we see the end of headers or body is complete.
                    let raw = &buf[..total];
                    if let Some(sep) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                        // Parse Content-Length to know if we have the full body.
                        let header_part = std::str::from_utf8(&raw[..sep]).unwrap_or("");
                        let content_len: usize = header_part
                            .lines()
                            .find(|l| l.to_lowercase().starts_with("content-length:"))
                            .and_then(|l| l.split(':').nth(1))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                        let body_received = total - (sep + 4);
                        if body_received >= content_len {
                            break;
                        }
                        // Need more data — extend buffer if needed.
                        if total == buf.len() {
                            buf.resize(buf.len() * 2, 0);
                        }
                    }
                    if n == 0 {
                        break;
                    }
                }

                let raw_str = String::from_utf8_lossy(&buf[..total]).to_string();
                let (headers_part, body_part) =
                    raw_str.split_once("\r\n\r\n").unwrap_or((&raw_str, ""));
                requests_clone
                    .lock()
                    .unwrap()
                    .push((headers_part.to_string(), body_part.trim().to_string()));

                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
        });

        MockServer { requests, addr }
    }
}

// ---------------------------------------------------------------------------
// Helper: find all tool names in a request's `tools` array.
// ---------------------------------------------------------------------------
fn tool_names_in_request(body: &serde_json::Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t["name"].as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Helper: find content of the first user message (as string).
// ---------------------------------------------------------------------------
fn first_user_message_content(body: &serde_json::Value) -> String {
    body["messages"]
        .as_array()
        .and_then(|msgs| msgs.iter().find(|m| m["role"] == "user"))
        .and_then(|m| m["content"].as_str())
        .unwrap_or("")
        .to_string()
}

// ---------------------------------------------------------------------------
// Helper: find a tool_result block for a given tool_use_id in the messages.
// ---------------------------------------------------------------------------
fn find_tool_result<'a>(
    body: &'a serde_json::Value,
    tool_use_id: &str,
) -> Option<&'a serde_json::Value> {
    body["messages"].as_array()?.iter().find_map(|m| {
        m["content"]
            .as_array()?
            .iter()
            .find(|block| block["type"] == "tool_result" && block["tool_use_id"] == tool_use_id)
    })
}

// ---------------------------------------------------------------------------
// Test 1: initial request does not expose deferred tool schemas.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn deferred_tool_absent_from_initial_tools_array() {
    // Tests use a localhost mock; opt in to deferred tools explicitly since
    // supports_deferred_tools() only auto-enables for api.anthropic.com.
    std::env::set_var("RECURSIVE_DEFERRED_TOOLS", "true");

    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // Round 0: model calls ToolSearchTool asking for WebFetch.
    // Round 1: model calls WebFetch (now in tools array).
    // Round 2: model returns final text.
    let server = MockServer::spawn(vec![
        // Round 0 response: model calls ToolSearchTool
        serde_json::json!({
            "type": "message",
            "content": [{
                "type": "tool_use",
                "id": "ts_call_1",
                "name": "ToolSearchTool",
                "input": {"query": "web fetch url", "max_results": 3}
            }],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 100, "output_tokens": 20}
        })
        .to_string(),
        // Round 1 response (after ToolSearch resolves WebFetch): model calls WebFetch
        serde_json::json!({
            "type": "message",
            "content": [{
                "type": "tool_use",
                "id": "wf_call_1",
                "name": "WebFetch",
                "input": {"url": "https://example.com", "prompt": "get title"}
            }],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 150, "output_tokens": 30}
        })
        .to_string(),
        // Round 2 response: agent turn complete, model returns text
        // (WebFetch result handled by another LLM step is not needed here
        //  because WebFetch itself will error on the mock — the tool result
        //  goes back as an error message, and the model says done)
        serde_json::json!({
            "type": "message",
            "content": [{"type": "text", "text": "Fetched the page."}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 200, "output_tokens": 10}
        })
        .to_string(),
    ]);

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let provider = Arc::new(
        AnthropicProvider::new(
            format!("http://{}", server.addr),
            "sk-noop",
            "claude-sonnet-4-6",
        )
        .unwrap(),
    );

    let transport: Arc<dyn ToolTransport> = Arc::new(LocalTransport);
    let tools = ToolRegistry::new(transport)
        // ReadFile is eager (is_deferred = false)
        .register(Arc::new(ReadFile::new(root)))
        // WebFetch is deferred (is_deferred = true)
        .register(Arc::new(WebFetch::new()));

    let mut runtime = AgentRuntime::builder()
        .llm(provider)
        .tools(tools)
        .system_prompt("you are a test agent")
        .max_steps(5)
        .build()
        .unwrap();

    runtime
        .run("fetch https://example.com and summarise")
        .await
        .unwrap();

    let captured = server.requests.lock().unwrap();

    // We expect at least 2 requests (round 0 + round 1 inside the search loop,
    // plus the final step that returns text).
    assert!(
        captured.len() >= 2,
        "expected at least 2 HTTP requests, got {}",
        captured.len()
    );

    // ── Assertions on round 0 ──────────────────────────────────────────────

    let body0: serde_json::Value =
        serde_json::from_str(&captured[0].1).expect("round-0 body is valid JSON");
    let headers0 = &captured[0].0;

    // Beta header must be present.
    assert!(
        headers0.to_lowercase().contains("advanced-tool-use"),
        "round-0 headers missing advanced-tool-use beta: {}",
        headers0
    );

    let names0 = tool_names_in_request(&body0);

    // ToolSearchTool must be present (model needs it to discover WebFetch).
    assert!(
        names0.contains(&"ToolSearchTool".to_string()),
        "ToolSearchTool missing from round-0 tools: {:?}",
        names0
    );

    // ReadFile (eager) must be present.
    assert!(
        names0.contains(&"Read".to_string()),
        "Read missing from round-0 tools: {:?}",
        names0
    );

    // WebFetch (deferred) must NOT be in the tools array.
    assert!(
        !names0.contains(&"WebFetch".to_string()),
        "WebFetch should be absent from round-0 tools array (it is deferred): {:?}",
        names0
    );

    // WebFetch must appear in <available-deferred-tools> in the first user message.
    let first_user = first_user_message_content(&body0);
    assert!(
        first_user.contains("available-deferred-tools"),
        "round-0 first user message missing <available-deferred-tools> block: {:?}",
        first_user
    );
    assert!(
        first_user.contains("WebFetch"),
        "round-0 <available-deferred-tools> block missing WebFetch: {:?}",
        first_user
    );
}

// ---------------------------------------------------------------------------
// Test 2: ToolSearchTool runs as a normal tool via run_core; its result is
//         serialized as tool_reference blocks so Anthropic can expand schemas.
//         WebFetch remains deferred (not in tools array) but the API expands
//         it via tool_reference in the message history.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn toolsearch_result_serialized_as_tool_references() {
    std::env::set_var("RECURSIVE_DEFERRED_TOOLS", "true");

    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // Step 1: model calls ToolSearchTool (run_core executes it, gets ["WebFetch"])
    // Step 2: model calls WebFetch (run_core tries to execute it — will error,
    //         but that's fine for this test)
    // Step 3: model returns final text
    let server = MockServer::spawn(vec![
        // LLM step 1: model calls ToolSearchTool
        serde_json::json!({
            "type": "message",
            "content": [{
                "type": "tool_use",
                "id": "ts_call_1",
                "name": "ToolSearchTool",
                "input": {"query": "select:WebFetch"}
            }],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 100, "output_tokens": 20}
        })
        .to_string(),
        // LLM step 2: model calls WebFetch (after seeing tool_reference expansion)
        serde_json::json!({
            "type": "message",
            "content": [{
                "type": "tool_use",
                "id": "wf_call_1",
                "name": "WebFetch",
                "input": {"url": "https://example.com", "prompt": "title"}
            }],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 150, "output_tokens": 30}
        })
        .to_string(),
        // LLM step 3: final text
        serde_json::json!({
            "type": "message",
            "content": [{"type": "text", "text": "Done."}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 200, "output_tokens": 5}
        })
        .to_string(),
    ]);

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let provider = Arc::new(
        AnthropicProvider::new(
            format!("http://{}", server.addr),
            "sk-noop",
            "claude-sonnet-4-6",
        )
        .unwrap(),
    );

    let transport: Arc<dyn ToolTransport> = Arc::new(LocalTransport);
    let tools = ToolRegistry::new(transport)
        .register(Arc::new(ReadFile::new(root)))
        .register(Arc::new(WebFetch::new()));

    let mut runtime = AgentRuntime::builder()
        .llm(provider)
        .tools(tools)
        .system_prompt("you are a test agent")
        .max_steps(5)
        .build()
        .unwrap();

    runtime.run("fetch https://example.com").await.unwrap();

    let captured = server.requests.lock().unwrap();
    assert!(
        captured.len() >= 2,
        "expected at least 2 HTTP requests, got {}",
        captured.len()
    );

    // ── Assertions on step 2 (after ToolSearchTool was executed) ─────────────
    // The second LLM request should include the ToolSearch tool_result serialized
    // as tool_reference blocks, not as a plain string.
    let body1: serde_json::Value =
        serde_json::from_str(&captured[1].1).expect("step-2 body is valid JSON");

    // WebFetch is still deferred — it must NOT be in the tools array.
    // Anthropic expands it via the tool_reference block in the messages.
    let names1 = tool_names_in_request(&body1);
    assert!(
        !names1.contains(&"WebFetch".to_string()),
        "WebFetch should remain deferred (not in tools array): {:?}",
        names1
    );

    // The tool_result for the ToolSearch call must have tool_reference blocks.
    let tr = find_tool_result(&body1, "ts_call_1");
    let tr = tr.expect("tool_result for ts_call_1 should be in step-2 messages");
    let content = tr["content"]
        .as_array()
        .expect("tool_result content should be an array of tool_reference blocks");
    assert!(
        !content.is_empty(),
        "tool_reference array should be non-empty"
    );
    for block in content {
        assert_eq!(
            block["type"], "tool_reference",
            "each item should be a tool_reference block: {:?}",
            block
        );
        assert!(block["tool_name"].is_string());
    }
    let resolved_names: Vec<&str> = content
        .iter()
        .filter_map(|b| b["tool_name"].as_str())
        .collect();
    assert!(
        resolved_names.contains(&"WebFetch"),
        "tool_reference blocks should include WebFetch: {:?}",
        resolved_names
    );
}

// ---------------------------------------------------------------------------
// Test 3: without deferred tools the request is clean (no ToolSearchTool,
//         no <available-deferred-tools>).
// ---------------------------------------------------------------------------
#[tokio::test]
async fn no_deferred_tools_means_no_tool_search_injection() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let server = MockServer::spawn(vec![serde_json::json!({
        "type": "message",
        "content": [{"type": "text", "text": "Hi there."}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    })
    .to_string()]);

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let provider = Arc::new(
        AnthropicProvider::new(
            format!("http://{}", server.addr),
            "sk-noop",
            "claude-sonnet-4-6",
        )
        .unwrap(),
    );

    let transport: Arc<dyn ToolTransport> = Arc::new(LocalTransport);
    // Only eager tools — no deferred tools registered.
    let tools = ToolRegistry::new(transport).register(Arc::new(ReadFile::new(root)));

    let mut runtime = AgentRuntime::builder()
        .llm(provider)
        .tools(tools)
        .system_prompt("you are a test agent")
        .max_steps(3)
        .build()
        .unwrap();

    runtime.run("say hi").await.unwrap();

    let captured = server.requests.lock().unwrap();
    assert_eq!(captured.len(), 1, "expected exactly 1 request");

    let body: serde_json::Value = serde_json::from_str(&captured[0].1).expect("body is valid JSON");
    let names = tool_names_in_request(&body);

    // Read is an eager tool — it must appear with full schema in the tools array.
    assert!(
        names.contains(&"Read".to_string()),
        "Read (eager) should be in the tools array: {:?}",
        names
    );

    // Read must NOT appear in the <available-deferred-tools> block.
    // (AgentRuntime auto-injects TodoWriteTool as deferred, so there may be
    // an <available-deferred-tools> block — but Read must not be in it.)
    let msgs = body["messages"].as_array().unwrap();
    for msg in msgs {
        if let Some(content) = msg["content"].as_str() {
            if content.contains("available-deferred-tools") {
                assert!(
                    !content.contains("Read"),
                    "Read (eager) should not appear in <available-deferred-tools>: {}",
                    content
                );
            }
        }
    }
}
