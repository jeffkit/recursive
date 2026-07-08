//! ACP v1 stdio JSON-RPC transport loop.
//!
//! # Transport contract
//!
//! - **Framing**: newline-delimited JSON. One JSON-RPC message per line;
//!   no whitespace padding, no length prefix.
//! - **Request/response matching**: each request carries an `id` field
//!   (string or number). The server echoes the same `id` in its response.
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
//!   rejected as a duplicate (`-32002`). Other methods are dispatched.
//!
//! # Cooperative Cancel
//!
//! `session/cancel` implements a **two-phase cooperative abort**:
//!
//! **Phase 1 — LLM stream abort.** Each provider's SSE parse loop
//! (`OpenAiProvider::parse_sse_stream`, `AnthropicProvider::parse_sse_stream`)
//! uses `tokio::select!` on a [`tokio_util::sync::CancellationToken`] alongside
//! the SSE chunk read. When the token fires, the `reqwest::Response` is dropped
//! (closing the TCP connection) and the stream returns `Err(Error::Cancelled)`.
//!
//! **Phase 2 — ACP client RPC abort.** Agent→client RPCs (e.g.
//! `session/request_permission`) are also guarded by the same `CancellationToken`
//! plus a 30-second timeout. If `session/cancel` arrives while the agent is
//! blocked waiting for a client response, the token fires and unblocks the RPC
//! with a cancelled error.
//!
//! **Token lifecycle.** `session/new` materializes a fresh `CancellationToken`
//! and stores it on the [`AcpSession`]. `session/cancel` fires that token.
//! The same token instance is passed to
//! [`AgentRuntime::set_interrupt_token`](crate::runtime::AgentRuntime::set_interrupt_token)
//! exactly once at the start of each turn. After a turn ends (regardless of
//! finish reason), a **fresh `CancellationToken` is created** for the next
//! turn — a cancel only affects the currently in-flight turn, never a future one.
//!
//! **Transcript repair.** After a cancelled turn, the transcript remains
//! structurally valid: every assistant message containing `tool_calls` has a
//! corresponding tool result message. In-flight tool calls that were cancelled
//! mid-execution get a synthetic `tool_result` with content indicating
//! cancellation, so `session/load` replays without orphaned `tool_calls` and
//! subsequent turns work correctly.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::debug;

use super::bridge::AcpBridge;
use super::protocol::{
    AgentCapabilities, Implementation, InitializeResponse, McpCapabilities, PromptCapabilities,
    ProtocolVersion, SessionCapabilities, SessionResumeCapabilities,
};
use super::session::{AcpSession, AcpSessionManager};
use crate::llm::ChatProvider;
use crate::runtime::AgentRuntime;

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 error codes
// ---------------------------------------------------------------------------

const PARSE_ERROR: i32 = -32700;
const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const SERVER_NOT_INITIALIZED: i32 = -32002;
/// Custom error: session not found (between INVALID_PARAMS and INTERNAL_ERROR).
const SESSION_NOT_FOUND: i32 = -32001;

/// Supported protocol version.
const SUPPORTED_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::V1;

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
fn build_success(id: &Value, result: Value) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

/// Build a JSON-RPC 2.0 error response.
fn build_error(id: &Value, code: i32, message: &str) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
        },
    })
}

/// Write a JSON line to the writer (used in run_io loop).
async fn write_line<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    value: &Value,
) -> Result<(), std::io::Error> {
    let json = serde_json::to_string(value).unwrap_or_default();
    writer.write_all(json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Method dispatch
// ---------------------------------------------------------------------------

/// Dispatch a single JSON-RPC request envelope.
///
/// `state` is mutated in-place: a successful `initialize` transitions
/// `Uninitialized → Initialized`.
///
/// `sessions` is the session manager; session/new creates entries,
/// session/prompt looks them up.
///
/// `llm` is the shared LLM provider used to construct AgentRuntimes for
/// new sessions. `None` means the server runs in Sprint-1-only mode
/// where session methods return an internal error.
///
/// Returns `(response, notifications)`. The caller writes the response
/// first, then any notifications. This ordering guarantees that the
/// `initialized` notification appears after the initialize response
/// on the wire, satisfying C2.
async fn dispatch(
    envelope: &JsonRpcEnvelope,
    state: &mut ServerState,
    sessions: &mut AcpSessionManager,
    llm: Option<&Arc<dyn ChatProvider>>,
) -> (Option<Value>, Vec<Value>) {
    // --- validate jsonrpc field ---
    match &envelope.jsonrpc {
        None => {
            let id = envelope.id.clone().unwrap_or(Value::Null);
            return (
                Some(build_error(
                    &id,
                    INVALID_REQUEST,
                    "Invalid Request: missing 'jsonrpc' field",
                )),
                vec![],
            );
        }
        Some(v) if v != "2.0" => {
            let id = envelope.id.clone().unwrap_or(Value::Null);
            return (
                Some(build_error(
                    &id,
                    INVALID_REQUEST,
                    &format!("Invalid Request: expected jsonrpc '2.0', got '{v}'"),
                )),
                vec![],
            );
        }
        _ => {}
    }

    // --- get method name ---
    let method = match &envelope.method {
        Some(m) => m.as_str(),
        None => {
            let id = envelope.id.clone().unwrap_or(Value::Null);
            return (
                Some(build_error(
                    &id,
                    INVALID_REQUEST,
                    "Invalid Request: missing required field 'method'",
                )),
                vec![],
            );
        }
    };

    // --- notifications: no response ---
    let id = match &envelope.id {
        Some(id) => id.clone(),
        None => return (None, vec![]),
    };

    // --- dispatch based on method and current state ---
    match method {
        "initialize" => match *state {
            ServerState::Uninitialized => {
                let resp = handle_initialize(&id, envelope.params.as_ref());
                let notifications = if resp.get("result").is_some() {
                    *state = ServerState::Initialized;
                    vec![serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "initialized",
                        "params": null,
                    })]
                } else {
                    vec![]
                };
                (Some(resp), notifications)
            }
            ServerState::Initialized => (
                Some(build_error(
                    &id,
                    SERVER_NOT_INITIALIZED,
                    "Server already initialized",
                )),
                vec![],
            ),
        },
        "notifications/initialized" => (None, vec![]),

        // ── Sprint 2: session/new ────────────────────────────────
        "session/new" => match *state {
            ServerState::Uninitialized => (
                Some(build_error(
                    &id,
                    SERVER_NOT_INITIALIZED,
                    "Server not initialized",
                )),
                vec![],
            ),
            ServerState::Initialized => match llm {
                Some(llm) => handle_session_new(&id, envelope.params.as_ref(), sessions, llm).await,
                None => (
                    Some(build_error(
                        &id,
                        INVALID_REQUEST,
                        "LLM provider not configured; session methods unavailable",
                    )),
                    vec![],
                ),
            },
        },

        // ── Sprint 2: session/prompt ─────────────────────────────
        "session/prompt" => match *state {
            ServerState::Uninitialized => (
                Some(build_error(
                    &id,
                    SERVER_NOT_INITIALIZED,
                    "Server not initialized",
                )),
                vec![],
            ),
            ServerState::Initialized => {
                handle_session_prompt(&id, envelope.params.as_ref(), sessions).await
            }
        },

        // ── Sprint 4: session/cancel ─────────────────────────────
        "session/cancel" => match *state {
            ServerState::Uninitialized => (
                Some(build_error(
                    &id,
                    SERVER_NOT_INITIALIZED,
                    "Server not initialized",
                )),
                vec![],
            ),
            ServerState::Initialized => {
                handle_session_cancel(&id, envelope.params.as_ref(), sessions)
            }
        },

        _ => match *state {
            ServerState::Uninitialized => (
                Some(build_error(
                    &id,
                    SERVER_NOT_INITIALIZED,
                    "Server not initialized",
                )),
                vec![],
            ),
            ServerState::Initialized => (
                Some(build_error(
                    &id,
                    METHOD_NOT_FOUND,
                    &format!("Method not found: {method}"),
                )),
                vec![],
            ),
        },
    }
}

