//! ACP v1 stdio JSON-RPC transport loop.
//!
//! # Transport contract
//!
//! - **Framing**: newline-delimited JSON. One JSON-RPC message per line;
//!   no whitespace padding, no length prefix.
//! - **Request/response matching**: each request carries an `id` field
//!   (string or number). The server echoes the same `id` in its response.
//!   Clients use the `id` to correlate responses with their originating
//!   requests even when responses arrive out-of-order.
//! - **Notifications**: a JSON-RPC message without an `id` field is a
//!   notification. Notifications are consumed silently — no response is
//!   written to stdout.
//! - **Batch semantics**: when the input line is a JSON array, the server
//!   treats it as a batch. Every element is processed independently and
//!   the server writes back a JSON array of responses (notifications
//!   excluded). An empty batch is an error (-32600).
//! - **Stdout/stderr contract**: every byte written to stdout is valid
//!   JSON-RPC (`"jsonrpc":"2.0"` present). All logging, diagnostics, and
//!   tracing output goes to stderr only.
//!
//! # Handshake state machine
//!
//! The server tracks initialization state:
//! - **Uninitialized** (startup): only `initialize` is accepted. Any other
//!   method returns `-32002` ("Server not initialized").
//! - **Initialized** (after successful `initialize`): `initialize` is
//!   rejected as a duplicate (`-32002`). Other methods return `-32601`
//!   (Method not found) in Sprint 1.
//!
//! # Collaborative cancel (Decision 4c)
//!
//! In future sprints, `session/cancel` will trigger a cooperative abort:
//! the agent sets a [`CancellationToken`], the LLM streaming loop
//! (`tokio::select!`) observes it, and the run returns
//! `FinishReason::Cancelled`. The ACP bridge maps that to
//! `stopReason: "cancelled"`. See [`.dev/goals/325-acp-protocol-support.md`]
//! for the full design.

use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::debug;

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 error codes
// ---------------------------------------------------------------------------

const PARSE_ERROR: i32 = -32700;
const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const SERVER_NOT_INITIALIZED: i32 = -32002;

/// Supported protocol version.
const SUPPORTED_PROTOCOL_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Server state machine
// ---------------------------------------------------------------------------

/// Tracks whether the client has completed the `initialize` handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerState {
    /// Waiting for the first `initialize` request.
    Uninitialized,
    /// Handshake complete; further `initialize` calls are rejected.
    Initialized,
}

// ---------------------------------------------------------------------------
// Raw JSON-RPC wire types (minimal, for dispatch)
// ---------------------------------------------------------------------------

/// Minimal parsed JSON-RPC request envelope.
///
/// We use `serde_json::Value` for `id` and `params` to avoid coupling to
/// the full ACP routing types at the transport layer. Method dispatch does
/// a second typed parse for specific schemas (e.g. `InitializeRequest`).
#[derive(Debug, Deserialize)]
struct JsonRpcEnvelope {
    #[serde(default)]
    jsonrpc: Option<String>,
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Value>,
}

// ---------------------------------------------------------------------------
// Response builders
// ---------------------------------------------------------------------------

/// Build a JSON-RPC 2.0 success response.
fn build_success(id: Value, result: Value) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

/// Build a JSON-RPC 2.0 error response.
fn build_error(id: Value, code: i32, message: &str) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
        },
    })
}

// ---------------------------------------------------------------------------
// Method dispatch
// ---------------------------------------------------------------------------

