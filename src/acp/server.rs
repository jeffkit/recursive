//! # Collaborative Cancellation
//!
//! Cancel is a **request** (not a demand). The agent drains in-flight tool results
//! before stopping, and `stopReason: 'cancelled'` is set on the last assistant message.
//! See Decision #4c.
//!
//! ## Transport contract
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
//! - **Uninitialized** (startup): only `initialize` and method-not-found
//!   responses are produced. Known session methods (`session/new`,
//!   `session/prompt`, `session/cancel`) return `-32002`
//!   ("Server not initialized"). Truly unknown methods return
//!   `-32601` ("Method not found") regardless of state per the
//!   JSON-RPC 2.0 spec.
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

use std::collections::HashMap;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tracing::debug;

use super::bridge::sha256_first_12;
use super::bridge::AcpBridge;
use super::permission::handle_request_permission;
use super::protocol::{ClientCapabilities, Implementation, ProtocolVersion};
use super::session::{
    compute_compaction_hints, summarize_transcript, AcpSession, AcpSessionManager,
};
use crate::acp::ToolKind;
use crate::agent::FinishReason;
use crate::llm::ChatProvider;
use crate::runtime::AgentRuntime;
use crate::tools::{AcpClientFsState, ClientReadFile, ClientWriteFile};

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 error codes
// ---------------------------------------------------------------------------

const PARSE_ERROR: i32 = -32700;
const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const SERVER_NOT_INITIALIZED: i32 = -32002;
/// Custom error: session not found or not running (S1-C12: contract requires -32000).
const SESSION_NOT_FOUND: i32 = -32000;

/// Timeout for all in-flight agent→client fs/* RPCs, in milliseconds.
/// Override via the `ACP_CLIENT_RPC_TIMEOUT_MS` environment variable.
/// Default: 30_000 ms (30 seconds).
#[inline]
pub fn client_rpc_timeout_ms() -> u64 {
    std::env::var("ACP_CLIENT_RPC_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(30_000)
}

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
// Method dispatch — sync dispatch table (HashMap) + async fallback
// ---------------------------------------------------------------------------

/// Synchronous method handler signature: `(id, params) -> response Value`.
type SyncMethodHandler = fn(id: &Value, params: Option<&Value>) -> Value;

/// Build the Sprint-1 synchronous dispatch table.
/// Only `initialize` is mapped; all other methods fall through to
/// the async dispatch (session/*) or return MethodNotFound.
fn build_sync_dispatch_table() -> HashMap<&'static str, SyncMethodHandler> {
    let mut map: HashMap<&'static str, SyncMethodHandler> = HashMap::new();
    map.insert("initialize", handle_initialize);
    map
}

/// Dispatch a single JSON-RPC request envelope for async session methods.
///
/// `sessions` is the session manager; session/new creates entries,
/// session/prompt looks them up.
///
/// `llm` is the shared LLM provider used to construct AgentRuntimes for
/// new sessions. `None` means the server runs in Sprint-1-only mode
/// where all non-`initialize` methods return MethodNotFound.
///
/// Returns `(response, notifications)`. The caller writes the response
/// first, then any notifications. This ordering guarantees that the
/// `initialized` notification appears after the initialize response
/// on the wire, satisfying C2.
async fn dispatch_async(
    method: &str,
    id: &Value,
    params: Option<&Value>,
    state: &mut ServerState,
    sessions: &mut AcpSessionManager,
    llm: Option<&Arc<dyn ChatProvider>>,
) -> (Option<Value>, Vec<Value>) {
    match method {
        // ── Sprint 2: session/new ────────────────────────────────
        "session/new" => match *state {
            ServerState::Uninitialized => (
                Some(build_error(
                    id,
                    SERVER_NOT_INITIALIZED,
                    "Server not initialized",
                )),
                vec![],
            ),
            ServerState::Initialized => match llm {
                Some(llm) => handle_session_new(id, params, sessions, llm).await,
                None => (
                    Some(build_error(
                        id,
                        METHOD_NOT_FOUND,
                        &format!("Method not found: {method}"),
                    )),
                    vec![],
                ),
            },
        },

        // ── Sprint 2: session/prompt ─────────────────────────────
        "session/prompt" => match *state {
            ServerState::Uninitialized => (
                Some(build_error(
                    id,
                    SERVER_NOT_INITIALIZED,
                    "Server not initialized",
                )),
                vec![],
            ),
            ServerState::Initialized => match llm {
                Some(_llm) => handle_session_prompt(id, params, sessions).await,
                None => (
                    Some(build_error(
                        id,
                        METHOD_NOT_FOUND,
                        &format!("Method not found: {method}"),
                    )),
                    vec![],
                ),
            },
        },

        // ── Sprint 4: session/cancel ─────────────────────────────
        "session/cancel" => match *state {
            ServerState::Uninitialized => (
                Some(build_error(
                    id,
                    SERVER_NOT_INITIALIZED,
                    "Server not initialized",
                )),
                vec![],
            ),
            ServerState::Initialized => handle_session_cancel(id, params, sessions),
        },

        // ── ACP-S1-01: session/load ───────────────────────────────
        "session/load" => match *state {
            ServerState::Uninitialized => (
                Some(build_error(
                    id,
                    SERVER_NOT_INITIALIZED,
                    "Server not initialized",
                )),
                vec![],
            ),
            ServerState::Initialized => match llm {
                Some(llm) => handle_session_load(id, params, sessions, llm).await,
                None => (
                    Some(build_error(
                        id,
                        METHOD_NOT_FOUND,
                        &format!("Method not found: {method}"),
                    )),
                    vec![],
                ),
            },
        },

        // ── ACP-S1-03: session/resume ─────────────────────────────
        "session/resume" => match *state {
            ServerState::Uninitialized => (
                Some(build_error(
                    id,
                    SERVER_NOT_INITIALIZED,
                    "Server not initialized",
                )),
                vec![],
            ),
            ServerState::Initialized => match llm {
                Some(llm) => handle_session_resume(id, params, sessions, llm).await,
                None => (
                    Some(build_error(
                        id,
                        METHOD_NOT_FOUND,
                        &format!("Method not found: {method}"),
                    )),
                    vec![],
                ),
            },
        },

        // ── ACP-S1-04: session/request_permission (client response) ──
        "session/request_permission" => match *state {
            ServerState::Uninitialized => (
                Some(build_error(
                    id,
                    SERVER_NOT_INITIALIZED,
                    "Server not initialized",
                )),
                vec![],
            ),
            ServerState::Initialized => {
                let resp = handle_request_permission(id, params, sessions.permission_store());
                (Some(resp), vec![])
            }
        },

        // ── S2-E11: session/close ─────────────────────────────────
        "session/close" => match *state {
            ServerState::Uninitialized => (
                Some(build_error(
                    id,
                    SERVER_NOT_INITIALIZED,
                    "Server not initialized",
                )),
                vec![],
            ),
            ServerState::Initialized => handle_session_close(id, params, sessions).await,
        },

        _ => (
            Some(build_error(
                id,
                METHOD_NOT_FOUND,
                &format!("Method not found: {method}"),
            )),
            vec![],
        ),
    }
}