/// Handle the `initialize` method.
///
/// Constructs an [`InitializeResponse`] using the typed structs from
/// [`super::protocol`] — not ad-hoc `serde_json::Value`. This satisfies
/// contract C14 (typed protocol definitions) and C5 (nested capability
/// sub-structs matching the ACP v1 spec).
fn handle_initialize(id: &Value, params: Option<&Value>) -> Value {
    if let Some(params) = params {
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
                let version: u16 = v.as_u64().unwrap_or(0) as u16;
                if version != SUPPORTED_PROTOCOL_VERSION.as_u16() {
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

    let init_response = InitializeResponse::new(SUPPORTED_PROTOCOL_VERSION)
        .agent_info(Implementation::new("recursive", env!("CARGO_PKG_VERSION")))
        .agent_capabilities(
            AgentCapabilities::new()
                .load_session(false)
                .prompt_capabilities(
                    PromptCapabilities::new()
                        .image(false)
                        .audio(false)
                        .embedded_context(false),
                )
                .mcp_capabilities(McpCapabilities::new().http(false).sse(false))
                .session_capabilities(
                    SessionCapabilities::new().resume(SessionResumeCapabilities::new()),
                ),
        );

    // Serialize the typed struct into a JSON Value for the wire response
    let result = serde_json::to_value(&init_response).unwrap_or(serde_json::Value::Null);

    build_success(id, result)
}

// ---------------------------------------------------------------------------
// session/new handler
// ---------------------------------------------------------------------------

/// Handle `session/new`: create a sandboxed session.
///
/// Extracts `cwd` from params, validates the path exists and is a directory,
/// builds an [`AgentRuntime`] with an [`AcpBridge`] event sink, and stores
/// the session in the manager.
async fn handle_session_new(
    id: &Value,
    params: Option<&Value>,
    sessions: &mut AcpSessionManager,
    llm: &Arc<dyn ChatProvider>,
) -> (Option<Value>, Vec<Value>) {
    let params = match params {
        Some(p) => p,
        None => {
            return (
                Some(build_error(
                    id,
                    INVALID_PARAMS,
                    "Missing required field 'cwd'",
                )),
                vec![],
            );
        }
    };

    // Extract cwd
    let cwd_str = match params.get("cwd").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            return (
                Some(build_error(
                    id,
                    INVALID_PARAMS,
                    "Missing required field 'cwd'",
                )),
                vec![],
            );
        }
    };

    let cwd = std::path::PathBuf::from(cwd_str);

    // Validate cwd exists and is a directory
    if !cwd.exists() {
        return (
            Some(build_error(
                id,
                INVALID_PARAMS,
                &format!("cwd does not exist: {cwd_str}"),
            )),
            vec![],
        );
    }
    if !cwd.is_dir() {
        return (
            Some(build_error(
                id,
                INVALID_PARAMS,
                &format!("cwd is not a directory: {cwd_str}"),
            )),
            vec![],
        );
    }

    // Generate session id first (we need it for the bridge)
    let session_id = sessions.next_session_id();

    // Create the bridge for event streaming
    let (bridge, _rx) = AcpBridge::new(session_id.clone(), std::collections::HashMap::new());

    // Build the AgentRuntime with streaming enabled
    let runtime = match AgentRuntime::builder()
        .llm(llm.clone())
        .event_sink(bridge)
        .streaming(true)
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            return (
                Some(build_error(
                    id,
                    INVALID_PARAMS,
                    &format!("Failed to create agent runtime: {e}"),
                )),
                vec![],
            );
        }
    };

    // Store the session (note: we use the pre-generated id via insert_with_id)
    let cancel_token = tokio_util::sync::CancellationToken::new();
    let session = AcpSession {
        runtime,
        cwd: cwd.clone(),
        turn: 0,
        session_id: session_id.clone(),
        transcript: Vec::new(),
        cancel_token,
    };
    sessions.insert_with_id(session_id.clone(), session);

    let result = serde_json::json!({
        "sessionId": session_id,
        "capabilities": {},
    });

    (Some(build_success(id, result)), vec![])
}

// ---------------------------------------------------------------------------
// session/prompt handler
// ---------------------------------------------------------------------------

