//! Pure-Rust mock MCP server used by `mcp::tests` integration tests.
//!
//! Reads JSON-RPC requests one-per-line from stdin and writes responses
//! one-per-line to stdout. Behavior is selected via the **first command
//! line argument** (e.g. `mock_mcp_server echo`) so we keep a single
//! `[[bin]]` target instead of one binary per test scenario, while
//! avoiding the safety hazards of mutating process-wide env vars from
//! tests.
//!
//! This replaces an earlier bash+python3 inline script approach that was
//! flaky on Linux CI (subprocess startup + buffering races). All
//! `#[ignore]` annotations on the affected tests can therefore be lifted.
//!
//! Modes (see `dispatch` below for exact response payloads):
//!
//! | mode               | initialize.capabilities | special behaviour                        |
//! |--------------------|-------------------------|------------------------------------------|
//! | `echo`             | tools+resources+prompts | full happy-path responses                |
//! | `malformed`        | n/a (never initializes) | writes a non-JSON line and stalls        |
//! | `timeout`          | n/a (never initializes) | reads forever, never writes              |
//! | `error-tool`       | (none)                  | tools/call returns `isError:true`        |
//! | `read-blob`        | resources               | resources/read returns a blob entry      |
//! | `prompt-args`      | prompts                 | prompts/get → "Hello, Alice!"            |
//! | `prompt-default`   | prompts                 | prompts/get → "default prompt"           |
//! | `empty-resources`  | resources               | resources/read → empty contents          |
//! | `tools-only`       | tools                   | every other method → -32601              |

use std::io::{self, BufRead, Write};

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "echo".into());

    // Behaviours that don't follow the line-loop pattern.
    match mode.as_str() {
        "malformed" => {
            // Emit garbage on the first line, then stall so the client's
            // initialize handshake observes a parse failure rather than EOF.
            let _ = writeln!(io::stdout(), "not json");
            let _ = io::stdout().flush();
            // Block forever — the client will time out or fail parsing.
            loop {
                std::thread::sleep(std::time::Duration::from_secs(60));
            }
        }
        "timeout" => {
            // Drain stdin but never produce any output. The client's
            // handshake must time out.
            let stdin = io::stdin();
            let mut buf = String::new();
            loop {
                buf.clear();
                if stdin.lock().read_line(&mut buf).unwrap_or(0) == 0 {
                    // stdin closed → block instead of exiting so the
                    // client doesn't observe a clean EOF.
                    std::thread::sleep(std::time::Duration::from_secs(60));
                }
            }
        }
        _ => {}
    }

    let stdin = io::stdin();
    let stdout = io::stdout();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            // Ignore unparseable input; tests don't depend on this path.
            continue;
        };
        let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("");
        let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);

        // Notifications carry no `id` and expect no reply (e.g.
        // `notifications/initialized`). MCP spec: server must not respond.
        if id.is_null() {
            continue;
        }

        if let Some(response) = dispatch(&mode, method, &id) {
            let mut out = stdout.lock();
            let _ = writeln!(out, "{response}");
            let _ = out.flush();
        }
    }
}

/// Build the JSON-RPC response line for a given (mode, method) pair.
/// Returns `None` if the request should be silently dropped (used only
/// for malformed-mode parity; not currently exercised here because
/// `malformed` short-circuits before reaching this function).
fn dispatch(mode: &str, method: &str, id: &serde_json::Value) -> Option<String> {
    use serde_json::json;

    let result: serde_json::Value = match (mode, method) {
        // --- initialize: capability shape varies per mode -----------------
        (_, "initialize") => {
            let capabilities = match mode {
                "echo" => json!({"tools": true, "resources": true, "prompts": true}),
                "read-blob" | "empty-resources" => json!({"resources": true}),
                "prompt-args" | "prompt-default" => json!({"prompts": true}),
                "tools-only" => json!({"tools": true}),
                "error-tool" => json!({}),
                _ => json!({}),
            };
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": capabilities,
                "serverInfo": {"name": "mock-server", "version": "1.0"},
            })
        }

        // --- tools --------------------------------------------------------
        ("echo", "tools/list") => json!({
            "tools": [{
                "name": "echo",
                "description": "Echo back the input",
                "inputSchema": {
                    "type": "object",
                    "properties": {"message": {"type": "string"}},
                    "required": ["message"],
                },
            }]
        }),
        ("echo", "tools/call") => json!({
            "content": [{"type": "text", "text": "Echo: hello"}]
        }),
        ("error-tool", "tools/list") => json!({
            "tools": [{
                "name": "failing",
                "description": "Always fails",
                "inputSchema": {"type": "object"},
            }]
        }),
        ("error-tool", "tools/call") => json!({
            "isError": true,
            "content": [{"type": "text", "text": "Something went wrong"}],
        }),

        // --- resources ----------------------------------------------------
        ("echo", "resources/list") => json!({
            "resources": [{
                "uri": "file:///tmp/test.txt",
                "name": "Test File",
                "description": "A test file",
                "mimeType": "text/plain",
            }]
        }),
        ("echo", "resources/read") => json!({
            "contents": [{
                "uri": "file:///tmp/test.txt",
                "mimeType": "text/plain",
                "text": "Hello, world!",
            }]
        }),
        ("read-blob", "resources/read") => json!({
            "contents": [{
                "uri": "file:///tmp/image.png",
                "mimeType": "image/png",
                "blob": "iVBORw0KGgoAAAANSUhEUgAAAAE=",
            }]
        }),
        ("empty-resources", "resources/read") => json!({"contents": []}),

        // --- prompts ------------------------------------------------------
        ("echo", "prompts/list") => json!({
            "prompts": [{
                "name": "greet",
                "description": "Greet someone",
                "arguments": [{
                    "name": "name",
                    "description": "The name to greet",
                    "required": true,
                }],
            }]
        }),
        ("echo", "prompts/get") => json!({
            "messages": [{
                "role": "user",
                "content": {"type": "text", "text": "Hello, world!"},
            }]
        }),
        ("prompt-args", "prompts/get") => json!({
            "messages": [{
                "role": "user",
                "content": {"type": "text", "text": "Hello, Alice!"},
            }]
        }),
        ("prompt-default", "prompts/get") => json!({
            "messages": [{
                "role": "assistant",
                "content": {"type": "text", "text": "default prompt"},
            }]
        }),

        // --- everything else: method not found ----------------------------
        _ => {
            return Some(
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32601, "message": "Method not found"},
                })
                .to_string(),
            );
        }
    };

    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string())
}