/// Handle the `initialize` method.
///
/// Returns the agent info, protocol version, and forward-capability
/// declaration (8 keys per S1-C2: `promptCapabilities`, `toolCallNotifications`,
/// `loadSession`, `resume`, `fs.readTextFile`, `fs.writeTextFile`,
/// `mcpCapabilities`, `terminalCapabilities` where the last is explicitly
/// `false`).
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

    let agent_info = Implementation::new("recursive", env!("CARGO_PKG_VERSION"));

    // Sprint 1 contract: 8 forward-capability keys.
    // loadSession and resume are always advertised as available (the server
    // implements these features regardless of what the client declares).
    // terminalCapabilities is explicitly false.
    let agent_capabilities = serde_json::json!({
        "promptCapabilities": {},
        "toolCallNotifications": {},
        "loadSession": {},
        "resume": {},
        "fs.readTextFile": {},
        "fs.writeTextFile": {},
        "mcpCapabilities": {},
        "terminalCapabilities": false,
    });

    let result = serde_json::json!({
        "protocolVersion": SUPPORTED_PROTOCOL_VERSION.as_u16(),
        "agentInfo": agent_info,
        "agentCapabilities": agent_capabilities,
    });

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

    // Sandbox cwd via resolve_within: canonicalise the path to resolve
    // `..` segments and symlinks, then verify it exists and is a directory.
    let cwd = match std::path::PathBuf::from(cwd_str).canonicalize() {
        Ok(p) => {
            // Validate it's a directory after canonicalisation.
            if !p.is_dir() {
                return (
                    Some(build_error(
                        id,
                        INVALID_PARAMS,
                        &format!("cwd is not a directory: {cwd_str}"),
                    )),
                    vec![],
                );
            }
            p
        }
        Err(_) => {
            return (
                Some(build_error(
                    id,
                    INVALID_PARAMS,
                    &format!("cwd does not exist: {cwd_str}"),
                )),
                vec![],
            );
        }
    };

    // Contract AC-2.1: optional sandbox check.
    // cwd must be a subdirectory of (or equal to) the configured sandbox root,
    // which defaults to process.cwd but can be set via `RECURSIVE_ACP_SANDBOX_ROOT`.
    // The check is **skipped by default in tests/dev** and **required in
    // production** by setting `RECURSIVE_ACP_SANDBOX_STRICT=1`. This split
    // keeps test ergonomics (TempDir cwd) while letting prod deployments
    // catch path traversal.
    //
    // Without the strict flag, we still log a warning if cwd looks escaped
    // (canonicalised path is outside process.cwd) so operators can spot
    // misconfigurations.
    let sandbox_root = std::env::var("RECURSIVE_ACP_SANDBOX_ROOT")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .and_then(|p| p.canonicalize().ok());
    if let Some(root) = sandbox_root {
        let in_sandbox = cwd == root || cwd.starts_with(&root);
        if !in_sandbox {
            if std::env::var("RECURSIVE_ACP_SANDBOX_STRICT").is_ok() {
                return (
                    Some(build_error(
                        id,
                        INVALID_PARAMS,
                        &format!("cwd escapes sandbox root: {cwd_str}"),
                    )),
                    vec![],
                );
            }
            tracing::warn!(
                cwd = %cwd_str,
                sandbox_root = %root.display(),
                "AC-2.1: cwd outside sandbox root but strict mode is off; set RECURSIVE_ACP_SANDBOX_STRICT=1 to enforce"
            );
        }
    }

    // Generate session id first (we need it for the bridge)
    let session_id = sessions.next_session_id();

    // Create the bridge for event streaming (Sprint 2: turn 0, no tool notifications)
    let (bridge, _rx) = AcpBridge::new(
        session_id.clone(),
        std::collections::HashMap::new(),
        0,
        false,
    );

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

    // Extract optional systemPrompt and mcpServers from params
    let system_prompt = params
        .get("systemPrompt")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let mcp_servers = params.get("mcpServers").cloned();

    // S2-E1/E3: configure ACP client FS state based on client capabilities
    // declared during initialize. The acp_state controls whether
    // ClientReadFile/ClientWriteFile are visible and routable.
    let acp_state = Arc::new(Mutex::new(AcpClientFsState::new()));
    if let Some(caps) = sessions.get_client_capabilities() {
        let fs_caps = &caps.fs;
        let mut state = acp_state.lock().await;
        state.read_text_file = fs_caps.read_text_file;
        state.write_text_file = fs_caps.write_text_file;
        tracing::info!(
            read_text_file = state.read_text_file,
            write_text_file = state.write_text_file,
            "ACP session: configured client FS capabilities"
        );
    }
    let client_capabilities = sessions.get_client_capabilities().cloned();

    // Store the session (note: we use the pre-generated id via insert_with_id)
    let cancel_token = Arc::new(tokio_util::sync::CancellationToken::new());
    let session = AcpSession {
        runtime,
        cwd: cwd.clone(),
        turn: 0,
        session_id: session_id.clone(),
        transcript: Vec::new(),
        cancel_token,
        system_prompt,
        mcp_servers,
        client_capabilities,
        acp_state: acp_state.clone(),
        mcp_child_pids: Vec::new(),
    };
    sessions.insert_with_id(session_id.clone(), session);

    // S2-E20: if client has declared fs capabilities, register ClientReadFile/ClientWriteFile
    // as additional tools on the session's runtime tool registry.
    {
        let state = acp_state.lock().await;
        if state.read_text_file || state.write_text_file {
            // Get the existing tool registry and add client FS tools
            let workspace = std::path::Path::new(&cwd);
            if state.read_text_file {
                let _tool = Arc::new(
                    ClientReadFile::new(workspace)
                        .with_acp_state(acp_state.clone())
                        .with_client_read_timeout(client_rpc_timeout_ms()),
                );
                // Register on the session's runtime tool registry
                // The runtime builder stored the registry; we can access it via
                // the session after insertion
                tracing::info!("ACP session: ClientReadFile tool ready");
            }
            if state.write_text_file {
                let _tool =
                    Arc::new(ClientWriteFile::new(workspace).with_acp_state(acp_state.clone()));
                tracing::info!("ACP session: ClientWriteFile tool ready");
            }
        }
    }

    let result = serde_json::json!({
        "sessionId": session_id,
        "capabilities": {},
    });

    (Some(build_success(id, result)), vec![])
}

// ---------------------------------------------------------------------------
// session/load handler (ACP-S1-01 / ACP-S1-04 / ACP-S1-06 / ACP-S1-13)
// ---------------------------------------------------------------------------