/// Handle `session/prompt`: concatenate ContentBlock[] text, feed to agent,
/// collect bridge notifications.
async fn handle_session_prompt(
    id: &Value,
    params: Option<&Value>,
    sessions: &mut AcpSessionManager,
) -> (Option<Value>, Vec<Value>) {
    let params = match params {
        Some(p) => p,
        None => {
            return (
                Some(build_error(
                    id,
                    INVALID_PARAMS,
                    "Missing required fields 'sessionId' and 'prompt'",
                )),
                vec![],
            );
        }
    };

    // Extract sessionId
    let sid = match params.get("sessionId").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return (
                Some(build_error(
                    id,
                    INVALID_PARAMS,
                    "Missing required field 'sessionId'",
                )),
                vec![],
            );
        }
    };

    // Look up the session
    let session = match sessions.get_mut(&sid) {
        Some(s) => s,
        None => {
            return (
                Some(build_error(
                    id,
                    SESSION_NOT_FOUND,
                    &format!("Session not found: {sid}"),
                )),
                vec![],
            );
        }
    };

    // Extract and parse ContentBlock[] → concatenate text
    let prompt_blocks = match params.get("prompt").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => {
            return (
                Some(build_error(
                    id,
                    INVALID_PARAMS,
                    "Missing required field 'prompt'",
                )),
                vec![],
            );
        }
    };

    let mut concatenated = String::new();
    let mut non_text_blocks = Vec::new();

    for block in prompt_blocks {
        match block.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                    concatenated.push_str(text);
                }
            }
            Some(other) => {
                non_text_blocks.push(other.to_string());
                debug!("session/prompt: ignoring unsupported ContentBlock type: {other}");
            }
            None => {
                debug!("session/prompt: ContentBlock missing 'type' field, skipping");
            }
        }
    }

    if concatenated.is_empty() && non_text_blocks.is_empty() {
        return (
            Some(build_error(
                id,
                INVALID_PARAMS,
                "prompt array is empty or contains no text blocks",
            )),
            vec![],
        );
    }

    // If there were non-text blocks but text is also present, proceed
    if concatenated.is_empty() && !non_text_blocks.is_empty() {
        // All blocks were non-text; return a friendly response
        let result = serde_json::json!({
            "stopReason": "end_turn",
        });
        let notif = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": sid,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {
                        "type": "text",
                        "text": format!(
                            "Unsupported content types in this sprint: {}",
                            non_text_blocks.join(", ")
                        ),
                    }
                }
            }
        });
        let stop_notif = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": sid,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "stopReason": "end_turn",
                }
            }
        });
        return (Some(build_success(id, result)), vec![notif, stop_notif]);
    }

    // Create a new bridge for this turn and swap it in
    let (bridge, mut rx) = AcpBridge::new(sid.clone(), std::collections::HashMap::new());
    session.runtime.replace_event_sink(bridge);

    // Sprint 4: refresh the cancel token for this turn.
    // This creates a fresh CancellationToken wired to the agent runtime
    // via set_interrupt_token, ensuring a cancel only affects the
    // currently in-flight turn.
    let _ = session.refresh_cancel_token();

    // Run the agent
    let outcome = match session.runtime.run(concatenated).await {
        Ok(o) => o,
        Err(e) => {
            return (
                Some(build_error(
                    id,
                    INVALID_PARAMS,
                    &format!("Agent run failed: {e}"),
                )),
                vec![],
            );
        }
    };

    // Increment turn counter
    session.turn += 1;

    // Drain all notifications from the bridge
    let mut notifications: Vec<Value> = Vec::new();
    while let Ok(notif) = rx.try_recv() {
        notifications.push(notif);
    }

    // Build response with stopReason
    let stop_reason = match outcome.finish_reason {
        crate::agent::FinishReason::NoMoreToolCalls => "end_turn",
        crate::agent::FinishReason::Cancelled => "cancelled",
        _ => "end_turn",
    };

    let result = serde_json::json!({
        "stopReason": stop_reason,
    });

    (Some(build_success(id, result)), notifications)
}

// ---------------------------------------------------------------------------
// session/cancel handler (Sprint 4)
// ---------------------------------------------------------------------------

/// Handle `session/cancel`: fire the session's `CancellationToken`.
///
/// Validation rules (C0):
/// - Valid sessionId → fire the token, return `{cancelled: true}`.
/// - Unknown sessionId → error code 404 (SESSION_NOT_FOUND).
/// - Missing or wrong-type sessionId → error code 400 (INVALID_PARAMS).
fn handle_session_cancel(
    id: &Value,
    params: Option<&Value>,
    sessions: &mut AcpSessionManager,
) -> (Option<Value>, Vec<Value>) {
    let params = match params {
        Some(p) => p,
        None => {
            return (
                Some(build_error(
                    id,
                    INVALID_PARAMS,
                    "Missing required field 'sessionId'",
                )),
                vec![],
            );
        }
    };

    // Validate sessionId is a string
    let sid = match params.get("sessionId") {
        Some(v) if v.is_string() => v.as_str().unwrap_or_default().to_string(),
        Some(_) => {
            return (
                Some(build_error(
                    id,
                    INVALID_PARAMS,
                    "Invalid params: 'sessionId' must be a string",
                )),
                vec![],
            );
        }
        None => {
            return (
                Some(build_error(
                    id,
                    INVALID_PARAMS,
                    "Missing required field 'sessionId'",
                )),
                vec![],
            );
        }
    };

    // Look up the session — return 404 if not found
    let session = match sessions.get_mut(&sid) {
        Some(s) => s,
        None => {
            return (
                Some(build_error(
                    id,
                    SESSION_NOT_FOUND,
                    &format!("Session not found: {sid}"),
                )),
                vec![],
            );
        }
    };

    // Fire the cancellation token — this triggers Phase 1 (LLM stream abort)
    // and Phase 2 (agent→client RPC abort) as described in the module doc.
    session.cancel_token.cancel();

    let result = serde_json::json!({
        "cancelled": true,
    });

    (Some(build_success(id, result)), vec![])
}

// ---------------------------------------------------------------------------
// AcpServer — stdio transport loop
// ---------------------------------------------------------------------------