/// Dispatch a single JSON-RPC request envelope.
///
/// Returns `None` for notifications (no `id` field). Returns a
/// JSON-RPC response [`Value`] for requests.
///
/// `state` is mutated in-place: a successful `initialize` transitions
/// `Uninitialized → Initialized`.
fn dispatch(envelope: &JsonRpcEnvelope, state: &mut ServerState) -> Option<Value> {
    // --- validate jsonrpc field ---
    match &envelope.jsonrpc {
        None => {
            let id = envelope.id.clone().unwrap_or(Value::Null);
            return Some(build_error(
                id,
                PARSE_ERROR,
                "Parse error: missing 'jsonrpc' field",
            ));
        }
        Some(v) if v != "2.0" => {
            let id = envelope.id.clone().unwrap_or(Value::Null);
            return Some(build_error(
                id,
                INVALID_REQUEST,
                &format!("Invalid jsonrpc version: expected '2.0', got '{v}'"),
            ));
        }
        _ => {}
    }

    // --- get method name ---
    let method = match &envelope.method {
        Some(m) => m.as_str(),
        None => {
            let id = envelope.id.clone().unwrap_or(Value::Null);
            return Some(build_error(
                id,
                INVALID_REQUEST,
                "Missing required field 'method'",
            ));
        }
    };

    // --- notifications: no response ---
    let id = match &envelope.id {
        Some(id) => id.clone(),
        None => return None,
    };

    // --- dispatch based on method and current state ---
    match method {
        "initialize" => match *state {
            ServerState::Uninitialized => {
                let resp = handle_initialize(id, envelope.params.as_ref());
                // Transition to Initialized only on success (response has "result").
                if resp.get("result").is_some() {
                    *state = ServerState::Initialized;
                }
                Some(resp)
            }
            ServerState::Initialized => Some(build_error(
                id,
                SERVER_NOT_INITIALIZED,
                "Server already initialized",
            )),
        },
        "notifications/initialized" => {
            // Protocol-level notification — no response.
            None
        }
        _ => match *state {
            ServerState::Uninitialized => Some(build_error(
                id,
                SERVER_NOT_INITIALIZED,
                "Server not initialized",
            )),
            ServerState::Initialized => Some(build_error(
                id,
                METHOD_NOT_FOUND,
                &format!("Method not found: {method}"),
            )),
        },
    }
}