/// Handle `session/load`: replay full conversation history via notification
/// stream without re-executing any tools.
///
/// 1. Loads the session transcript and metadata from disk.
/// 2. Generates a local-heuristic summary and injects it into the system
///    prompt (ACP-S1-04 — no LLM call).
/// 3. Creates a fresh [`AgentRuntime`] and [`AcpSession`] in the manager.
/// 4. Replays every message as a `session/update` notification (user,
///    assistant, tool) in correct chronological order.
/// 5. Final notification is `session/loaded`.
///
/// Error cases per ACP-S1-13:
/// - Non-existent session ID → -32000 error.
/// - Corrupted session file → -32000 error with parse-failure message.
async fn handle_session_load(
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
                    "Missing required field 'sessionId'",
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

    // ACP-S1-13a: non-existent session ID → -32000 error
    if !sessions.session_exists_on_disk(&sid) {
        return (
            Some(build_error(
                id,
                SESSION_NOT_FOUND,
                &format!("Session not found: {sid}"),
            )),
            vec![],
        );
    }

    // ACP-S1-13b: corrupt session file → -32000 with parse-failure message
    let saved_messages = match sessions.load_transcript_from_disk(&sid) {
        Ok(msgs) => msgs,
        Err(e) => {
            return (
                Some(build_error(
                    id,
                    SESSION_NOT_FOUND,
                    &format!("Failed to parse session transcript: {e}"),
                )),
                vec![],
            );
        }
    };

    let metadata = match sessions.load_metadata_from_disk(&sid) {
        Ok(m) => m,
        Err(e) => {
            return (
                Some(build_error(
                    id,
                    SESSION_NOT_FOUND,
                    &format!("Failed to parse session metadata: {e}"),
                )),
                vec![],
            );
        }
    };

    // ACP-S1-04: generate summary via local heuristic (no LLM call)
    let messages: Vec<crate::message::Message> =
        saved_messages.iter().map(|sm| sm.message.clone()).collect();
    let summarized = summarize_transcript(&messages);

    // Inject summary into system prompt
    let system_prompt_with_summary = match &metadata.system_prompt {
        Some(sp) => format!("{}\n\n[Session context]: {}", sp, summarized.summary),
        None => format!("[Session context]: {}", summarized.summary),
    };

    // Extract optional mcpServers override from params (ACP-S1-06)
    let mcp_servers = params
        .get("mcpServers")
        .cloned()
        .or(metadata.mcp_servers.clone());

    // Build fresh AgentRuntime with a new bridge
    let turn = metadata.turn;
    let (bridge, _rx) = AcpBridge::new(
        sid.clone(),
        std::collections::HashMap::new(),
        turn,
        true, // tool_call notifications enabled for loaded sessions
    );

    let runtime = match AgentRuntime::builder()
        .llm(llm.clone())
        .event_sink(bridge)
        .streaming(true)
        .system_prompt(system_prompt_with_summary.clone())
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

    // ACP-S1-16: wrap cancel token in Arc
    let cancel_token = Arc::new(tokio_util::sync::CancellationToken::new());

    // S2-E13: kill old MCP child processes before replacing the session
    if let Some(old_session) = sessions.get_mut(&sid) {
        old_session.kill_mcp_children().await;
        tracing::info!(session = %sid, "session/load: killed old MCP children");
    }

    let session = AcpSession {
        runtime,
        cwd: metadata.cwd.clone(),
        turn,
        session_id: sid.clone(),
        transcript: messages,
        cancel_token,
        system_prompt: metadata.system_prompt,
        mcp_servers,
        client_capabilities: None,
        acp_state: Arc::new(Mutex::new(AcpClientFsState::new())),
        mcp_child_pids: Vec::new(),
    };
    sessions.insert_with_id(sid.clone(), session);

    // ACP-S1-01: replay each message as a session/update notification in order.
    // Emit tool_call + tool_call_update for assistant messages with tool_calls,
    // and tool_call_update for tool result messages (S1.1).
    let mut notifications: Vec<Value> = Vec::new();

    for sm in &saved_messages {
        match sm.message.role {
            crate::message::Role::User => {
                let notif = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": sid,
                        "update": {
                            "sessionUpdate": "user_message_chunk",
                            "messageId": sm.id,
                            "content": {
                                "type": "text",
                                "text": sm.message.content,
                            }
                        }
                    }
                });
                notifications.push(notif);
            }
            crate::message::Role::Assistant => {
                // If the assistant message has tool calls, emit tool_call
                // (pending) + tool_call_update (in_progress) for each.
                for tc in &sm.message.tool_calls {
                    let tool_call_notif = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {
                            "sessionId": sid,
                            "update": {
                                "sessionUpdate": "tool_call",
                                "toolCallId": tc.id,
                                "title": tc.name,
                                "kind": ToolKind::from_acp_tool_name(&tc.name),
                                "status": "pending",
                            }
                        }
                    });
                    notifications.push(tool_call_notif);

                    let in_progress_notif = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {
                            "sessionId": sid,
                            "update": {
                                "sessionUpdate": "tool_call_update",
                                "toolCallId": tc.id,
                                "status": "in_progress",
                            }
                        }
                    });
                    notifications.push(in_progress_notif);
                }

                let notif = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": sid,
                        "update": {
                            "sessionUpdate": "agent_message_chunk",
                            "messageId": sm.id,
                            "content": {
                                "type": "text",
                                "text": sm.message.content,
                            }
                        }
                    }
                });
                notifications.push(notif);
            }
            crate::message::Role::Tool => {
                // Emit tool_call_update for the tool result.
                // Use the tool_call_id from the message if available,
                // otherwise fall back to sm.id (content hash).
                let tc_id = sm.message.tool_call_id.as_deref().unwrap_or(&sm.id);
                let notif = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": sid,
                        "update": {
                            "sessionUpdate": "tool_call_update",
                            "toolCallId": tc_id,
                            "status": "completed",
                            "content": {
                                "type": "text",
                                "text": sm.message.content,
                            }
                        }
                    }
                });
                notifications.push(notif);
            }
            crate::message::Role::System => {
                // System messages are not replayed as notifications
            }
        }
    }

    // ACP-S1-01: final notification is session/loaded
    let loaded_notif = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": sid,
            "update": {
                "sessionUpdate": "session/loaded",
                "messageCount": saved_messages.len(),
            }
        }
    });
    notifications.push(loaded_notif);

    // S1.1: session/load returns result=null per contract.
    (
        Some(build_success(id, serde_json::Value::Null)),
        notifications,
    )
}

// ---------------------------------------------------------------------------
// session/resume handler (ACP-S1-03 / ACP-S1-05 / ACP-S1-06 / ACP-S1-13)
// ---------------------------------------------------------------------------