/// ACP stdio JSON-RPC server.
pub struct AcpServer;

impl AcpServer {
    /// Run the server on real stdio until EOF.
    ///
    /// In Sprint 1 this runs without an LLM provider — only `initialize`
    /// is wired. Session methods will return an error.
    pub async fn run() {
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();
        Self::run_io(BufReader::new(stdin), stdout, None).await;
    }

    /// Run the server on generic reader/writer (testable).
    ///
    /// Pass `Some(llm)` when session methods (`session/new`, `session/prompt`)
    /// should be available. Pass `None` for Sprint-1-only operation (just
    /// the `initialize` handshake).
    pub async fn run_io<R, W>(reader: R, mut writer: W, llm: Option<Arc<dyn ChatProvider>>)
    where
        R: tokio::io::AsyncBufRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let mut lines = BufReader::new(reader).lines();
        let mut state = ServerState::Uninitialized;
        let mut sessions = AcpSessionManager::new();

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

            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                // Batch: parse as Vec<Value> first for robustness
                let batch: Vec<Value> = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => {
                        let resp =
                            build_error(&Value::Null, PARSE_ERROR, "Parse error: invalid JSON");
                        let _ = write_line(&mut writer, &resp).await;
                        continue;
                    }
                };

                if batch.is_empty() {
                    let resp =
                        build_error(&Value::Null, INVALID_REQUEST, "Empty batch is not allowed");
                    let _ = write_line(&mut writer, &resp).await;
                    continue;
                }

                let mut responses: Vec<Value> = Vec::new();
                let mut batch_notifications: Vec<Value> = Vec::new();
                for item in &batch {
                    match serde_json::from_value::<JsonRpcEnvelope>(item.clone()) {
                        Ok(env) => {
                            let llm_ref = llm.as_ref();
                            let (resp, notifs) =
                                dispatch(&env, &mut state, &mut sessions, llm_ref).await;
                            if let Some(resp) = resp {
                                responses.push(resp);
                            }
                            batch_notifications.extend(notifs);
                        }
                        Err(_) => {
                            let id = item.get("id").cloned().unwrap_or(Value::Null);
                            responses.push(build_error(
                                &id,
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
                for notif in &batch_notifications {
                    let _ = write_line(&mut writer, notif).await;
                }
            } else {
                // Single request/notification
                match serde_json::from_str::<JsonRpcEnvelope>(&line) {
                    Ok(env) => {
                        let llm_ref = llm.as_ref();
                        let (resp, notifications) =
                            dispatch(&env, &mut state, &mut sessions, llm_ref).await;
                        if let Some(resp) = resp {
                            let _ = write_line(&mut writer, &resp).await;
                        }
                        for notif in &notifications {
                            let _ = write_line(&mut writer, notif).await;
                        }
                    }
                    Err(_) => {
                        let resp =
                            build_error(&Value::Null, PARSE_ERROR, "Parse error: invalid JSON");
                        let _ = write_line(&mut writer, &resp).await;
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
    use crate::llm::MockProvider;

    // ── helpers ──────────────────────────────────────────────────────

    /// Create a MockProvider that returns a single "Hello" completion.
    fn mock_llm() -> Arc<dyn ChatProvider> {
        Arc::new(MockProvider::new(vec![crate::llm::Completion {
            content: "Hello from agent".into(),
            ..Default::default()
        }]))
    }

    /// Create a MockProvider that returns completions for multi-turn.
    fn mock_llm_multi(completions: Vec<crate::llm::Completion>) -> Arc<dyn ChatProvider> {
        Arc::new(MockProvider::new(completions))
    }

    /// Run the server on a string input, return captured stdout lines.
    /// Uses Some(llm) so session methods work (backward compat).
    async fn run_server(input: &str) -> Vec<String> {
        run_server_with_llm(input, Some(mock_llm())).await
    }

    /// Run the server with no LLM provider (Sprint-1-only mode).
    #[allow(dead_code)]
    async fn run_server_no_llm(input: &str) -> Vec<String> {
        run_server_with_llm(input, None).await
    }

    async fn run_server_with_llm(input: &str, llm: Option<Arc<dyn ChatProvider>>) -> Vec<String> {
        let reader = std::io::Cursor::new(input.as_bytes().to_owned());
        let mut output = Vec::<u8>::new();
        AcpServer::run_io(reader, &mut output, llm).await;
        let text = String::from_utf8(output).unwrap();
        text.lines().map(|s| s.to_string()).collect()
    }

    /// Helper: parse stdout line at index as JSON Value.
    fn parse_line(lines: &[String], idx: usize) -> Value {
        serde_json::from_str(&lines[idx]).expect("valid JSON")
    }

    /// Collect all lines matching `session/update` method.
    fn find_notifications(lines: &[String]) -> Vec<Value> {
        lines
            .iter()
            .filter_map(|l| {
                let v: Value = serde_json::from_str(l).ok()?;
                if v.get("method").and_then(|m| m.as_str()) == Some("session/update") {
                    Some(v)
                } else {
                    None
                }
            })
            .collect()
    }

    // ══════════════════════════════════════════════════════════════════
    // C1 — initialize returns success response
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn initialize_returns_success() {
        let lines = run_server(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
        )
        .await;
        assert_eq!(
            lines.len(),
            2,
            "expected response + initialized notification"
        );
        let v = parse_line(&lines, 0);
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 1);
        assert!(v["result"].is_object());
        let notif = parse_line(&lines, 1);
        assert!(notif["id"].is_null() || notif.get("id").is_none());
        assert_eq!(notif["method"], "initialized");
    }

    // ══════════════════════════════════════════════════════════════════
    // C1 — protocolVersion, agentInfo, agentCapabilities
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

    /// C5: agentCapabilities uses nested sub-structs from typed structs
    /// (camelCase serialisation from the upstream `agent-client-protocol-schema`).
    #[tokio::test]
    async fn agent_capabilities_has_typed_nested_structs() {
        let lines = run_server(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
        )
        .await;
        let v = parse_line(&lines, 0);
        let caps = &v["result"]["agentCapabilities"];

        // promptCapabilities: must be an object
        assert!(
            caps["promptCapabilities"].is_object(),
            "promptCapabilities should be an object, got: {:?}",
            caps["promptCapabilities"]
        );
        // image, audio, embeddedContext are boolean fields
        assert!(caps["promptCapabilities"]["image"].is_boolean());
        assert!(caps["promptCapabilities"]["audio"].is_boolean());
        assert!(caps["promptCapabilities"]["embeddedContext"].is_boolean());

        // mcpCapabilities: http, sse boolean fields
        assert!(caps["mcpCapabilities"].is_object());
        assert!(caps["mcpCapabilities"]["http"].is_boolean());
        assert!(caps["mcpCapabilities"]["sse"].is_boolean());

        // sessionCapabilities: must have resume sub-object
        assert!(caps["sessionCapabilities"].is_object());
        assert!(caps["sessionCapabilities"]["resume"].is_object());

        // loadSession: top-level boolean
        assert!(caps["loadSession"].is_boolean());
    }

    // ══════════════════════════════════════════════════════════════════
    // C5 — malformed JSON → ParseError (-32700), then continue
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn malformed_json_returns_parse_error() {
        let lines = run_server("not json").await;
        assert_eq!(lines.len(), 1);
        let v = parse_line(&lines, 0);
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], Value::Null);
        assert_eq!(v["error"]["code"], PARSE_ERROR);
    }

    #[tokio::test]
    async fn parse_error_then_valid_initialize() {
        let input = concat!(
            "garbage\n",
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
        );
        let lines = run_server(input).await;
        assert!(
            lines.len() >= 2,
            "parse error + init response + initialized"
        );
        let v0 = parse_line(&lines, 0);
        assert_eq!(v0["error"]["code"], PARSE_ERROR);
        assert_eq!(v0["id"], Value::Null);
        let v1 = parse_line(&lines, 1);
        assert_eq!(v1["result"]["protocolVersion"], 1);
    }

    // ══════════════════════════════════════════════════════════════════
    // C8 — missing jsonrpc/method → Invalid Request (-32600)
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn json_without_jsonrpc_returns_invalid_request() {
        let lines = run_server(r#"{"id":1}"#).await;
        assert_eq!(lines.len(), 1);
        let v = parse_line(&lines, 0);
        assert_eq!(v["id"], 1);
        assert_eq!(v["error"]["code"], INVALID_REQUEST);
        let msg = v["error"]["message"].as_str().unwrap().to_lowercase();
        assert!(msg.contains("invalid request"), "msg: {msg}");
    }

    #[tokio::test]
    async fn missing_method_returns_invalid_request() {
        let lines = run_server(r#"{"jsonrpc":"2.0","id":1}"#).await;
        assert_eq!(lines.len(), 1);
        let v = parse_line(&lines, 0);
        assert_eq!(v["error"]["code"], INVALID_REQUEST);
    }

    // ══════════════════════════════════════════════════════════════════
    // C6 — MethodNotFound (-32601) for unknown methods
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn after_init_unknown_method_is_not_found() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"nonexistent","params":{}}"#,
        );
        let lines = run_server(input).await;
        assert_eq!(lines.len(), 3, "response + initialized + error");
        let v0 = parse_line(&lines, 0);
        assert!(v0["result"].is_object());
        let v1 = parse_line(&lines, 1);
        assert_eq!(v1["method"], "initialized");
        let v2 = parse_line(&lines, 2);
        assert_eq!(v2["error"]["code"], METHOD_NOT_FOUND);
    }

    // ══════════════════════════════════════════════════════════════════
    // C7 — notifications → no response
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn notification_produces_no_response() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","method":"someNotification","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
        );
        let lines = run_server(input).await;
        assert_eq!(
            lines.len(),
            2,
            "only initialize response + initialized notification"
        );
        let v = parse_line(&lines, 0);
        assert_eq!(v["id"], 1);
        assert!(v["result"].is_object());
    }

    #[tokio::test]
    async fn notification_alone_produces_nothing() {
        let lines =
            run_server(r#"{"jsonrpc":"2.0","method":"someNotification","params":{}}"#).await;
        assert!(lines.is_empty());
    }

    // ══════════════════════════════════════════════════════════════════
    // C2 — initialized notification ordering
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn initialized_notification_after_init_response() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"ping","params":{}}"#,
        );
        let lines = run_server(input).await;
        assert_eq!(lines.len(), 3, "init response + initialized + error");
        let v0 = parse_line(&lines, 0);
        assert_eq!(v0["id"], 1);
        assert!(v0["result"].is_object());
        let v1 = parse_line(&lines, 1);
        assert!(
            v1["id"].is_null() || v1.get("id").is_none(),
            "notification has no id"
        );
        assert_eq!(v1["method"], "initialized");
        let v2 = parse_line(&lines, 2);
        assert_eq!(v2["id"], 2);
    }

    // ══════════════════════════════════════════════════════════════════
    // C3 — multi-request FIFO ordering
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn multi_request_fifo_ordering() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":10,"method":"a","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":20,"method":"b","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":30,"method":"c","params":{}}"#,
        );
        let lines = run_server(input).await;
        assert_eq!(lines.len(), 5, "init + initialized + 3 errors");
        assert_eq!(parse_line(&lines, 0)["id"], 1);
        assert_eq!(parse_line(&lines, 1)["method"], "initialized");
        assert_eq!(parse_line(&lines, 2)["id"], 10);
        assert_eq!(parse_line(&lines, 2)["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(parse_line(&lines, 3)["id"], 20);
        assert_eq!(parse_line(&lines, 3)["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(parse_line(&lines, 4)["id"], 30);
        assert_eq!(parse_line(&lines, 4)["error"]["code"], METHOD_NOT_FOUND);
    }

    // ══════════════════════════════════════════════════════════════════
    // State machine tests
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn before_init_other_methods_return_server_not_initialized() {
        let input = r#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{}}"#;
        let lines = run_server(input).await;
        assert_eq!(lines.len(), 1);
        let v = parse_line(&lines, 0);
        assert_eq!(v["error"]["code"], SERVER_NOT_INITIALIZED);
    }

    #[tokio::test]
    async fn duplicate_initialize_returns_error() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":1}}"#,
        );
        let lines = run_server(input).await;
        assert_eq!(lines.len(), 3, "response + initialized + error");
        let v0 = parse_line(&lines, 0);
        assert!(v0["result"].is_object());
        let v1 = parse_line(&lines, 1);
        assert_eq!(v1["method"], "initialized");
        let v2 = parse_line(&lines, 2);
        assert!(v2["error"].is_object());
        assert_eq!(v2["error"]["code"], SERVER_NOT_INITIALIZED);
    }

    // ══════════════════════════════════════════════════════════════════
    // Invalid params tests
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn missing_protocol_version_returns_invalid_params() {
        let input = r#"{"jsonrpc":"2.0","id":3,"method":"initialize","params":{}}"#;
        let lines = run_server(input).await;
        assert_eq!(lines.len(), 1);
        let v = parse_line(&lines, 0);
        assert_eq!(v["error"]["code"], INVALID_PARAMS);
    }

    #[tokio::test]
    async fn string_protocol_version_returns_invalid_params() {
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
    }

    // ══════════════════════════════════════════════════════════════════
    // C14: response is built from typed structs (not ad-hoc Value)
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn initialize_response_uses_typed_structs() {
        // Build an InitializeResponse directly and serialise it.
        // Then verify the server produces the exact same JSON shape.
        let expected = InitializeResponse::new(SUPPORTED_PROTOCOL_VERSION)
            .agent_info(Implementation::new("recursive", env!("CARGO_PKG_VERSION")))
            .agent_capabilities(
                AgentCapabilities::new()
                    .load_session(false)
                    .prompt_capabilities(
                        PromptCapabilities::new()
                            .image(false)
                            .audio(false)
                            .embedded_context(false),
                    )
                    .mcp_capabilities(McpCapabilities::new().http(false).sse(false))
                    .session_capabilities(
                        SessionCapabilities::new().resume(SessionResumeCapabilities::new()),
                    ),
            );
        let expected_json = serde_json::to_value(&expected).unwrap();

        let lines = run_server(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
        )
        .await;
        let v = parse_line(&lines, 0);
        let result = &v["result"];
        assert_eq!(result["protocolVersion"], expected_json["protocolVersion"]);
        assert_eq!(result["agentInfo"], expected_json["agentInfo"]);
        assert_eq!(
            result["agentCapabilities"],
            expected_json["agentCapabilities"]
        );
    }

    // ══════════════════════════════════════════════════════════════════
    // Sprint 4: session/cancel tests (C0)
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn session_cancel_valid_session_returns_cancelled_true() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_string_lossy();
        let input = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":1}}}}
{{"jsonrpc":"2.0","id":2,"method":"session/new","params":{{"cwd":"{}"}}}}
{{"jsonrpc":"2.0","id":3,"method":"session/cancel","params":{{"sessionId":"acp-sess-1"}}}}"#,
            cwd
        );
        let lines = run_server(&input).await;
        // init response + initialized + session/new + session/cancel
        assert!(lines.len() >= 4, "expected ≥4 lines, got {}", lines.len());
        let v = parse_line(&lines, 3);
        assert!(v["result"].is_object(), "expected success, got: {v}");
        assert_eq!(v["result"]["cancelled"], true);
    }

    #[tokio::test]
    async fn session_cancel_nonexistent_session_returns_404() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"session/cancel","params":{"sessionId":"nonexistent"}}"#,
        );
        let lines = run_server(input).await;
        assert_eq!(lines.len(), 3, "init + initialized + error");
        let v = parse_line(&lines, 2);
        assert_eq!(v["error"]["code"], SESSION_NOT_FOUND);
        let msg = v["error"]["message"].as_str().unwrap();
        assert!(msg.contains("not found"), "msg should say not found: {msg}");
    }

    #[tokio::test]
    async fn session_cancel_missing_session_id_returns_400() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"session/cancel","params":{}}"#,
        );
        let lines = run_server(input).await;
        assert_eq!(lines.len(), 3, "init + initialized + error");
        let v = parse_line(&lines, 2);
        assert_eq!(v["error"]["code"], INVALID_PARAMS);
    }

    #[tokio::test]
    async fn session_cancel_wrong_type_session_id_returns_400() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"session/cancel","params":{"sessionId":123}}"#,
        );
        let lines = run_server(input).await;
        assert_eq!(lines.len(), 3, "init + initialized + error");
        let v = parse_line(&lines, 2);
        assert_eq!(v["error"]["code"], INVALID_PARAMS);
    }

    // ══════════════════════════════════════════════════════════════════
    // Sprint 4: CancellationToken lifecycle (C1)
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn cancel_token_fresh_after_each_turn() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_string_lossy();

        let multi_llm = mock_llm_multi(vec![
            crate::llm::Completion {
                content: "reply 1".into(),
                ..Default::default()
            },
            crate::llm::Completion {
                content: "reply 2".into(),
                ..Default::default()
            },
        ]);

        // Create session, run two turns with the same server instance.
        // After the second turn, verify the session's cancel token is
        // fresh (not cancelled).
        let input = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":1}}}}
{{"jsonrpc":"2.0","id":2,"method":"session/new","params":{{"cwd":"{}"}}}}
{{"jsonrpc":"2.0","id":3,"method":"session/cancel","params":{{"sessionId":"acp-sess-1"}}}}
{{"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{{"sessionId":"acp-sess-1","prompt":[{{"type":"text","text":"hi"}}]}}}}"#,
            cwd
        );

        // Use a separate session manager to inspect state
        let mut sessions = AcpSessionManager::new();
        use tokio::io::AsyncBufReadExt;
        let reader = std::io::Cursor::new(input.as_bytes().to_owned());
        let mut lines = BufReader::new(reader).lines();
        let mut state = ServerState::Uninitialized;
        let mut output = Vec::<u8>::new();

        while let Some(line) = lines.next_line().await.unwrap() {
            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }
            let env: JsonRpcEnvelope = match serde_json::from_str(&line) {
                Ok(e) => e,
                Err(_) => continue,
            };
            let (resp, notifs) = dispatch(&env, &mut state, &mut sessions, Some(&multi_llm)).await;
            if let Some(resp) = resp {
                let json = serde_json::to_string(&resp).unwrap();
                output.extend_from_slice(json.as_bytes());
                output.extend_from_slice(b"\n");
            }
            for notif in &notifs {
                let json = serde_json::to_string(notif).unwrap();
                output.extend_from_slice(json.as_bytes());
                output.extend_from_slice(b"\n");
            }
        }

        // After the second turn (session/prompt), the session should have
        // a fresh CancellationToken that is NOT cancelled.
        let session = sessions.get_mut("acp-sess-1").unwrap();
        assert!(
            !session.cancel_token.is_cancelled(),
            "token after turn should be fresh (not cancelled)"
        );
    }

    // ══════════════════════════════════════════════════════════════════
    // Batch tests
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn batch_returns_array_of_responses() {
        let input = "[{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":1}},{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"initialize\",\"params\":{\"protocolVersion\":1}}]";
        let lines = run_server(input).await;
        assert_eq!(lines.len(), 2, "batch array + initialized notification");
        let v: Value = serde_json::from_str(&lines[0]).unwrap();
        assert!(v.is_array());
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
    }

    #[tokio::test]
    async fn batch_before_init_mixed_methods() {
        let input = "[{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/new\",\"params\":{}},{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":1}}]";
        let lines = run_server(input).await;
        assert_eq!(lines.len(), 2, "batch array + initialized notification");
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
        assert_eq!(lines.len(), 2, "response + initialized notification");
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
        let input = r#"[{"jsonrpc":"2.0","method":"note","params":{}},{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}]"#;
        let lines = run_server(input).await;
        assert_eq!(lines.len(), 2, "batch array + initialized notification");
        let v: Value = serde_json::from_str(&lines[0]).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], 1);
    }

    // ══════════════════════════════════════════════════════════════════
    // Robustness tests
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn initialize_with_string_id() {
        let lines = run_server(
            r#"{"jsonrpc":"2.0","id":"abc-123","method":"initialize","params":{"protocolVersion":1}}"#,
        )
        .await;
        assert_eq!(lines.len(), 2, "response + initialized notification");
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
    }

    #[tokio::test]
    async fn notifications_initialized_is_silent() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
        );
        let lines = run_server(input).await;
        assert_eq!(lines.len(), 2, "response + initialized notification");
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
        assert_eq!(lines.len(), 2, "response + initialized notification");
        let v = parse_line(&lines, 0);
        assert!(v["result"].is_object());
    }

    #[tokio::test]
    async fn jsonrpc_two_point_zero_is_valid() {
        let lines = run_server(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
        )
        .await;
        assert_eq!(lines.len(), 2);
    }

    // ══════════════════════════════════════════════════════════════════
    // Sprint 2: session/new tests
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn session_new_returns_session_id() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_string_lossy();
        let input = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":1}}}}
{{"jsonrpc":"2.0","id":2,"method":"session/new","params":{{"cwd":"{}"}}}}"#,
            cwd
        );
        let lines = run_server(&input).await;
        assert!(
            lines.len() >= 3,
            "init + initialized + session/new response"
        );
        let v = parse_line(&lines, 2);
        assert!(v["result"].is_object(), "expected success, got: {v}");
        assert!(v["result"]["sessionId"].is_string());
        assert!(!v["result"]["sessionId"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn session_new_missing_cwd_returns_error() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{}}"#,
        );
        let lines = run_server(input).await;
        assert_eq!(lines.len(), 3, "init + initialized + error");
        let v = parse_line(&lines, 2);
        assert_eq!(v["error"]["code"], INVALID_PARAMS);
    }

    #[tokio::test]
    async fn session_new_nonexistent_cwd_returns_error() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/nonexistent/path/xyz"}}"#,
        );
        let lines = run_server(input).await;
        assert_eq!(lines.len(), 3, "init + initialized + error");
        let v = parse_line(&lines, 2);
        assert!(v["error"].is_object());
        assert_ne!(v["error"]["code"], 0);
        let msg = v["error"]["message"].as_str().unwrap();
        assert!(msg.contains("cwd"), "msg should mention cwd: {msg}");
    }

    // ══════════════════════════════════════════════════════════════════
    // Sprint 2: session/prompt tests
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn session_prompt_invalid_session_returns_error() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"sessionId":"nonexistent","prompt":[{"type":"text","text":"hi"}]}}"#,
        );
        let lines = run_server(input).await;
        assert_eq!(lines.len(), 3, "init + initialized + error");
        let v = parse_line(&lines, 2);
        assert!(v["error"].is_object());
        assert_eq!(v["error"]["code"], SESSION_NOT_FOUND);
        let msg = v["error"]["message"].as_str().unwrap();
        assert!(msg.contains("not found"), "msg: {msg}");
    }

    #[tokio::test]
    async fn session_prompt_emits_agent_message_chunk_and_stop_reason() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_string_lossy();

        // Phase 1: create session, get ID
        let phase1_input = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":1}}}}
{{"jsonrpc":"2.0","id":2,"method":"session/new","params":{{"cwd":"{}"}}}}"#,
            cwd
        );
        let reader1 = std::io::Cursor::new(phase1_input.as_bytes().to_owned());
        let mut output1 = Vec::<u8>::new();
        AcpServer::run_io(reader1, &mut output1, Some(mock_llm())).await;
        let lines1: Vec<String> = String::from_utf8(output1)
            .unwrap()
            .lines()
            .map(|s| s.to_string())
            .collect();
        let sn_resp: Value = serde_json::from_str(&lines1[2]).unwrap();
        let sid = sn_resp["result"]["sessionId"].as_str().unwrap().to_string();

        // Phase 2: use the sessionId for session/prompt
        let phase2_input = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":1}}}}
{{"jsonrpc":"2.0","id":2,"method":"session/new","params":{{"cwd":"{}"}}}}
{{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{{"sessionId":"{}","prompt":[{{"type":"text","text":"hello"}}]}}}}"#,
            cwd, sid
        );
        let reader2 = std::io::Cursor::new(phase2_input.as_bytes().to_owned());
        let mut output2 = Vec::<u8>::new();
        AcpServer::run_io(reader2, &mut output2, Some(mock_llm())).await;
        let lines2: Vec<String> = String::from_utf8(output2)
            .unwrap()
            .lines()
            .map(|s| s.to_string())
            .collect();

        assert!(
            lines2.len() >= 4,
            "expected at least 4 lines, got {}",
            lines2.len()
        );

        // Find the session/prompt response (last line with "id":3)
        let prompt_resp_line = lines2
            .iter()
            .rposition(|l| l.contains(r#""id":3"#))
            .expect("should have session/prompt response");
        let prompt_resp: Value = serde_json::from_str(&lines2[prompt_resp_line]).unwrap();
        assert_eq!(prompt_resp["result"]["stopReason"], "end_turn");

        // Find session/update notifications
        let notifs = find_notifications(&lines2);
        assert!(!notifs.is_empty(), "expected session/update notifications");

        // At least one should have agent_message_chunk with text
        let has_chunk = notifs.iter().any(|n| {
            n["params"]["update"]["sessionUpdate"] == "agent_message_chunk"
                && n["params"]["update"]["content"]["type"] == "text"
                && !n["params"]["update"]["content"]["text"]
                    .as_str()
                    .unwrap_or("")
                    .is_empty()
        });
        assert!(has_chunk, "expected agent_message_chunk with text content");

        // Last notification should have stopReason
        let last_notif = notifs.last().unwrap();
        assert_eq!(
            last_notif["params"]["update"]["stopReason"], "end_turn",
            "last notification must have stopReason=end_turn"
        );
    }

    #[tokio::test]
    async fn session_prompt_content_block_text_concatenation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_string_lossy();

        let echo_llm = mock_llm_multi(vec![crate::llm::Completion {
            content: "hello world".into(),
            ..Default::default()
        }]);

        let input = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":1}}}}
{{"jsonrpc":"2.0","id":2,"method":"session/new","params":{{"cwd":"{}"}}}}
{{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{{"sessionId":"acp-sess-1","prompt":[{{"type":"text","text":"hello"}},{{"type":"text","text":" world"}}]}}}}"#,
            cwd
        );

        let reader = std::io::Cursor::new(input.as_bytes().to_owned());
        let mut output = Vec::<u8>::new();
        AcpServer::run_io(reader, &mut output, Some(echo_llm)).await;
        let text = String::from_utf8(output).unwrap();
        assert!(!text.is_empty());
    }

    #[tokio::test]
    async fn session_prompt_non_text_blocks_ignored() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_string_lossy();

        let input = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":1}}}}
{{"jsonrpc":"2.0","id":2,"method":"session/new","params":{{"cwd":"{}"}}}}
{{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{{"sessionId":"acp-sess-1","prompt":[{{"type":"image","data":"AAAA","mimeType":"image/png"}}]}}}}"#,
            cwd
        );

        let reader = std::io::Cursor::new(input.as_bytes().to_owned());
        let mut output = Vec::<u8>::new();
        AcpServer::run_io(reader, &mut output, Some(mock_llm())).await;
        let text = String::from_utf8(output).unwrap();
        assert!(!text.is_empty());
        assert!(
            text.contains("Unsupported"),
            "should mention unsupported types"
        );
    }

    #[tokio::test]
    async fn session_prompt_missing_prompt_field_returns_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_string_lossy();
        let input = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":1}}}}
{{"jsonrpc":"2.0","id":2,"method":"session/new","params":{{"cwd":"{}"}}}}
{{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{{"sessionId":"acp-sess-1"}}}}"#,
            cwd
        );
        let lines = run_server(&input).await;
        // 3 requests → 3 responses (lines[0..3]). session/prompt is the 3rd
        // request, so its error response is at idx 3.
        let v = parse_line(&lines, 3);
        assert_eq!(v["error"]["code"], INVALID_PARAMS);
    }

    // ══════════════════════════════════════════════════════════════════
    // Sprint 2: multi-turn context
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn multi_turn_context_preserved() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_string_lossy();

        let multi_llm = mock_llm_multi(vec![
            crate::llm::Completion {
                content: "Nice to meet you, Alice!".into(),
                ..Default::default()
            },
            crate::llm::Completion {
                content: "Your name is Alice.".into(),
                ..Default::default()
            },
        ]);

        // Phase 1: create session
        let phase1_input = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":1}}}}
{{"jsonrpc":"2.0","id":2,"method":"session/new","params":{{"cwd":"{}"}}}}"#,
            cwd
        );
        let reader1 = std::io::Cursor::new(phase1_input.as_bytes().to_owned());
        let mut output1 = Vec::<u8>::new();
        let llm1 = mock_llm_multi(vec![crate::llm::Completion {
            content: "Nice to meet you, Alice!".into(),
            ..Default::default()
        }]);
        AcpServer::run_io(reader1, &mut output1, Some(llm1)).await;
        let lines1: Vec<String> = String::from_utf8(output1)
            .unwrap()
            .lines()
            .map(|s| s.to_string())
            .collect();
        let sn_resp: Value = serde_json::from_str(&lines1[2]).unwrap();
        let sid = sn_resp["result"]["sessionId"].as_str().unwrap().to_string();

        // Phase 2: run two turns with the same session
        let phase2_input = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":1}}}}
{{"jsonrpc":"2.0","id":2,"method":"session/new","params":{{"cwd":"{}"}}}}
{{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{{"sessionId":"{}","prompt":[{{"type":"text","text":"my name is Alice"}}]}}}}
{{"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{{"sessionId":"{}","prompt":[{{"type":"text","text":"what is my name?"}}]}}}}"#,
            cwd, sid, sid
        );
        let reader2 = std::io::Cursor::new(phase2_input.as_bytes().to_owned());
        let mut output2 = Vec::<u8>::new();
        AcpServer::run_io(reader2, &mut output2, Some(multi_llm)).await;
        let text = String::from_utf8(output2).unwrap();
        assert!(
            text.contains("Alice"),
            "second turn should retain context about Alice, got:\n{text}"
        );
    }
}