/// Handle the `initialize` method.
///
/// Validates params: `protocolVersion` must be present, a number, and
/// equal to the server's supported version. Returns `InvalidParams`
/// (-32602) for missing, wrong-type, or unsupported version.
fn handle_initialize(id: Value, params: Option<&Value>) -> Value {
    // --- validate params ---
    if let Some(params) = params {
        // Extract protocolVersion — must be present and must be a number.
        match params.get("protocolVersion") {
            None => {
                return build_error(
                    id,
                    INVALID_PARAMS,
                    "Missing required field 'protocolVersion'",
                );
            }
            Some(v) if !v.is_number() => {
                return build_error(
                    id,
                    INVALID_PARAMS,
                    "Invalid params: 'protocolVersion' must be a number",
                );
            }
            Some(v) => {
                let version: u32 = v.as_u64().unwrap_or(0) as u32;
                if version != SUPPORTED_PROTOCOL_VERSION {
                    return build_error(
                        id,
                        INVALID_PARAMS,
                        &format!(
                            "Unsupported protocol version {version}; server supports {SUPPORTED_PROTOCOL_VERSION}",
                        ),
                    );
                }
            }
        }
    }

    // --- build response with full capabilities ---
    // We build the capabilities JSON manually so we can include `fsCapabilities`,
    // which is not part of the upstream `AgentCapabilities` struct (it's a client
    // capability in the ACP spec, but editors expect it in the agent response).
    let result = serde_json::json!({
        "protocolVersion": SUPPORTED_PROTOCOL_VERSION,
        "agentInfo": {
            "name": "recursive",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "agentCapabilities": {
            "loadSession": false,
            "promptCapabilities": {
                "text": true,
                "image": false,
                "audio": false,
                "embeds": false,
            },
            "mcpCapabilities": {},
            "sessionCapabilities": {},
            "auth": {},
            "fsCapabilities": {
                "readTextFile": false,
                "writeTextFile": false,
            },
        },
    });

    build_success(id, result)
}

// ---------------------------------------------------------------------------
// AcpServer — stdio transport loop
// ---------------------------------------------------------------------------

/// ACP stdio JSON-RPC server.
///
/// Reads newline-delimited JSON-RPC 2.0 from stdin, dispatches requests,
/// and writes responses to stdout. Reuses the same async I/O pattern as
/// [`McpServerRunner`](crate::mcp::McpServerRunner).
pub struct AcpServer;

impl AcpServer {
    /// Run the server on real stdio until EOF.
    pub async fn run() {
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();
        Self::run_io(BufReader::new(stdin), stdout).await;
    }

    /// Run the server on generic reader/writer (testable).
    pub async fn run_io<R, W>(reader: R, mut writer: W)
    where
        R: tokio::io::AsyncBufRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let mut lines = BufReader::new(reader).lines();
        let mut state = ServerState::Uninitialized;

        while let Some(line) = match lines.next_line().await {
            Ok(l) => l,
            Err(e) => {
                debug!(%e, "ACP stdin read error");
                None
            }
        } {
            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }

            // --- check for batch (JSON array) vs single object ---
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                // Batch: parse as Vec<Value> first for robustness
                let batch: Vec<Value> = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => {
                        let resp =
                            build_error(Value::Null, PARSE_ERROR, "Parse error: invalid JSON");
                        let json = serde_json::to_string(&resp).unwrap_or_default();
                        let _ = writer.write_all(json.as_bytes()).await;
                        let _ = writer.write_all(b"\n").await;
                        let _ = writer.flush().await;
                        continue;
                    }
                };

                if batch.is_empty() {
                    let resp =
                        build_error(Value::Null, INVALID_REQUEST, "Empty batch is not allowed");
                    let json = serde_json::to_string(&resp).unwrap_or_default();
                    let _ = writer.write_all(json.as_bytes()).await;
                    let _ = writer.write_all(b"\n").await;
                    let _ = writer.flush().await;
                    continue;
                }

                let mut responses: Vec<Value> = Vec::new();
                for item in &batch {
                    match serde_json::from_value::<JsonRpcEnvelope>(item.clone()) {
                        Ok(env) => {
                            if let Some(resp) = dispatch(&env, &mut state) {
                                responses.push(resp);
                            }
                        }
                        Err(_) => {
                            // Try to extract id from the raw item
                            let id = item.get("id").cloned().unwrap_or(Value::Null);
                            responses.push(build_error(
                                id,
                                PARSE_ERROR,
                                "Parse error: invalid JSON-RPC message",
                            ));
                        }
                    }
                }

                if !responses.is_empty() {
                    let json = serde_json::to_string(&responses).unwrap_or_default();
                    let _ = writer.write_all(json.as_bytes()).await;
                    let _ = writer.write_all(b"\n").await;
                    let _ = writer.flush().await;
                }
            } else {
                // Single request/notification
                match serde_json::from_str::<JsonRpcEnvelope>(&line) {
                    Ok(env) => {
                        if let Some(resp) = dispatch(&env, &mut state) {
                            let json = serde_json::to_string(&resp).unwrap_or_default();
                            let _ = writer.write_all(json.as_bytes()).await;
                            let _ = writer.write_all(b"\n").await;
                            let _ = writer.flush().await;
                        }
                    }
                    Err(_) => {
                        let resp =
                            build_error(Value::Null, PARSE_ERROR, "Parse error: invalid JSON");
                        let json = serde_json::to_string(&resp).unwrap_or_default();
                        let _ = writer.write_all(json.as_bytes()).await;
                        let _ = writer.write_all(b"\n").await;
                        let _ = writer.flush().await;
                    }
                }
            }
        }

        debug!("ACP stdin closed, server shutting down");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── helpers ──────────────────────────────────────────────────────

    /// Run the server on a string input, return captured stdout lines.
    async fn run_server(input: &str) -> Vec<String> {
        let reader = std::io::Cursor::new(input.as_bytes().to_owned());
        let mut output = Vec::<u8>::new();
        AcpServer::run_io(reader, &mut output).await;
        let text = String::from_utf8(output).unwrap();
        text.lines().map(|s| s.to_string()).collect()
    }

    /// Helper: parse first stdout line as JSON Value.
    fn parse_line(lines: &[String], idx: usize) -> Value {
        serde_json::from_str(&lines[idx]).expect("valid JSON")
    }

    // ══════════════════════════════════════════════════════════════════
    // S1-C1/C2/C3: basic transport
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn initialize_returns_success() {
        let lines = run_server(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
        )
        .await;
        assert_eq!(lines.len(), 1);
        let v = parse_line(&lines, 0);
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 1);
        assert!(v["result"].is_object());
    }

    // ══════════════════════════════════════════════════════════════════
    // S1-C4: initialize response structure
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn initialize_has_protocol_version_and_agent_info() {
        let lines = run_server(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
        )
        .await;
        let v = parse_line(&lines, 0);
        let result = &v["result"];
        assert_eq!(result["protocolVersion"], 1);
        assert_eq!(result["agentInfo"]["name"], "recursive");
        assert!(result["agentInfo"]["version"].is_string());
    }

    // ══════════════════════════════════════════════════════════════════
    // S1-C5: all capability sub-objects present
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn agent_capabilities_has_all_required_keys() {
        let lines = run_server(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
        )
        .await;
        let v = parse_line(&lines, 0);
        let caps = &v["result"]["agentCapabilities"];
        assert!(caps["promptCapabilities"].is_object());
        assert!(caps["mcpCapabilities"].is_object());
        assert!(caps["sessionCapabilities"].is_object());
        assert!(caps["fsCapabilities"].is_object());
    }

    // ══════════════════════════════════════════════════════════════════
    // S1-C6: promptCapabilities.text = true
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn prompt_capabilities_text_is_true() {
        let lines = run_server(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
        )
        .await;
        let v = parse_line(&lines, 0);
        assert_eq!(
            v["result"]["agentCapabilities"]["promptCapabilities"]["text"],
            true
        );
    }

    // ══════════════════════════════════════════════════════════════════
    // S1-C7: malformed input → ParseError (-32700)
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn malformed_json_returns_parse_error() {
        let lines = run_server("not json").await;
        assert_eq!(lines.len(), 1);
        let v = parse_line(&lines, 0);
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], Value::Null);
        assert_eq!(v["error"]["code"], PARSE_ERROR);
        assert!(!v["error"]["message"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn json_without_jsonrpc_has_id_from_input() {
        // S1-C7: {"id":1} (no jsonrpc) → error with id=1
        let lines = run_server(r#"{"id":1}"#).await;
        assert_eq!(lines.len(), 1);
        let v = parse_line(&lines, 0);
        assert_eq!(v["id"], 1);
        assert_eq!(v["error"]["code"], PARSE_ERROR);
    }

    // ══════════════════════════════════════════════════════════════════
    // S1-C8: after init, unknown methods → MethodNotFound (-32601)
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn after_init_unknown_method_is_not_found() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{}}"#,
        );
        let lines = run_server(input).await;
        assert_eq!(lines.len(), 2);
        let v0 = parse_line(&lines, 0);
        assert!(
            v0["result"].is_object(),
            "first response should be init success"
        );
        let v1 = parse_line(&lines, 1);
        assert_eq!(v1["error"]["code"], METHOD_NOT_FOUND);
    }

    // ══════════════════════════════════════════════════════════════════
    // S1-C9: notifications → no response
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn notification_produces_no_response() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","method":"someNotification","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
        );
        let lines = run_server(input).await;
        assert_eq!(lines.len(), 1);
        let v = parse_line(&lines, 0);
        assert_eq!(v["id"], 1);
        assert!(v["result"].is_object());
    }

    // ══════════════════════════════════════════════════════════════════
    // S1-C10: quality gates (structural tests here; gates run externally)
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn stdout_only_contains_json_rpc() {
        let input =
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#;
        let lines = run_server(input).await;
        for line in &lines {
            let v: Value =
                serde_json::from_str(line).expect("every stdout line must be valid JSON");
            assert_eq!(
                v["jsonrpc"], "2.0",
                "every stdout line must contain jsonrpc: 2.0"
            );
        }
    }

    // ══════════════════════════════════════════════════════════════════
    // S1-C11: before init, non-init methods → ServerNotInitialized (-32002)
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn before_init_other_methods_return_server_not_initialized() {
        let input = r#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{}}"#;
        let lines = run_server(input).await;
        assert_eq!(lines.len(), 1);
        let v = parse_line(&lines, 0);
        assert_eq!(v["error"]["code"], SERVER_NOT_INITIALIZED);
        assert_eq!(v["error"]["message"], "Server not initialized");
    }

    // ══════════════════════════════════════════════════════════════════
    // S1-C12: duplicate initialize → error (-32002)
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn duplicate_initialize_returns_error() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":1}}"#,
        );
        let lines = run_server(input).await;
        assert_eq!(lines.len(), 2);
        let v0 = parse_line(&lines, 0);
        assert!(v0["result"].is_object(), "first init should succeed");
        let v1 = parse_line(&lines, 1);
        assert!(v1["error"].is_object(), "second init should be an error");
        assert_eq!(v1["error"]["code"], SERVER_NOT_INITIALIZED);
    }

    // ══════════════════════════════════════════════════════════════════
    // S1-C13: invalid params → InvalidParams (-32602)
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn missing_protocol_version_returns_invalid_params() {
        // S1-C13: params={} (missing protocolVersion) → -32602
        let input = r#"{"jsonrpc":"2.0","id":3,"method":"initialize","params":{}}"#;
        let lines = run_server(input).await;
        assert_eq!(lines.len(), 1);
        let v = parse_line(&lines, 0);
        assert_eq!(v["error"]["code"], INVALID_PARAMS);
        let msg = v["error"]["message"].as_str().unwrap();
        assert!(
            msg.contains("protocolVersion"),
            "message must mention protocolVersion: {msg}"
        );
    }

    #[tokio::test]
    async fn string_protocol_version_returns_invalid_params() {
        // S1-C13: protocolVersion as string → -32602
        let input =
            r#"{"jsonrpc":"2.0","id":4,"method":"initialize","params":{"protocolVersion":"1"}}"#;
        let lines = run_server(input).await;
        assert_eq!(lines.len(), 1);
        let v = parse_line(&lines, 0);
        assert_eq!(v["error"]["code"], INVALID_PARAMS);
    }

    #[tokio::test]
    async fn unsupported_protocol_version_returns_invalid_params() {
        let input =
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":99}}"#;
        let lines = run_server(input).await;
        assert_eq!(lines.len(), 1);
        let v = parse_line(&lines, 0);
        assert_eq!(v["error"]["code"], INVALID_PARAMS);
        let msg = v["error"]["message"].as_str().unwrap();
        assert!(
            msg.contains("99"),
            "message must include rejected version: {msg}"
        );
        assert!(
            msg.contains("1"),
            "message must mention supported version: {msg}"
        );
    }

    // ══════════════════════════════════════════════════════════════════
    // Batch & robustness tests
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn batch_returns_array_of_responses() {
        let input = "[{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":1}},{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"initialize\",\"params\":{\"protocolVersion\":1}}]";
        let lines = run_server(input).await;
        assert_eq!(lines.len(), 1);
        let v: Value = serde_json::from_str(&lines[0]).unwrap();
        assert!(v.is_array(), "batch response must be an array");
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["id"], 1);
        assert!(arr[0]["result"].is_object(), "first init succeeds");
        assert!(arr[1]["error"].is_object(), "second init fails (duplicate)");
    }

    #[tokio::test]
    async fn batch_before_init_mixed_methods() {
        // S1-C11 + batch: session/new before init → -32002
        let input = "[{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/new\",\"params\":{}},{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":1}}]";
        let lines = run_server(input).await;
        assert_eq!(lines.len(), 1);
        let v: Value = serde_json::from_str(&lines[0]).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["error"]["code"], SERVER_NOT_INITIALIZED);
        assert!(arr[1]["result"].is_object());
    }

    #[tokio::test]
    async fn empty_input_produces_no_output() {
        let lines = run_server("").await;
        assert!(lines.is_empty());
    }

    #[tokio::test]
    async fn blank_line_skipped() {
        let input = "\n\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":1}}\n\n";
        let lines = run_server(input).await;
        assert_eq!(lines.len(), 1);
    }

    #[tokio::test]
    async fn missing_method_returns_invalid_request() {
        let lines = run_server(r#"{"jsonrpc":"2.0","id":1}"#).await;
        assert_eq!(lines.len(), 1);
        let v = parse_line(&lines, 0);
        assert_eq!(v["error"]["code"], INVALID_REQUEST);
    }

    #[tokio::test]
    async fn batch_with_only_notifications_produces_no_output() {
        let input = r#"[{"jsonrpc":"2.0","method":"note1","params":{}},{"jsonrpc":"2.0","method":"note2","params":{}}]"#;
        let lines = run_server(input).await;
        assert!(lines.is_empty());
    }

    #[tokio::test]
    async fn batch_empty_array_is_error() {
        let lines = run_server("[]").await;
        assert_eq!(lines.len(), 1);
        let v = parse_line(&lines, 0);
        assert_eq!(v["error"]["code"], INVALID_REQUEST);
    }

    #[tokio::test]
    async fn batch_malformed_json_is_parse_error() {
        let lines = run_server("[not json]").await;
        assert_eq!(lines.len(), 1);
        let v = parse_line(&lines, 0);
        assert_eq!(v["error"]["code"], PARSE_ERROR);
    }

    #[tokio::test]
    async fn batch_with_mixed_notifications_and_requests() {
        let input = "[{\"jsonrpc\":\"2.0\",\"method\":\"note\",\"params\":{}},{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":1}}]";
        let lines = run_server(input).await;
        assert_eq!(lines.len(), 1);
        let v: Value = serde_json::from_str(&lines[0]).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1, "only the request gets a response");
        assert_eq!(arr[0]["id"], 1);
    }

    #[tokio::test]
    async fn initialize_with_numeric_id_string() {
        let lines = run_server(
            r#"{"jsonrpc":"2.0","id":"abc-123","method":"initialize","params":{"protocolVersion":1}}"#,
        )
        .await;
        assert_eq!(lines.len(), 1);
        let v = parse_line(&lines, 0);
        assert_eq!(v["id"], "abc-123");
        assert!(v["result"].is_object());
    }

    #[tokio::test]
    async fn invalid_jsonrpc_version_is_invalid_request() {
        let input =
            r#"{"jsonrpc":"1.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#;
        let lines = run_server(input).await;
        assert_eq!(lines.len(), 1);
        let v = parse_line(&lines, 0);
        assert_eq!(v["error"]["code"], INVALID_REQUEST);
        let msg = v["error"]["message"].as_str().unwrap();
        assert!(msg.contains("1.0"), "message must mention bad version");
    }

    #[tokio::test]
    async fn notifications_initialized_is_silent() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
        );
        let lines = run_server(input).await;
        assert_eq!(lines.len(), 1);
        let v = parse_line(&lines, 0);
        assert_eq!(v["id"], 1);
    }

    #[tokio::test]
    async fn large_payload_does_not_crash() {
        let big = "x".repeat(10_000);
        let input = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":1,"big":"{}"}}}}"#,
            big
        );
        let lines = run_server(&input).await;
        assert_eq!(lines.len(), 1);
        let v = parse_line(&lines, 0);
        assert!(
            v["result"].is_object(),
            "server must return valid initialize result"
        );
    }

    // ══════════════════════════════════════════════════════════════════
    // Missing jsonrpc field → ParseError (-32700)
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn missing_jsonrpc_field_returns_parse_error() {
        let lines = run_server(r#"{"id":1,"method":"initialize"}"#).await;
        assert_eq!(lines.len(), 1);
        let v = parse_line(&lines, 0);
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["error"]["code"], PARSE_ERROR);
        let msg = v["error"]["message"].as_str().unwrap();
        assert!(
            msg.contains("jsonrpc"),
            "message must mention jsonrpc: {msg}"
        );
    }
}
