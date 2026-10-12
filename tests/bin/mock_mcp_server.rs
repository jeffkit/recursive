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
//!
//! issue #181 modes:
//!
//! | mode                | initialize.capabilities  | special behaviour                                  |
//! |---------------------|--------------------------|----------------------------------------------------|
//! | `delay-call`        | tools (object form)      | tools/call sleeps 12s before replying              |
//! | `server-ping`       | tools                    | sends `ping` first; validates client's reply on stderr |
//! | `server-unknown`    | tools                    | sends `unknown/method` first; validates -32601     |
//! | `nonjson`           | tools                    | emits a non-JSON progress line before each reply   |
//! | `object-caps`       | resources/prompts as `{}`| spec-shaped (object) capability advertisement      |

use std::io::{self, BufRead, Write};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().cloned().unwrap_or_else(|| "echo".into());
    // `mock_mcp_server <mode> --marker-file <path>`: also append the
    // server-probe verdict to a file, so a test can learn what the *real*
    // `McpClient` replied without having to own the child's stderr.
    let marker_file: Option<String> = args
        .iter()
        .position(|a| a == "--marker-file")
        .and_then(|i| args.get(i + 1).cloned());

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

    // issue #181: server-initiated request modes. The request is pushed
    // *before* the client's first line is read, so the client's very first
    // `read_response` (the initialize handshake) is the one that has to
    // deal with it. The client's reply is validated and echoed to stderr
    // with a `SERVER_PING_OK` / `SERVER_UNKNOWN_OK` marker the test greps
    // for; a wrong (or missing) reply prints `SERVER_*_BAD` instead.
    let probe = match mode.as_str() {
        "server-ping" => Some(("1", "ping")),
        "server-unknown" => Some(("1", "unknown/method")),
        _ => None,
    };
    if let Some((id, method)) = probe {
        let mut out = stdout.lock();
        let _ = writeln!(
            out,
            "{}",
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method})
        );
        let _ = out.flush();
    }

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

        // issue #181: the reply to our own probe arrives here — it has our
        // probe id and neither `method` nor a result of its own method, so
        // intercept it before the normal dispatch table turns it into a
        // "Method not found".
        if let Some((probe_id, probe_method)) = probe {
            if method.is_empty() && id.as_str() == Some(probe_id) {
                report_probe_reply(probe_method, &req, marker_file.as_deref());
                continue;
            }
        }

        // issue #181: a non-JSON line on stdout (server logging) must not
        // abort the client's pending call.
        if mode == "nonjson" {
            let mut out = stdout.lock();
            let _ = writeln!(out, "[mock] progress: handling {method}");
            let _ = out.flush();
        }

        // issue #181: slow tool call — 12s, well past the old hard-coded 10s.
        if mode == "delay-call" && method == "tools/call" {
            std::thread::sleep(std::time::Duration::from_secs(12));
        }

        if let Some(response) = dispatch(&mode, method, &id) {
            let mut out = stdout.lock();
            let _ = writeln!(out, "{response}");
            let _ = out.flush();
        }
    }
}

/// Validate the client's reply to one of the issue #181 server-initiated
/// probes and record a machine-greppable marker (stderr, plus
/// `marker_file` when given).
///
/// `SERVER_PING_OK` — a well-formed `{"result":{}}` for `ping`.
/// `SERVER_UNKNOWN_OK` — a well-formed JSON-RPC `-32601` error.
/// `SERVER_*_BAD` — anything else (missing reply, wrong shape, wrong id).
fn report_probe_reply(probe_method: &str, reply: &serde_json::Value, marker_file: Option<&str>) {
    let err = io::stderr();
    let mut err = err.lock();
    let code = reply
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_i64());
    let marker = match probe_method {
        "ping" => {
            let ok = reply.get("result").map(|r| r.is_object()).unwrap_or(false)
                && reply.get("error").is_none();
            if ok {
                "SERVER_PING_OK"
            } else {
                "SERVER_PING_BAD"
            }
        }
        _ => {
            if code == Some(-32601) {
                "SERVER_UNKNOWN_OK"
            } else {
                "SERVER_UNKNOWN_BAD"
            }
        }
    };
    let _ = writeln!(err, "{marker}");
    let _ = err.flush();
    if let Some(path) = marker_file {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{marker}");
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
                "tools-only" | "nonjson" => json!({"tools": true}),
                "error-tool" => json!({}),
                // issue #181: the spec wire shape — capabilities are
                // *objects* (e.g. `"tools": {}`), not booleans. Reading
                // them with `as_bool()` reports "does not advertise".
                "object-caps" => json!({"tools": {}, "resources": {}, "prompts": {}}),
                "delay-call" => json!({"tools": {}}),
                _ => json!({}),
            };
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": capabilities,
                "serverInfo": {"name": "mock-server", "version": "1.0"},
            })
        }

        // --- tools --------------------------------------------------------
        ("delay-call", "tools/list") => json!({
            "tools": [{
                "name": "slow",
                "description": "Sleeps 12s before answering",
                "inputSchema": {"type": "object"},
            }]
        }),
        ("delay-call", "tools/call") => json!({
            "content": [{"type": "text", "text": "Slow: done"}]
        }),
        ("nonjson", "tools/list") => json!({
            "tools": [{
                "name": "echo",
                "description": "Echo back the input",
                "inputSchema": {"type": "object"},
            }]
        }),
        ("nonjson", "tools/call") => json!({
            "content": [{"type": "text", "text": "NonJson: done"}]
        }),
        ("object-caps", "resources/list") => json!({
            "resources": [{
                "uri": "file:///tmp/caps.txt",
                "name": "Caps File",
            }]
        }),
        ("object-caps", "prompts/list") => json!({
            "prompts": [{
                "name": "caps-prompt",
                "description": "Advertised as an object",
            }]
        }),
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