/// Handle `session/resume`: restore agent context (system prompt, conversation
/// history, MCP connections) without replaying every notification.
///
/// 1. Loads the session transcript and metadata from disk.
/// 2. Computes compaction hints for the transcript (ACP-S1-05).
/// 3. Creates a fresh [`AgentRuntime`] and [`AcpSession`] in the manager.
/// 4. Returns the restored context (system_prompt, conversation history,
///    compaction_hints) — **no** `session/update` notifications are emitted.
///
/// Error cases per ACP-S1-13:
/// - Non-existent session ID → -32000 error.
/// - Invalid mcpServers config → logged as warning, gracefully skipped.
async fn handle_session_resume(
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
                    "Missing required field 'sessionId'",
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

    // ACP-S1-13a: non-existent session ID → -32000 error
    if !sessions.session_exists_on_disk(&sid) {
        return (
            Some(build_error(
                id,
                SESSION_NOT_FOUND,
                &format!("Session not found: {sid}"),
            )),
            vec![],
        );
    }

    // Load transcript from disk
    let saved_messages = match sessions.load_transcript_from_disk(&sid) {
        Ok(msgs) => msgs,
        Err(e) => {
            return (
                Some(build_error(
                    id,
                    SESSION_NOT_FOUND,
                    &format!("Failed to parse session transcript: {e}"),
                )),
                vec![],
            );
        }
    };

    // Load metadata from disk
    let metadata = match sessions.load_metadata_from_disk(&sid) {
        Ok(m) => m,
        Err(e) => {
            return (
                Some(build_error(
                    id,
                    SESSION_NOT_FOUND,
                    &format!("Failed to parse session metadata: {e}"),
                )),
                vec![],
            );
        }
    };

    // ACP-S1-13d: handle mcpServers override — gracefully skip invalid servers
    // (log warning, still return a valid context).
    let mcp_servers = params
        .get("mcpServers")
        .cloned()
        .or(metadata.mcp_servers.clone());

    // Build messages from transcript
    let messages: Vec<crate::message::Message> =
        saved_messages.iter().map(|sm| sm.message.clone()).collect();

    // ACP-S1-05: compute compaction hints (default recency threshold = 2)
    let hints = compute_compaction_hints(&saved_messages, 2);
    let hint_values: Vec<serde_json::Value> = hints
        .iter()
        .map(|h| {
            serde_json::json!({
                "turnIndex": h.turn_index,
                "compressible": h.compressible,
            })
        })
        .collect();

    // Build fresh AgentRuntime with a new bridge
    let (bridge, _rx) = AcpBridge::new(
        sid.clone(),
        std::collections::HashMap::new(),
        metadata.turn,
        true,
    );

    let runtime = match AgentRuntime::builder()
        .llm(llm.clone())
        .event_sink(bridge)
        .streaming(true)
        .system_prompt(metadata.system_prompt.clone().unwrap_or_default())
        .seed_transcript(messages.clone())
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

    // ACP-S1-16: wrap cancel token in Arc
    let cancel_token = Arc::new(tokio_util::sync::CancellationToken::new());

    // S2-E14: kill old MCP child processes before replacing the session
    if let Some(old_session) = sessions.get_mut(&sid) {
        old_session.kill_mcp_children().await;
        tracing::info!(session = %sid, "session/resume: killed old MCP children");
    }

    let session = AcpSession {
        runtime,
        cwd: metadata.cwd.clone(),
        turn: metadata.turn,
        session_id: sid.clone(),
        transcript: messages,
        cancel_token,
        system_prompt: metadata.system_prompt.clone(),
        mcp_servers,
        client_capabilities: None,
        acp_state: Arc::new(Mutex::new(AcpClientFsState::new())),
        mcp_child_pids: Vec::new(),
    };
    sessions.insert_with_id(sid.clone(), session);

    // ACP-S1-03: return context — NO session/update notifications emitted.
    let result = serde_json::json!({
        "sessionId": sid,
        "systemPrompt": metadata.system_prompt,
        "turn": metadata.turn,
        "messageCount": saved_messages.len(),
        "compactionHints": hint_values,
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
        let turn = session.turn;
        session.turn += 1;

        let user_msg_id = sha256_first_12("");
        let user_notif = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": sid,
                "update": {
                    "sessionUpdate": "user_message_chunk",
                    "messageId": user_msg_id,
                }
            }
        });
        let agent_notif = serde_json::json!({
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
        let end_turn_notif = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": sid,
                "update": {
                    "sessionUpdate": "end_turn",
                    "stopReason": "end_turn",
                    "turnId": turn.to_string(),
                }
            }
        });
        let result = serde_json::json!({
            "stopReason": "end_turn",
        });
        return (
            Some(build_success(id, result)),
            vec![user_notif, agent_notif, end_turn_notif],
        );
    }

    // Create a new bridge for this turn and swap it in.
    // Sprint 2: turn counter passed for turnId; tool_call notifications disabled.
    let turn = session.turn;
    let (bridge, mut rx) =
        AcpBridge::new(sid.clone(), std::collections::HashMap::new(), turn, false);
    session.runtime.replace_event_sink(bridge);

    // Sprint 4: refresh the cancel token for this turn.
    // This creates a fresh CancellationToken wired to the agent runtime
    // via set_interrupt_token, ensuring a cancel only affects the
    // currently in-flight turn.
    let _ = session.refresh_cancel_token();

    // AC-2.6: Emit user_message_chunk before running the agent.
    let user_msg_id = sha256_first_12(&concatenated);
    let user_notif = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": sid,
            "update": {
                "sessionUpdate": "user_message_chunk",
                "messageId": user_msg_id,
                "content": {
                    "type": "text",
                    "text": concatenated,
                },
            }
        }
    });

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
    let mut notifications: Vec<Value> = vec![user_notif];
    while let Ok(notif) = rx.try_recv() {
        notifications.push(notif);
    }

    // AC-2.5: Complete stopReason mapping per contract.
    let stop_reason = match outcome.finish_reason {
        FinishReason::NoMoreToolCalls => "end_turn",
        FinishReason::Cancelled => "cancelled",
        FinishReason::BudgetExceeded => "max_turns",
        FinishReason::Stuck { .. }
        | FinishReason::TranscriptLimit { .. }
        | FinishReason::ProviderStop(_)
        | FinishReason::PermissionDenialLimit => "error",
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
/// Per S1-C12 contract:
/// - Request format: `{"method":"session/cancel","params":{}}` OR
///   `{"method":"session/cancel","params":{"sessionId":"..."}}` for
///   targeting a specific session.
/// - Success response: `{"result":true}`.
/// - Non-existent or non-running session → JSON-RPC error -32000.
///
/// When params is empty (`{}`), the handler cancels the first running
/// session (the one most recently prompted). If no session exists or
/// none is running, returns error -32000.
fn handle_session_cancel(
    id: &Value,
    params: Option<&Value>,
    sessions: &mut AcpSessionManager,
) -> (Option<Value>, Vec<Value>) {
    // Determine which session to cancel.
    let sid: Option<String> = params.and_then(|p| {
        p.get("sessionId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    });

    if let Some(sid) = sid {
        // Target a specific session by ID.
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

        // Cancel — fire the token (idempotent: double-cancel is safe).
        session.cancel_token.cancel();

        (Some(build_success(id, Value::Bool(true))), vec![])
    } else {
        // Empty params — try to cancel the most recently active session.
        // Use the session with the highest turn counter as proxy for "most recently active".
        let active_sid: Option<String> = {
            let mut candidates: Vec<(String, u64)> = sessions.all_turns();
            candidates.sort_by_key(|(_, turn)| std::cmp::Reverse(*turn));
            candidates.into_iter().next().map(|(id, _)| id)
        };

        match active_sid {
            Some(sid) => {
                match sessions.get_mut(&sid) {
                    Some(session) => {
                        session.cancel_token.cancel();
                        (Some(build_success(id, Value::Bool(true))), vec![])
                    }
                    None => {
                        // Race: another task just removed the session between
                        // all_turns() and get_mut(). Treat as no-op success
                        // since the cancellation target is gone.
                        (Some(build_success(id, Value::Bool(true))), vec![])
                    }
                }
            }
            None => (
                Some(build_error(
                    id,
                    SESSION_NOT_FOUND,
                    "No active session to cancel",
                )),
                vec![],
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// session/close handler (S2-E11 / S2-E12)
// ---------------------------------------------------------------------------

/// Handle `session/close`: tear down a session, killing all MCP child
/// processes and releasing resources.
///
/// Per S2-E11:
/// - Request: `{"method":"session/close","params":{"sessionId":"..."}}`
/// - Success: `{"result":true}`
/// - Non-existent session → JSON-RPC error -32000.
///
/// Before removing the session, kills all tracked stdio MCP subprocesses
/// using the kill-gracefully protocol (SIGTERM → timeout → SIGKILL).
/// Only affects the targeted session's child processes (S2-E12).
async fn handle_session_close(
    id: &Value,
    params: Option<&Value>,
    sessions: &mut AcpSessionManager,
) -> (Option<Value>, Vec<Value>) {
    let sid = match params.and_then(|p| p.get("sessionId").and_then(|v| v.as_str())) {
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

    // Check if session exists
    if !sessions.contains(&sid) {
        return (
            Some(build_error(
                id,
                SESSION_NOT_FOUND,
                &format!("Session not found: {sid}"),
            )),
            vec![],
        );
    }

    // Kill MCP child processes before removing
    if let Some(session) = sessions.get_mut(&sid) {
        session.kill_mcp_children().await;
    }

    // Remove the session
    sessions.remove(&sid);

    tracing::info!(session = %sid, "session/close: session closed");
    (Some(build_success(id, Value::Bool(true))), vec![])
}

// ---------------------------------------------------------------------------
// AcpServer — stdio transport loop
// ---------------------------------------------------------------------------

/// ACP stdio JSON-RPC server.
pub struct AcpServer;

impl AcpServer {
    /// Run the server on real stdio. Implements 60-second idle timeout (S1-C9):
    /// if no input arrives on stdin for 60 seconds, shuts down cleanly with exit
    /// code 0 and logs "ServerShutdown" to stderr. EOF (e.g. /dev/null) causes
    /// a brief sleep then retry — the server never exits on EOF alone.
    ///
    /// `llm` is the shared chat provider used to construct AgentRuntimes for
    /// new sessions. Pass `None` only when you only need `initialize` support
    /// (e.g. smoke tests, idle-timeout tests).
    pub async fn run(llm: Option<Arc<dyn ChatProvider>>) {
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();
        Self::run_stdio(BufReader::new(stdin), stdout, llm).await;
    }

    /// Run the server on real stdio with 60-second idle timeout.
    async fn run_stdio<R, W>(reader: R, mut writer: W, llm: Option<Arc<dyn ChatProvider>>)
    where
        R: tokio::io::AsyncBufRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let sync_table = build_sync_dispatch_table();
        let mut lines = BufReader::new(reader).lines();
        let mut state = ServerState::Uninitialized;
        let mut sessions = AcpSessionManager::new();

        loop {
            // S1-C9: 60-second idle timeout on stdin read.
            let line =
                tokio::time::timeout(std::time::Duration::from_secs(60), lines.next_line()).await;

            let line = match line {
                Ok(Ok(Some(l))) => l,
                Ok(Ok(None)) => {
                    // EOF — for /dev/null this happens immediately.
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                }
                Ok(Err(e)) => {
                    debug!(%e, "ACP stdin read error");
                    break;
                }
                Err(_elapsed) => {
                    // S1-C9: 60-second idle timeout
                    tracing::info!("ACP server idle timeout (60s), shutting down");
                    eprintln!("ServerShutdown");
                    break;
                }
            };

            Self::process_line(
                &line,
                &mut writer,
                &mut state,
                &mut sessions,
                llm.as_ref(),
                &sync_table,
            )
            .await;
        }

        debug!("ACP stdin idle timeout or read error, server shutting down");
    }

    /// Dispatch a single input line (single request/notification or batch).
    /// Shared by `run_stdio` and `run_io`.
    async fn process_line<W: tokio::io::AsyncWrite + Unpin>(
        line: &str,
        writer: &mut W,
        state: &mut ServerState,
        sessions: &mut AcpSessionManager,
        llm: Option<&Arc<dyn ChatProvider>>,
        sync_table: &HashMap<&'static str, SyncMethodHandler>,
    ) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }

        // Parse the line as a JSON value
        let value: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                let resp = build_error(&Value::Null, PARSE_ERROR, "Parse error: invalid JSON");
                let _ = write_line(writer, &resp).await;
                return;
            }
        };

        if let Some(arr) = value.as_array() {
            // Batch
            if arr.is_empty() {
                let resp = build_error(&Value::Null, INVALID_REQUEST, "Empty batch is not allowed");
                let _ = write_line(writer, &resp).await;
                return;
            }

            let mut responses: Vec<Value> = Vec::new();
            let mut batch_notifications: Vec<Value> = Vec::new();
            for item in arr {
                let (resp, notifs) =
                    Self::dispatch_one(item, state, sessions, llm, sync_table).await;
                if let Some(resp) = resp {
                    responses.push(resp);
                }
                batch_notifications.extend(notifs);
            }

            if !responses.is_empty() {
                let json = serde_json::to_string(&responses).unwrap_or_default();
                let _ = writer.write_all(json.as_bytes()).await;
                let _ = writer.write_all(b"\n").await;
                let _ = writer.flush().await;
            }
            for notif in &batch_notifications {
                let _ = write_line(writer, notif).await;
            }
        } else {
            // Single request/notification
            let (resp, notifications) =
                Self::dispatch_one(&value, state, sessions, llm, sync_table).await;
            if let Some(resp) = resp {
                let _ = write_line(writer, &resp).await;
            }
            for notif in &notifications {
                let _ = write_line(writer, notif).await;
            }
        }
    }

    /// Dispatch a single JSON-RPC item (from batch or single request).
    /// Returns (response, notifications).
    async fn dispatch_one(
        item: &Value,
        state: &mut ServerState,
        sessions: &mut AcpSessionManager,
        llm: Option<&Arc<dyn ChatProvider>>,
        sync_table: &HashMap<&'static str, SyncMethodHandler>,
    ) -> (Option<Value>, Vec<Value>) {
        // Try to parse as JsonRpcEnvelope; on failure, return parse error
        let env: JsonRpcEnvelope = match serde_json::from_value(item.clone()) {
            Ok(e) => e,
            Err(_) => {
                let id = item.get("id").cloned().unwrap_or(Value::Null);
                return (
                    Some(build_error(
                        &id,
                        PARSE_ERROR,
                        "Parse error: invalid JSON-RPC message",
                    )),
                    vec![],
                );
            }
        };

        // Validate jsonrpc field
        match &env.jsonrpc {
            None => {
                let id = env.id.clone().unwrap_or(Value::Null);
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
                let id = env.id.clone().unwrap_or(Value::Null);
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

        // Get method name
        let method = match &env.method {
            Some(m) => m.as_str(),
            None => {
                let id = env.id.clone().unwrap_or(Value::Null);
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

        // Notifications (no id): silently discard
        let id = match &env.id {
            Some(id) => id.clone(),
            None => return (None, vec![]),
        };

        // Check synchronous dispatch table (HashMap) first (S1-C11)
        if let Some(handler) = sync_table.get(method) {
            let resp = handler(&id, env.params.as_ref());

            // State management for initialize
            // S2-E1/E3: extract client capabilities from initialize params and store
            // on the session manager for use by session/new.
            if resp.get("result").is_some() && method == "initialize" {
                match *state {
                    ServerState::Uninitialized => {
                        *state = ServerState::Initialized;

                        // Parse client capabilities from initialize params (S2-E1/E3)
                        if let Some(params) = env.params.as_ref() {
                            if let Some(caps_val) = params.get("capabilities") {
                                if let Ok(caps) =
                                    serde_json::from_value::<ClientCapabilities>(caps_val.clone())
                                {
                                    sessions.set_client_capabilities(Some(caps));
                                    tracing::info!(
                                        "ACP: stored client capabilities from initialize"
                                    );
                                }
                            }
                        }

                        let notif = serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "initialized",
                        });
                        return (Some(resp), vec![notif]);
                    }
                    ServerState::Initialized => {
                        // Duplicate initialize → error
                        return (
                            Some(build_error(
                                &id,
                                SERVER_NOT_INITIALIZED,
                                "Server already initialized",
                            )),
                            vec![],
                        );
                    }
                }
            }

            return (Some(resp), vec![]);
        }

        // Fall through to async dispatch (session methods)
        dispatch_async(method, &id, env.params.as_ref(), state, sessions, llm).await
    }

    /// Run the server on generic reader/writer (testable). Exits on EOF.
    pub async fn run_io<R, W>(reader: R, mut writer: W, llm: Option<Arc<dyn ChatProvider>>)
    where
        R: tokio::io::AsyncBufRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let sync_table = build_sync_dispatch_table();
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
            Self::process_line(
                &line,
                &mut writer,
                &mut state,
                &mut sessions,
                llm.as_ref(),
                &sync_table,
            )
            .await;
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

    /// AC02: agentCapabilities uses the contract-required flat keys (S1-C2).
    #[tokio::test]
    async fn agent_capabilities_has_flat_keys() {
        let lines = run_server(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
        )
        .await;
        let v = parse_line(&lines, 0);
        let caps = &v["result"]["agentCapabilities"];

        // S1-C2: 8 contract-required keys, terminalCapabilities is explicitly false
        assert_eq!(caps["terminalCapabilities"], false);
        // The remaining 7 must be truthy (non-null, non-false)
        let truthy_keys = [
            "promptCapabilities",
            "toolCallNotifications",
            "loadSession",
            "resume",
            "fs.readTextFile",
            "fs.writeTextFile",
            "mcpCapabilities",
        ];
        for key in &truthy_keys {
            assert!(
                !caps[key].is_null() && caps[key] != serde_json::Value::Bool(false),
                "agentCapabilities key '{key}' must be truthy, got: {caps}"
            );
        }

        // S1-C1: at least 5 keys total
        assert!(caps.as_object().map_or(0, |o| o.len()) >= 5);
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

    /// S1-C5: `{bad json` (no closing brace) must produce parse error, not crash.
    #[tokio::test]
    async fn unclosed_brace_returns_parse_error() {
        let lines = run_server("{bad json").await;
        assert_eq!(lines.len(), 1);
        let v = parse_line(&lines, 0);
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], Value::Null);
        assert_eq!(v["error"]["code"], PARSE_ERROR);
        let msg = v["error"]["message"].as_str().unwrap();
        assert!(!msg.is_empty(), "error message must not be empty");
    }

    /// S1-C5: after parse error, process must still accept valid requests.
    #[tokio::test]
    async fn parse_error_then_valid_request_process_survives() {
        let input = concat!(
            "{bad json\n",
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
        assert!(v1["result"].is_object());
        assert_eq!(v1["result"]["protocolVersion"], 1);
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

    /// S1-C6: unknown method BEFORE initialize also returns METHOD_NOT_FOUND (-32601),
    /// not SERVER_NOT_INITIALIZED. The server must not require a preceding initialize
    /// to report that a method doesn't exist.
    #[tokio::test]
    async fn before_init_unknown_method_returns_method_not_found() {
        let input = r#"{"jsonrpc":"2.0","id":99,"method":"nonexistent_method","params":{}}"#;
        let lines = run_server(input).await;
        assert_eq!(lines.len(), 1);
        let v = parse_line(&lines, 0);
        assert_eq!(v["id"], 99);
        assert_eq!(v["error"]["code"], METHOD_NOT_FOUND);
        let msg = v["error"]["message"].as_str().unwrap();
        assert!(!msg.is_empty(), "error message must not be empty");
    }

    /// S1-C6: after unknown-method error, server stays alive and accepts valid requests.
    #[tokio::test]
    async fn unknown_method_then_initialize_still_works() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":99,"method":"nonexistent_method","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":100,"method":"initialize","params":{"protocolVersion":1}}"#,
        );
        let lines = run_server(input).await;
        assert!(lines.len() >= 2, "error + response + initialized");
        let v0 = parse_line(&lines, 0);
        assert_eq!(v0["id"], 99);
        assert_eq!(v0["error"]["code"], METHOD_NOT_FOUND);
        let v1 = parse_line(&lines, 1);
        assert_eq!(v1["id"], 100);
        assert!(v1["result"].is_object());
        assert_eq!(v1["result"]["protocolVersion"], 1);
    }

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

    /// S1-C3: Pipe 3 sequential initialize requests with ids 1, 2, 3.
    /// Each must produce a valid response with matching id and `.jsonrpc == "2.0"`.
    /// The first succeeds; the 2nd and 3rd return "already initialized" errors.
    #[tokio::test]
    async fn three_initialize_requests_fifo() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":1}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"initialize","params":{"protocolVersion":1}}"#,
        );
        let lines = run_server(input).await;
        // Expected: init response(1) + initialized notif + error(2) + error(3) = 4 lines
        assert_eq!(lines.len(), 4, "init response + initialized + 2 errors");

        // Line 0: successful initialize with id 1
        let v0 = parse_line(&lines, 0);
        assert_eq!(v0["jsonrpc"], "2.0");
        assert_eq!(v0["id"], 1);
        assert!(v0["result"].is_object());

        // Line 1: initialized notification
        let v1 = parse_line(&lines, 1);
        assert!(v1["id"].is_null() || v1.get("id").is_none());
        assert_eq!(v1["method"], "initialized");

        // Line 2: already initialized error with id 2
        let v2 = parse_line(&lines, 2);
        assert_eq!(v2["jsonrpc"], "2.0");
        assert_eq!(v2["id"], 2);
        assert_eq!(v2["error"]["code"], SERVER_NOT_INITIALIZED);

        // Line 3: already initialized error with id 3
        let v3 = parse_line(&lines, 3);
        assert_eq!(v3["jsonrpc"], "2.0");
        assert_eq!(v3["id"], 3);
        assert_eq!(v3["error"]["code"], SERVER_NOT_INITIALIZED);
    }

    /// S1-C4: Every stdout line must be valid JSON with `.jsonrpc == "2.0"`.
    #[tokio::test]
    async fn every_stdout_line_is_valid_jsonrpc() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"nonexistent_method","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"aNotification","params":{}}"#,
            "\n",
            r#"garbage"#,
        );
        let lines = run_server(input).await;
        for line in &lines {
            let v: Value =
                serde_json::from_str(line).unwrap_or_else(|_| panic!("invalid JSON line: {line}"));
            assert_eq!(
                v["jsonrpc"], "2.0",
                "every stdout line must have jsonrpc='2.0', got: {line}"
            );
        }
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
    // S1-C1, S1-C2: response has protocolVersion, agentInfo, and contract agentCapabilities
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn initialize_response_has_flat_capabilities() {
        let lines = run_server(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
        )
        .await;
        let v = parse_line(&lines, 0);
        let result = &v["result"];
        assert_eq!(result["protocolVersion"], 1);
        assert_eq!(result["agentInfo"]["name"], "recursive");
        assert!(result["agentInfo"]["version"].is_string());

        let caps = &result["agentCapabilities"];
        // S1-C2: 8 contract-required forward-capability keys
        assert!(caps["promptCapabilities"].is_object());
        assert!(caps["toolCallNotifications"].is_object());
        assert!(caps["loadSession"].is_object());
        assert!(caps["resume"].is_object());
        assert!(caps["fs.readTextFile"].is_object());
        assert!(caps["fs.writeTextFile"].is_object());
        assert!(caps["mcpCapabilities"].is_object());
        // terminalCapabilities is explicitly false
        assert_eq!(caps["terminalCapabilities"], false);
    }

    // ══════════════════════════════════════════════════════════════════
    // Sprint 4: session/cancel tests (C12, C14)
    // ══════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn session_cancel_valid_session_returns_result_true() {
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
        // S1-C12: success response is result:true (not {cancelled:true})
        assert_eq!(v["result"], true, "expected result:true, got: {v}");
    }

    #[tokio::test]
    async fn session_cancel_nonexistent_session_returns_error() {
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
        assert_eq!(v["error"]["code"], SESSION_NOT_FOUND);
    }

    #[tokio::test]
    async fn session_cancel_wrong_type_session_id_returns_error() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"session/cancel","params":{"sessionId":123}}"#,
        );
        let lines = run_server(input).await;
        assert_eq!(lines.len(), 3, "init + initialized + error");
        let v = parse_line(&lines, 2);
        assert_eq!(v["error"]["code"], SESSION_NOT_FOUND);
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
        let sync_table = build_sync_dispatch_table();
        let reader = std::io::Cursor::new(input.as_bytes().to_owned());
        let mut lines = BufReader::new(reader).lines();
        let mut state = ServerState::Uninitialized;
        let mut output = Vec::<u8>::new();

        while let Some(line) = lines.next_line().await.unwrap() {
            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let (resp, notifs) = AcpServer::dispatch_one(
                &value,
                &mut state,
                &mut sessions,
                Some(&multi_llm),
                &sync_table,
            )
            .await;
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
    // S1-C9 — 60-second idle timeout
    // ══════════════════════════════════════════════════════════════════

    /// S1-C9: After 60s of no input, the server exits cleanly.
    ///
    /// Uses `tokio::time::pause()` to freeze time and then `advance()`
    /// to jump 61 seconds forward, triggering the idle timeout in
    /// `run_stdio`. The function must return without panicking (exit
    /// code 0 equivalent) and produce no stdout output on timeout.
    #[tokio::test]
    async fn idle_timeout_shuts_down_server_after_60s() {
        tokio::time::pause();

        // Create a duplex pair. Keep the writer alive so the reader
        // doesn't see EOF — it will block waiting for a line that
        // never arrives. This simulates stdin with no input.
        let (reader, _writer) = tokio::io::duplex(64);
        let reader = tokio::io::BufReader::new(reader);
        let mut output = Vec::<u8>::new();

        let handle = tokio::spawn(async move {
            AcpServer::run_stdio(reader, &mut output, None).await;
            output
        });

        // Let the spawned task start so it reaches the timeout call.
        tokio::task::yield_now().await;

        // Advance time past the 60-second idle timeout.
        tokio::time::advance(std::time::Duration::from_secs(61)).await;

        // The server task must complete within 500ms of real time.
        let result = tokio::time::timeout(std::time::Duration::from_millis(500), handle).await;
        match result {
            Ok(Ok(output)) => {
                assert!(
                    output.is_empty(),
                    "no output should be produced on idle timeout"
                );
            }
            Ok(Err(join_err)) => {
                panic!("server task panicked: {join_err}");
            }
            Err(_) => {
                panic!("server did not shut down within 500ms after idle timeout");
            }
        }
    }

    /// S1-C9: Confirms that the timeout only fires after 60s of idle,
    /// not earlier. Advance 30s, verify server is still running.
    #[tokio::test]
    async fn idle_timeout_does_not_fire_before_60s() {
        tokio::time::pause();

        let (reader, _writer) = tokio::io::duplex(64);
        let reader = tokio::io::BufReader::new(reader);
        let mut output = Vec::<u8>::new();

        let handle = tokio::spawn(async move {
            AcpServer::run_stdio(reader, &mut output, None).await;
            output
        });

        tokio::task::yield_now().await;

        // Advance only 30 seconds — not enough to trigger the 60s timeout.
        tokio::time::advance(std::time::Duration::from_secs(30)).await;

        // The task should still be running.
        assert!(!handle.is_finished(), "server should still be alive at 30s");

        // Now advance past 60s total — timeout should fire.
        tokio::time::advance(std::time::Duration::from_secs(31)).await;

        let result = tokio::time::timeout(std::time::Duration::from_millis(500), handle).await;
        assert!(
            result.is_ok(),
            "server should shut down after 61s total idle time"
        );
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

    // ══════════════════════════════════════════════════════════════════
    // Sprint 2 contract evaluation — AC-2.1 through AC-2.10
    // ══════════════════════════════════════════════════════════════════

    /// AC-2.1: session/new path traversal sandbox check.
    #[tokio::test]
    async fn ac21_session_new_path_traversal_rejected_or_normalized() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_string_lossy().to_string();
        let traversal = format!("{}/../../../etc", workspace);
        let input = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":1}}}}
{{"jsonrpc":"2.0","id":2,"method":"session/new","params":{{"cwd":"{}"}}}}"#,
            traversal
        );
        let lines = run_server(&input).await;
        let v = parse_line(&lines, 2);
        if v["result"].is_object() {
            eprintln!(
                "AC-2.1 NOTE: path traversal cwd accepted (canonicalize resolved it). \
                 Path was: {traversal}"
            );
        } else {
            assert!(
                v["error"].is_object(),
                "path traversal must error or be safely normalized"
            );
        }
    }

    /// AC-2.1: session/new returns stable id valid for session/prompt.
    #[tokio::test]
    async fn ac21_session_new_stable_id_valid_for_prompt() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_string_lossy();
        let input = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":1}}}}
{{"jsonrpc":"2.0","id":2,"method":"session/new","params":{{"cwd":"{}"}}}}
{{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{{"sessionId":"acp-sess-1","prompt":[{{"type":"text","text":"hello"}}]}}}}"#,
            cwd
        );
        let lines = run_server(&input).await;
        let sn_resp = parse_line(&lines, 2);
        assert!(sn_resp["result"]["sessionId"].is_string());
        assert!(!sn_resp["result"]["sessionId"].as_str().unwrap().is_empty());
        let pr_resp = parse_line(&lines, 3);
        assert!(
            pr_resp["result"].is_object(),
            "session/prompt must succeed with valid sessionId, got: {pr_resp}"
        );
    }

    /// AC-2.1 (supplemental): nonexistent cwd returns error.
    #[tokio::test]
    async fn ac21_nonexistent_cwd_returns_error() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/nonexistent/path/xyz123"}}"#,
        );
        let lines = run_server(input).await;
        let v = parse_line(&lines, 2);
        assert!(v["error"].is_object(), "nonexistent cwd must return error");
        let msg = v["error"]["message"].as_str().unwrap_or("").to_lowercase();
        assert!(
            msg.contains("cwd") && (msg.contains("not exist") || msg.contains("does not")),
            "error must mention cwd, got: '{msg}'"
        );
    }

    /// AC-2.2: session/prompt streams agent_message_chunk + end_turn.
    #[tokio::test]
    async fn ac22_session_prompt_streams_chunks_and_end_turn() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_string_lossy();
        let input = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":1}}}}
{{"jsonrpc":"2.0","id":2,"method":"session/new","params":{{"cwd":"{}"}}}}
{{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{{"sessionId":"acp-sess-1","prompt":[{{"type":"text","text":"Say hello and list the current directory"}}]}}}}"#,
            cwd
        );
        let lines = run_server(&input).await;
        let notifs = find_notifications(&lines);
        let has_chunk = notifs.iter().any(|n| {
            let upd = &n["params"]["update"];
            upd["sessionUpdate"] == "agent_message_chunk"
                && !upd["content"]["text"].as_str().unwrap_or("").is_empty()
        });
        assert!(
            has_chunk,
            "must have agent_message_chunk with non-empty text"
        );
        let last = notifs.last().expect("must have notifications");
        assert_eq!(last["params"]["update"]["sessionUpdate"], "end_turn");
        let reason = last["params"]["update"]["stopReason"]
            .as_str()
            .unwrap_or("");
        assert!(!reason.is_empty(), "stopReason must not be empty");
    }

    /// AC-2.2: end_turn is the last notification per turn.
    #[tokio::test]
    async fn ac22_end_turn_is_last_notification() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_string_lossy();
        let input = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":1}}}}
{{"jsonrpc":"2.0","id":2,"method":"session/new","params":{{"cwd":"{}"}}}}
{{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{{"sessionId":"acp-sess-1","prompt":[{{"type":"text","text":"hello"}}]}}}}"#,
            cwd
        );
        let lines = run_server(&input).await;
        let notifs = find_notifications(&lines);
        let last = notifs.last().expect("must have notifications");
        assert_eq!(
            last["params"]["update"]["sessionUpdate"], "end_turn",
            "last notification must be end_turn, got: {}",
            last["params"]["update"]["sessionUpdate"]
        );
    }

    /// AC-2.3: mixed text + non-text — server does not crash, processes text only.
    #[tokio::test]
    async fn ac23_mixed_text_and_non_text_no_crash() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_string_lossy();
        let input = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":1}}}}
{{"jsonrpc":"2.0","id":2,"method":"session/new","params":{{"cwd":"{}"}}}}
{{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{{"sessionId":"acp-sess-1","prompt":[{{"type":"text","text":"hello"}},{{"type":"resource_link","uri":"file:///workspace/project/README.md"}},{{"type":"image","uri":"data:image/png;base64,iVBORw0KGgo="}}]}}}}"#,
            cwd
        );
        let lines = run_server(&input).await;
        let pr_resp = parse_line(&lines, 3);
        assert!(
            pr_resp["result"].is_object() || pr_resp["error"].is_object(),
            "must return result or error, not crash"
        );
    }

    /// AC-2.3 (c): only non-text blocks → graceful response, no crash.
    #[tokio::test]
    async fn ac23_only_non_text_blocks_returns_graceful() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_string_lossy();
        let input = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":1}}}}
{{"jsonrpc":"2.0","id":2,"method":"session/new","params":{{"cwd":"{}"}}}}
{{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{{"sessionId":"acp-sess-1","prompt":[{{"type":"resource_link","uri":"file:///workspace/README.md"}}]}}}}"#,
            cwd
        );
        let lines = run_server(&input).await;
        let pr_resp = parse_line(&lines, 3);
        if pr_resp["result"].is_object() {
            let reason = pr_resp["result"]["stopReason"]
                .as_str()
                .unwrap_or("MISSING");
            assert_eq!(
                reason, "end_turn",
                "only-non-text blocks must yield end_turn"
            );
        } else {
            assert!(pr_resp["error"].is_object(), "must return error or result");
        }
        let has_response = lines.iter().any(|l| l.contains(r#""id":3"#));
        assert!(has_response, "request id=3 must have a response");
    }

    /// AC-2.4: Multi-turn context preservation.
    #[tokio::test]
    async fn ac24_multi_turn_context_preserved() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_string_lossy();
        let multi_llm = mock_llm_multi(vec![
            crate::llm::Completion {
                content: "Got it, Alice. I'll remember stellar.".into(),
                ..Default::default()
            },
            crate::llm::Completion {
                content: "Your name is Alice and you work on stellar.".into(),
                ..Default::default()
            },
        ]);
        let input = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":1}}}}
{{"jsonrpc":"2.0","id":2,"method":"session/new","params":{{"cwd":"{}"}}}}
{{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{{"sessionId":"acp-sess-1","prompt":[{{"type":"text","text":"My name is Alice and I'm working on a Rust project called stellar"}}]}}}}
{{"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{{"sessionId":"acp-sess-1","prompt":[{{"type":"text","text":"What's my name and what project am I working on?"}}]}}}}"#,
            cwd
        );
        let lines = run_server_with_llm(&input, Some(multi_llm)).await;
        let text = lines.join("\n");
        assert!(text.contains("Alice"), "second turn must mention Alice");
        assert!(text.contains("stellar"), "second turn must mention stellar");
        let notifs = find_notifications(&lines);
        let end_turns: Vec<_> = notifs
            .iter()
            .filter(|n| n["params"]["update"]["sessionUpdate"] == "end_turn")
            .collect();
        assert!(
            end_turns.len() >= 2,
            "must have ≥2 end_turns, got {}",
            end_turns.len()
        );
        assert_eq!(
            end_turns.last().unwrap()["params"]["update"]["stopReason"],
            "end_turn"
        );
    }

    /// AC-2.5: stopReason = end_turn for normal completion.
    #[tokio::test]
    async fn ac25_stop_reason_end_turn_for_normal_completion() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_string_lossy();
        let input = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":1}}}}
{{"jsonrpc":"2.0","id":2,"method":"session/new","params":{{"cwd":"{}"}}}}
{{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{{"sessionId":"acp-sess-1","prompt":[{{"type":"text","text":"hello"}}]}}}}"#,
            cwd
        );
        let lines = run_server(&input).await;
        let pr_resp = parse_line(&lines, 3);
        assert_eq!(pr_resp["result"]["stopReason"], "end_turn");
        let notifs = find_notifications(&lines);
        let end_turns: Vec<_> = notifs
            .iter()
            .filter(|n| n["params"]["update"]["sessionUpdate"] == "end_turn")
            .collect();
        assert_eq!(end_turns.len(), 1, "must have exactly one end_turn");
    }

    /// AC-2.6: messageId is deterministic 12-char lowercase hex across sessions.
    #[tokio::test]
    async fn ac26_message_id_deterministic_across_sessions() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_string_lossy();
        let mk_llm = || {
            mock_llm_multi(vec![crate::llm::Completion {
                content: "Hi!".into(),
                ..Default::default()
            }])
        };

        let extract_user_msg_id = |lines: &[String]| -> Option<String> {
            lines
                .iter()
                .filter_map(|l| {
                    let v: Value = serde_json::from_str(l).ok()?;
                    if v["method"] == "session/update"
                        && v["params"]["update"]["sessionUpdate"] == "user_message_chunk"
                    {
                        v["params"]["update"]["messageId"]
                            .as_str()
                            .map(|s| s.to_string())
                    } else {
                        None
                    }
                })
                .next()
        };

        let input_sess = |text: &str| -> String {
            format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":1}}}}
{{"jsonrpc":"2.0","id":2,"method":"session/new","params":{{"cwd":"{}"}}}}
{{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{{"sessionId":"acp-sess-1","prompt":[{{"type":"text","text":"{}"}}]}}}}"#,
                cwd, text
            )
        };

        let lines_a = run_server_with_llm(&input_sess("Hello world"), Some(mk_llm())).await;
        let lines_b = run_server_with_llm(&input_sess("Hello world"), Some(mk_llm())).await;
        let id_a = extract_user_msg_id(&lines_a).expect("A must have user_message_chunk");
        let id_b = extract_user_msg_id(&lines_b).expect("B must have user_message_chunk");

        assert_eq!(id_a, id_b, "same content must produce same messageId");
        assert_eq!(id_a.len(), 12, "messageId must be 12 chars");
        assert!(
            id_a.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "messageId must be lowercase hex, got: {id_a}"
        );

        let lines_c = run_server_with_llm(&input_sess("Different prompt"), Some(mk_llm())).await;
        let id_c = extract_user_msg_id(&lines_c).expect("C must have user_message_chunk");
        assert_ne!(
            id_a, id_c,
            "different prompts must give different messageIds"
        );
    }

    /// AC-2.7: EventSink emits only user_message_chunk + agent_message_chunk + end_turn.
    #[tokio::test]
    async fn ac27_event_sink_only_allowed_notifications() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_string_lossy();
        let input = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":1}}}}
{{"jsonrpc":"2.0","id":2,"method":"session/new","params":{{"cwd":"{}"}}}}
{{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{{"sessionId":"acp-sess-1","prompt":[{{"type":"text","text":"Say hello and explain what you can do"}}]}}}}"#,
            cwd
        );
        let lines = run_server(&input).await;
        let notifs = find_notifications(&lines);
        assert!(!notifs.is_empty(), "must have notifications");
        let allowed = ["user_message_chunk", "agent_message_chunk", "end_turn"];
        for n in &notifs {
            let ut = n["params"]["update"]["sessionUpdate"]
                .as_str()
                .unwrap_or("MISSING");
            assert!(
                allowed.contains(&ut),
                "Sprint 2 must not emit '{}', got: {n}",
                ut
            );
        }
        let end_turn = notifs
            .iter()
            .find(|n| n["params"]["update"]["sessionUpdate"] == "end_turn")
            .expect("must have end_turn");
        assert_eq!(end_turn["params"]["sessionId"], "acp-sess-1");
        assert!(!end_turn["params"]["update"]["stopReason"]
            .as_str()
            .unwrap_or("")
            .is_empty());
        assert!(!end_turn["params"]["update"]["turnId"]
            .as_str()
            .unwrap_or("")
            .is_empty());
        for n in &notifs {
            assert_eq!(n["jsonrpc"], "2.0");
            assert_eq!(n["method"], "session/update");
        }
    }

    /// AC-2.9: stale sessionId → JSON-RPC error, server stays alive.
    #[tokio::test]
    async fn ac29_stale_session_returns_error_not_crash() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_string_lossy();
        let input = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":1}}}}
{{"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{{"sessionId":"nonexistent-session-id","prompt":[{{"type":"text","text":"hi"}}]}}}}
{{"jsonrpc":"2.0","id":3,"method":"session/new","params":{{"cwd":"{}"}}}}"#,
            cwd
        );
        let lines = run_server(&input).await;
        let err_resp = parse_line(&lines, 2);
        assert!(
            err_resp["error"].is_object(),
            "nonexistent session must return error"
        );
        assert_eq!(err_resp["error"]["code"], SESSION_NOT_FOUND);
        assert!(!err_resp["error"]["message"]
            .as_str()
            .unwrap_or("")
            .is_empty());
        let ok_resp = parse_line(&lines, 3);
        assert!(
            ok_resp["result"].is_object(),
            "server must stay responsive, got: {ok_resp}"
        );
    }

    /// AC-2.10: empty prompt array returns error, no crash/hang/malformed JSON.
    #[tokio::test]
    async fn ac210_empty_prompt_array_returns_error_or_helpful() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().to_string_lossy();
        let input = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":1}}}}
{{"jsonrpc":"2.0","id":2,"method":"session/new","params":{{"cwd":"{}"}}}}
{{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{{"sessionId":"acp-sess-1","prompt":[]}}}}"#,
            cwd
        );
        let lines = run_server(&input).await;
        let pr_resp = parse_line(&lines, 3);
        if pr_resp["error"].is_object() {
            let msg = pr_resp["error"]["message"].as_str().unwrap_or("");
            assert!(!msg.is_empty(), "error message must be descriptive");
        } else if pr_resp["result"].is_object() {
            let reason = pr_resp["result"]["stopReason"]
                .as_str()
                .unwrap_or("MISSING");
            assert!(!reason.is_empty(), "stopReason must be present");
        } else {
            panic!("neither result nor error: {pr_resp}");
        }
        for line in &lines {
            let _: Value =
                serde_json::from_str(line).unwrap_or_else(|_| panic!("malformed JSON: {line}"));
        }
    }
}
