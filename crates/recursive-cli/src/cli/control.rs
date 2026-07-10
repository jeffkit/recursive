//! Claude Code–compatible bidirectional control channel.
//!
//! Wire shapes match `@anthropic-ai/claude-agent-sdk` (`SDKControlRequest` /
//! `SDKControlResponse` / `SDKControlCancelRequest`).
//!
//! ## Directions
//!
//! * **CLI → host** (we emit, host answers): `can_use_tool`, `hook_callback`,
//!   `elicitation`, `request_user_dialog`.
//! * **Host → CLI** (host emits on stdin, we answer): `interrupt`, `initialize`,
//!   `set_*`, `get_*`, MCP admin, reload, rewind, etc.
//!
//! Stdin also accepts `type: "user"` messages when `--input-format stream-json`
//! is active; they are buffered on [`ControlSession::inbound_user_messages`].

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use recursive::agent::PermissionDecision;
use recursive::permissions::PermissionMode;
use recursive::tools::{AccessTier, PermissionHook, ReadFileState};
use recursive::SessionWriter;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{oneshot, Mutex};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

// ── Bridge (stdout lock + pending CLI→host waiters) ─────────────────────────

/// Shared bridge between stdout emitters and the stdin demuxer.
pub(crate) struct ControlBridge {
    pending: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    stdout: Mutex<()>,
}

impl ControlBridge {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            pending: Mutex::new(HashMap::new()),
            stdout: Mutex::new(()),
        })
    }

    pub(crate) async fn println_locked(&self, value: &Value) {
        let _guard = self.stdout.lock().await;
        match serde_json::to_string(value) {
            Ok(line) => println!("{line}"),
            Err(e) => eprintln!("control: failed to serialise: {e}"),
        }
    }

    async fn register_waiter(&self, request_id: String) -> oneshot::Receiver<Value> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(request_id, tx);
        rx
    }

    /// Emit an arbitrary CLI→host `control_request` and wait for the response body.
    pub(crate) async fn ask_host(&self, request: Value) -> Option<Value> {
        let request_id = Uuid::new_v4().to_string();
        let rx = self.register_waiter(request_id.clone()).await;
        let frame = json!({
            "type": "control_request",
            "request_id": request_id,
            "request": request,
        });
        self.println_locked(&frame).await;
        rx.await.ok()
    }

    pub(crate) async fn ask_can_use_tool(
        &self,
        tool_name: &str,
        input: &Value,
        tool_use_id: Option<&str>,
    ) -> Option<Value> {
        self.ask_host(json!({
            "subtype": "can_use_tool",
            "tool_name": tool_name,
            "input": input,
            "tool_use_id": tool_use_id.unwrap_or(""),
            "blocked_path": Value::Null,
            "decision_reason": Value::Null,
        }))
        .await
    }

    /// Ask the host to run a registered SDK hook callback.
    pub(crate) async fn ask_hook_callback(
        &self,
        callback_id: &str,
        input: Value,
        tool_use_id: Option<&str>,
    ) -> Option<Value> {
        let mut req = json!({
            "subtype": "hook_callback",
            "callback_id": callback_id,
            "input": input,
        });
        if let Some(id) = tool_use_id {
            req.as_object_mut()
                .expect("object")
                .insert("tool_use_id".into(), json!(id));
        }
        self.ask_host(req).await
    }

    /// Forward an MCP elicitation to the host (Claude `elicitation` subtype).
    pub(crate) async fn ask_elicitation(&self, request: Value) -> Option<Value> {
        let mut req = request;
        if let Some(m) = req.as_object_mut() {
            m.insert("subtype".into(), json!("elicitation"));
        }
        self.ask_host(req).await
    }

    pub(crate) async fn ask_user_dialog(
        &self,
        dialog_kind: &str,
        payload: Value,
        tool_use_id: Option<&str>,
    ) -> Option<Value> {
        self.ask_host(json!({
            "subtype": "request_user_dialog",
            "dialog_kind": dialog_kind,
            "payload": payload,
            "tool_use_id": tool_use_id,
        }))
        .await
    }

    /// Resolve a pending CLI→host waiter from a `control_response` line.
    pub(crate) async fn resolve_response(&self, v: &Value) -> bool {
        if v.get("type").and_then(|t| t.as_str()) != Some("control_response") {
            return false;
        }
        let response = match v.get("response") {
            Some(r) => r,
            None => return true,
        };
        let Some(request_id) = response
            .get("request_id")
            .and_then(|id| id.as_str())
            .map(str::to_string)
        else {
            return true;
        };
        let sender = self.pending.lock().await.remove(&request_id);
        if let Some(tx) = sender {
            let payload = response
                .get("response")
                .cloned()
                .unwrap_or_else(|| response.clone());
            let _ = tx.send(payload);
        }
        true
    }

    /// Cancel a pending CLI→host waiter (`control_cancel_request`).
    pub(crate) async fn cancel_pending(&self, request_id: &str) {
        if let Some(tx) = self.pending.lock().await.remove(request_id) {
            let _ = tx.send(json!({
                "behavior": "deny",
                "message": "cancelled by host",
                "interrupt": true,
            }));
        }
    }

    pub(crate) async fn fail_all_pending(&self, message: &str) {
        let mut pending = self.pending.lock().await;
        for (_, tx) in pending.drain() {
            let _ = tx.send(json!({
                "behavior": "deny",
                "message": message,
            }));
        }
    }
}

// ── Session state shared with handlers ──────────────────────────────────────

/// Mutable session state reachable from the stdin control demux without
/// locking the running [`recursive::AgentRuntime`] (avoids deadlock with
/// [`StdioPermissionHook`]).
pub(crate) struct ControlSession {
    pub bridge: Arc<ControlBridge>,
    pub shutdown: CancellationToken,
    pub workspace: PathBuf,
    pub model: Arc<std::sync::Mutex<String>>,
    pub permission_mode: Arc<std::sync::Mutex<PermissionMode>>,
    /// Shared permissions config (same Arc the registry holds), if any.
    pub permissions: Option<recursive::permissions::SharedPermissions>,
    pub session_writer: Option<Arc<std::sync::Mutex<SessionWriter>>>,
    pub read_file_state: Option<Arc<std::sync::Mutex<ReadFileState>>>,
    pub sandbox_roots: Option<recursive::tools::SharedSandboxRoots>,
    pub tools: Arc<std::sync::Mutex<Vec<String>>>,
    pub inbound_user_messages: Arc<std::sync::Mutex<VecDeque<String>>>,
    /// Accept `type:user` on stdin (requires `--input-format stream-json`).
    pub accept_user_messages: bool,
    pub started: std::time::Instant,
    pub api_ms: Arc<std::sync::Mutex<u64>>,
    pub usage: Arc<std::sync::Mutex<recursive::llm::TokenUsage>>,
    /// Optional plan-approval gate so hosts can approve via `request_user_dialog`.
    pub plan_approval_gate: Option<Arc<recursive::PlanApprovalGate>>,
    /// Host-registered SDK hook callbacks from `initialize` (`event` → ids).
    pub sdk_hook_callbacks: Arc<std::sync::Mutex<HashMap<String, Vec<String>>>>,
    /// Set when the stdin demux exits (EOF / error).
    pub stdin_closed: std::sync::atomic::AtomicBool,
}

impl ControlSession {
    fn session_id(&self) -> String {
        self.session_writer
            .as_ref()
            .map(|w| {
                w.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .session_id()
                    .to_string()
            })
            .unwrap_or_else(|| "unknown".into())
    }
}

// ── Response helpers ────────────────────────────────────────────────────────

fn success_response(request_id: &str, body: Value) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": body,
        }
    })
}

fn error_response(request_id: &str, error: impl Into<String>) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "error",
            "request_id": request_id,
            "error": error.into(),
        }
    })
}

fn parse_permission_mode(s: &str) -> Option<PermissionMode> {
    match s {
        "default" => Some(PermissionMode::Default),
        "acceptEdits" | "accept_edits" => Some(PermissionMode::AcceptEdits),
        "bypassPermissions" | "bypass_permissions" => Some(PermissionMode::BypassPermissions),
        "dontAsk" | "dont_ask" => Some(PermissionMode::DontAsk),
        "auto" => Some(PermissionMode::Auto),
        "strict" => Some(PermissionMode::Strict),
        "plan" => Some(PermissionMode::Plan {
            pre_plan_mode: Box::new(PermissionMode::Default),
            bypass_available: false,
        }),
        _ => None,
    }
}

fn permission_mode_wire(mode: &PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Default => "default",
        PermissionMode::AcceptEdits => "acceptEdits",
        PermissionMode::BypassPermissions => "bypassPermissions",
        PermissionMode::DontAsk => "dontAsk",
        PermissionMode::Auto => "auto",
        PermissionMode::Strict => "strict",
        PermissionMode::Plan { .. } => "plan",
    }
}

// ── Host→CLI dispatch ───────────────────────────────────────────────────────

async fn handle_host_control_request(session: &ControlSession, request_id: &str, request: &Value) {
    let subtype = request
        .get("subtype")
        .and_then(|s| s.as_str())
        .unwrap_or("");

    let reply = match subtype {
        "interrupt" => {
            session.shutdown.cancel();
            success_response(request_id, json!({ "still_queued": [] }))
        }
        "initialize" => {
            // Persist host-registered SDK hook callbacks for later forward.
            if let Some(hooks) = request.get("hooks").and_then(|h| h.as_object()) {
                let mut map = session
                    .sdk_hook_callbacks
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                map.clear();
                for (event, matchers) in hooks {
                    let mut ids = Vec::new();
                    if let Some(arr) = matchers.as_array() {
                        for m in arr {
                            if let Some(cb_ids) =
                                m.get("hookCallbackIds").and_then(|v| v.as_array())
                            {
                                for id in cb_ids {
                                    if let Some(s) = id.as_str() {
                                        ids.push(s.to_string());
                                    }
                                }
                            }
                        }
                    }
                    if !ids.is_empty() {
                        map.insert(event.clone(), ids);
                    }
                }
            }
            if let Some(title) = request.get("title").and_then(|t| t.as_str()) {
                if let Some(ref sw) = session.session_writer {
                    sw.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .set_name(title.to_string());
                }
            }
            success_response(
                request_id,
                json!({
                    "commands": [],
                    "agents": [],
                    "output_style": "default",
                    "available_output_styles": ["default"],
                    "models": [{
                        "value": session.model.lock().unwrap_or_else(|e| e.into_inner()).clone(),
                        "displayName": session.model.lock().unwrap_or_else(|e| e.into_inner()).clone(),
                        "description": "active model",
                    }],
                    "account": {
                        "email": Value::Null,
                        "organization": Value::Null,
                        "subscriptionType": Value::Null,
                        "tokenSource": "env",
                    },
                    "skills": [],
                    "plugins": [],
                    "mcp_servers": [],
                    "cwd": session.workspace.display().to_string(),
                    "session_id": session.session_id(),
                    "tools": session.tools.lock().unwrap_or_else(|e| e.into_inner()).clone(),
                    "permissionMode": permission_mode_wire(
                        &session.permission_mode.lock().unwrap_or_else(|e| e.into_inner())
                    ),
                }),
            )
        }
        "set_permission_mode" => {
            let mode_str = request.get("mode").and_then(|m| m.as_str()).unwrap_or("");
            match parse_permission_mode(mode_str) {
                Some(mode) => {
                    *session
                        .permission_mode
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = mode.clone();
                    if let Some(ref sp) = session.permissions {
                        let mut guard = sp.write().await;
                        guard.mode = mode;
                    }
                    success_response(request_id, json!({ "mode": mode_str }))
                }
                None => error_response(request_id, format!("unknown permission mode: {mode_str}")),
            }
        }
        "set_model" => {
            if let Some(model) = request.get("model").and_then(|m| m.as_str()) {
                *session.model.lock().unwrap_or_else(|e| e.into_inner()) = model.to_string();
                // Provider hot-swap is not wired; acknowledge for protocol
                // compatibility. Subsequent get_* / result envelopes reflect
                // the new name.
                success_response(request_id, json!({ "model": model }))
            } else {
                // Clear / keep current
                success_response(
                    request_id,
                    json!({
                        "model": session.model.lock().unwrap_or_else(|e| e.into_inner()).clone()
                    }),
                )
            }
        }
        "set_max_thinking_tokens" => {
            // Recursive has no separate thinking-token budget knob yet.
            success_response(
                request_id,
                json!({
                    "max_thinking_tokens": request.get("max_thinking_tokens"),
                    "thinking_display": request.get("thinking_display"),
                    "applied": false,
                    "note": "thinking budget not enforced by this runtime",
                }),
            )
        }
        "set_color" => success_response(
            request_id,
            json!({ "color": request.get("color").and_then(|c| c.as_str()).unwrap_or("default") }),
        ),
        "rename_session" => {
            let title = request
                .get("title")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            if let Some(ref sw) = session.session_writer {
                sw.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .set_name(title.clone());
                success_response(request_id, json!({ "title": title }))
            } else {
                error_response(request_id, "no active session writer")
            }
        }
        "get_session_cost" => {
            let usage = *session.usage.lock().unwrap_or_else(|e| e.into_inner());
            let model = session
                .model
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let cost = recursive::llm::pricing_for(&model)
                .map(|p| p.cost_usd(usage))
                .unwrap_or(0.0);
            success_response(
                request_id,
                json!({
                    "total_cost_usd": cost,
                    "currency": "USD",
                }),
            )
        }
        "get_usage" => {
            let usage = *session.usage.lock().unwrap_or_else(|e| e.into_inner());
            let model = session
                .model
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let cost = recursive::llm::pricing_for(&model)
                .map(|p| p.cost_usd(usage))
                .unwrap_or(0.0);
            let api_ms = *session.api_ms.lock().unwrap_or_else(|e| e.into_inner());
            success_response(
                request_id,
                json!({
                    "session": {
                        "total_cost_usd": cost,
                        "total_api_duration_ms": api_ms,
                        "total_duration_ms": session.started.elapsed().as_millis() as u64,
                        "total_lines_added": 0,
                        "total_lines_removed": 0,
                        "model_usage": {
                            model: {
                                "inputTokens": usage.prompt_tokens,
                                "outputTokens": usage.completion_tokens,
                                "cacheReadInputTokens": usage.cache_hit_tokens,
                                "cacheCreationInputTokens": usage.cache_miss_tokens,
                                "costUSD": cost,
                            }
                        }
                    },
                    "subscription_type": Value::Null,
                    "rate_limits_available": false,
                    "rate_limits": Value::Null,
                }),
            )
        }
        "get_context_usage" => {
            let usage = *session.usage.lock().unwrap_or_else(|e| e.into_inner());
            let model = session
                .model
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let total = usage.prompt_tokens.saturating_add(usage.completion_tokens);
            success_response(
                request_id,
                json!({
                    "categories": [
                        {"name": "messages", "tokens": usage.prompt_tokens, "color": "blue"},
                        {"name": "output", "tokens": usage.completion_tokens, "color": "green"},
                    ],
                    "totalTokens": total,
                    "maxTokens": 200_000,
                    "rawMaxTokens": 200_000,
                    "percentage": 0,
                    "gridRows": [],
                    "model": model,
                    "memoryFiles": [],
                    "mcpTools": [],
                }),
            )
        }
        "get_settings" => success_response(
            request_id,
            json!({
                "permissionMode": permission_mode_wire(
                    &session.permission_mode.lock().unwrap_or_else(|e| e.into_inner())
                ),
                "model": session.model.lock().unwrap_or_else(|e| e.into_inner()).clone(),
                "cwd": session.workspace.display().to_string(),
            }),
        ),
        "get_plan" => {
            let plan = session.plan_approval_gate.as_ref().and_then(|g| {
                g.pending_plan
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
            });
            success_response(
                request_id,
                json!({
                    "plan": plan,
                }),
            )
        }
        "get_binary_version" => success_response(
            request_id,
            json!({
                "version": env!("CARGO_PKG_VERSION"),
                "name": "recursive",
            }),
        ),
        "list_models" => {
            let model = session
                .model
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            success_response(
                request_id,
                json!({
                    "models": [{
                        "value": model,
                        "displayName": model,
                        "description": "configured model",
                    }]
                }),
            )
        }
        "get_workspace_diff" => match workspace_diff(&session.workspace) {
            Ok(diff) => success_response(request_id, json!({ "diff": diff })),
            Err(e) => error_response(request_id, e),
        },
        "read_file" => match control_read_file(session, request) {
            Ok(body) => success_response(request_id, body),
            Err(e) => error_response(request_id, e),
        },
        "seed_read_state" => match seed_read_state(session, request) {
            Ok(()) => success_response(request_id, json!({ "ok": true })),
            Err(e) => error_response(request_id, e),
        },
        "register_repo_root" => match register_repo_root(session, request) {
            Ok(path) => success_response(request_id, json!({ "directory": path })),
            Err(e) => error_response(request_id, e),
        },
        "file_suggestions" => {
            let query = request.get("query").and_then(|q| q.as_str()).unwrap_or("");
            let suggestions = file_suggestions(&session.workspace, query, 20);
            success_response(request_id, json!({ "suggestions": suggestions }))
        }
        "reload_skills" => {
            let skills = discover_loaded_skills_for_workspace(&session.workspace);
            // Discovery only — live registry reload mid-run is best-effort.
            let names: Vec<String> = skills.into_iter().map(|s| s.name).collect();
            success_response(
                request_id,
                json!({
                    "skills": names.iter().map(|n| json!({
                        "name": n,
                        "description": "",
                    })).collect::<Vec<_>>(),
                }),
            )
        }
        "reload_plugins" => success_response(
            request_id,
            json!({
                "commands": [],
                "agents": [],
                "plugins": [],
                "mcpServers": [],
                "error_count": 0,
                "note": "recursive has no plugin system; returned empty sets",
            }),
        ),
        "apply_flag_settings" => {
            // Shallow-merge recognised keys.
            if let Some(settings) = request.get("settings").and_then(|s| s.as_object()) {
                if let Some(mode) = settings.get("permissionMode").and_then(|m| m.as_str()) {
                    if let Some(parsed) = parse_permission_mode(mode) {
                        *session
                            .permission_mode
                            .lock()
                            .unwrap_or_else(|e| e.into_inner()) = parsed.clone();
                        if let Some(ref sp) = session.permissions {
                            let mut g = sp.write().await;
                            g.mode = parsed;
                        }
                    }
                }
                if let Some(model) = settings.get("model").and_then(|m| m.as_str()) {
                    *session.model.lock().unwrap_or_else(|e| e.into_inner()) = model.to_string();
                }
            }
            success_response(request_id, json!({ "ok": true }))
        }
        "rewind_files" => match rewind_files(session, request) {
            Ok(body) => success_response(request_id, body),
            Err(e) => error_response(request_id, e),
        },
        "cancel_async_message" => {
            // No async user-message queue yet; acknowledge.
            success_response(request_id, json!({ "cancelled": false }))
        }
        "background_tasks" => success_response(
            request_id,
            json!({
                "ok": true,
                "note": "backgrounding via control is a no-op; use RunBackground tool",
            }),
        ),
        "stop_task" => {
            let task_id = request
                .get("task_id")
                .and_then(|t| t.as_str())
                .unwrap_or("");
            success_response(
                request_id,
                json!({
                    "task_id": task_id,
                    "stopped": false,
                    "note": "use TaskStop / CheckBackground tools; control stop is best-effort",
                }),
            )
        }
        "mcp_status" => success_response(
            request_id,
            json!({
                "servers": list_mcp_servers_status(&session.workspace),
            }),
        ),
        "mcp_set_servers" | "mcp_reconnect" | "mcp_toggle" | "mcp_call" | "mcp_message" => {
            // Dynamic MCP mutation mid-run is not supported; keep the wire
            // contract so hosts don't hang.
            success_response(
                request_id,
                json!({
                    "ok": false,
                    "note": format!("{subtype} acknowledged but not applied mid-run"),
                }),
            )
        }
        // CLI→host subtypes should not arrive as host→CLI; reject clearly.
        "can_use_tool" | "hook_callback" | "elicitation" | "request_user_dialog" => error_response(
            request_id,
            format!("{subtype} is a CLI→host request; do not send it on stdin"),
        ),
        other => error_response(request_id, format!("unsupported control subtype: {other}")),
    };

    session.bridge.println_locked(&reply).await;
}

fn workspace_diff(workspace: &Path) -> Result<String, String> {
    let output = std::process::Command::new("git")
        .args(["diff", "HEAD"])
        .current_dir(workspace)
        .output()
        .map_err(|e| format!("git diff failed: {e}"))?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn control_read_file(session: &ControlSession, request: &Value) -> Result<Value, String> {
    let path = request
        .get("path")
        .and_then(|p| p.as_str())
        .ok_or_else(|| "missing path".to_string())?;
    let max_bytes = request
        .get("max_bytes")
        .and_then(|n| n.as_u64())
        .unwrap_or(512 * 1024) as usize;
    let encoding = request
        .get("encoding")
        .and_then(|e| e.as_str())
        .unwrap_or("utf-8");

    let abs =
        recursive::tools::resolve_within(&session.workspace, path).map_err(|e| e.to_string())?;
    let bytes = std::fs::read(&abs).map_err(|e| format!("read failed: {e}"))?;
    let truncated = bytes.len() > max_bytes;
    let slice = if truncated {
        &bytes[..max_bytes]
    } else {
        &bytes
    };

    let contents = if encoding == "base64" {
        base64_encode(slice)
    } else {
        String::from_utf8_lossy(slice).into_owned()
    };

    let mut body = json!({
        "contents": contents,
        "absPath": abs.display().to_string(),
    });
    if truncated {
        body.as_object_mut()
            .expect("object")
            .insert("truncated".into(), json!(true));
    }
    if encoding == "base64" {
        body.as_object_mut()
            .expect("object")
            .insert("encoding".into(), json!("base64"));
    }
    Ok(body)
}

fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn seed_read_state(session: &ControlSession, request: &Value) -> Result<(), String> {
    let path = request
        .get("path")
        .and_then(|p| p.as_str())
        .ok_or_else(|| "missing path".to_string())?;
    let abs =
        recursive::tools::resolve_within(&session.workspace, path).map_err(|e| e.to_string())?;
    let state = session
        .read_file_state
        .as_ref()
        .ok_or_else(|| "read_file_state not available".to_string())?;
    state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .record(abs, false);
    Ok(())
}

fn register_repo_root(session: &ControlSession, request: &Value) -> Result<String, String> {
    let dir = request
        .get("directory")
        .and_then(|d| d.as_str())
        .ok_or_else(|| "missing directory".to_string())?;
    let path = PathBuf::from(dir);
    if !path.is_absolute() {
        return Err("directory must be absolute".into());
    }
    let roots = session
        .sandbox_roots
        .as_ref()
        .ok_or_else(|| "sandbox roots not available on this run".to_string())?;
    let mut guard = roots.write().map_err(|e| e.to_string())?;
    if !guard.iter().any(|(p, _)| p == &path) {
        guard.push((path.clone(), AccessTier::ReadWrite));
    }
    Ok(path.display().to_string())
}

fn file_suggestions(workspace: &Path, query: &str, limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    let walker = walkdir_shallow(workspace, 4);
    let q = query.to_ascii_lowercase();
    for path in walker {
        let rel = path
            .strip_prefix(workspace)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| path.display().to_string());
        if q.is_empty() || rel.to_ascii_lowercase().contains(&q) {
            out.push(rel);
            if out.len() >= limit {
                break;
            }
        }
    }
    out
}

fn walkdir_shallow(root: &Path, max_depth: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || name == "target" || name == "node_modules" {
                continue;
            }
            if path.is_dir() {
                if depth < max_depth {
                    stack.push((path, depth + 1));
                }
            } else {
                out.push(path);
            }
        }
    }
    out
}

fn rewind_files(session: &ControlSession, request: &Value) -> Result<Value, String> {
    let dry_run = request
        .get("dry_run")
        .and_then(|d| d.as_bool())
        .unwrap_or(false);
    // Without a turn index we can only report capability.
    let _msg_id = request.get("user_message_id").and_then(|u| u.as_str());
    if dry_run {
        return Ok(json!({
            "dry_run": true,
            "would_rewind": false,
            "note": "use `recursive sessions rewind --to-turn N` for full rewind",
        }));
    }
    let _ = session;
    Err("rewind_files requires a turn index; use `recursive sessions rewind --to-turn N`".into())
}

fn list_mcp_servers_status(workspace: &Path) -> Vec<Value> {
    let path = workspace.join(".mcp.json");
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let Some(servers) = v.get("mcpServers").and_then(|s| s.as_object()) else {
        return Vec::new();
    };
    servers
        .keys()
        .map(|name| json!({ "name": name, "status": "configured" }))
        .collect()
}

// ── Construction helpers ────────────────────────────────────────────────────

impl ControlSession {
    /// Build a control session for a JSON-mode CLI run.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        bridge: Arc<ControlBridge>,
        shutdown: CancellationToken,
        workspace: PathBuf,
        model: String,
        permission_mode: PermissionMode,
        permissions: Option<recursive::permissions::SharedPermissions>,
        session_writer: Option<Arc<std::sync::Mutex<SessionWriter>>>,
        read_file_state: Option<Arc<std::sync::Mutex<ReadFileState>>>,
        sandbox_roots: Option<recursive::tools::SharedSandboxRoots>,
        tools: Vec<String>,
        accept_user_messages: bool,
        plan_approval_gate: Option<Arc<recursive::PlanApprovalGate>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            bridge,
            shutdown,
            workspace,
            model: Arc::new(std::sync::Mutex::new(model)),
            permission_mode: Arc::new(std::sync::Mutex::new(permission_mode)),
            permissions,
            session_writer,
            read_file_state,
            sandbox_roots,
            tools: Arc::new(std::sync::Mutex::new(tools)),
            inbound_user_messages: Arc::new(std::sync::Mutex::new(VecDeque::new())),
            accept_user_messages,
            started: std::time::Instant::now(),
            api_ms: Arc::new(std::sync::Mutex::new(0)),
            usage: Arc::new(std::sync::Mutex::new(recursive::llm::TokenUsage::default())),
            plan_approval_gate,
            sdk_hook_callbacks: Arc::new(std::sync::Mutex::new(HashMap::new())),
            stdin_closed: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Pop the next inbound user message (FIFO), if any.
    pub(crate) fn pop_inbound_user(&self) -> Option<String> {
        self.inbound_user_messages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
    }

    pub(crate) fn is_stdin_closed(&self) -> bool {
        self.stdin_closed.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Record cumulative usage for `get_usage` / `get_session_cost`.
    pub(crate) fn record_usage(&self, usage: recursive::llm::TokenUsage, api_ms: u64) {
        {
            let mut g = self.usage.lock().unwrap_or_else(|e| e.into_inner());
            *g = g.accumulate(usage);
        }
        {
            let mut g = self.api_ms.lock().unwrap_or_else(|e| e.into_inner());
            *g = g.saturating_add(api_ms);
        }
    }
}

/// Watch [`PlanApprovalGate`] and ask the host via `request_user_dialog`
/// when a plan is pending. Runs until `shutdown` is cancelled.
pub(crate) async fn plan_dialog_loop(session: Arc<ControlSession>) {
    let Some(gate) = session.plan_approval_gate.clone() else {
        return;
    };
    let mut last_asked: Option<String> = None;
    loop {
        tokio::select! {
            _ = session.shutdown.cancelled() => break,
            _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {}
        }
        let pending = gate
            .pending_plan
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(plan) = pending else {
            last_asked = None;
            continue;
        };
        if last_asked.as_deref() == Some(plan.as_str()) {
            continue;
        }
        last_asked = Some(plan.clone());
        let answer = session
            .bridge
            .ask_user_dialog("plan_approval", json!({ "plan": plan }), None)
            .await;
        match answer {
            Some(v) => {
                let behavior = v
                    .get("behavior")
                    .and_then(|b| b.as_str())
                    .unwrap_or("cancelled");
                match behavior {
                    "allow" | "approved" | "approve" => gate.approve(),
                    "deny" | "rejected" | "reject" => {
                        let reason = v
                            .get("message")
                            .or_else(|| v.get("reason"))
                            .and_then(|m| m.as_str())
                            .unwrap_or("rejected by host")
                            .to_string();
                        gate.reject(reason);
                    }
                    _ => gate.reject("cancelled by host"),
                }
            }
            None => {
                gate.reject("permission channel closed");
                break;
            }
        }
    }
}

// ── Stdin loop ──────────────────────────────────────────────────────────────

/// Demux stdin: `control_response`, `control_request`, `control_cancel_request`,
/// and optional `user` messages.
pub(crate) async fn stdin_control_loop(session: Arc<ControlSession>) {
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                handle_stdin_line(&session, &line).await;
            }
            Ok(None) => break,
            Err(e) => {
                eprintln!("control: stdin read error: {e}");
                break;
            }
        }
    }
    session
        .stdin_closed
        .store(true, std::sync::atomic::Ordering::SeqCst);
    session
        .bridge
        .fail_all_pending("stdin closed before permission response")
        .await;
}

async fn handle_stdin_line(session: &ControlSession, line: &str) {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return;
    };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("control_response") => {
            let _ = session.bridge.resolve_response(&v).await;
        }
        Some("control_cancel_request") => {
            if let Some(id) = v.get("request_id").and_then(|i| i.as_str()) {
                session.bridge.cancel_pending(id).await;
            }
        }
        Some("control_request") => {
            let request_id = v
                .get("request_id")
                .and_then(|i| i.as_str())
                .unwrap_or("")
                .to_string();
            let request = v.get("request").cloned().unwrap_or(json!({}));
            handle_host_control_request(session, &request_id, &request).await;
        }
        Some("user") if session.accept_user_messages => {
            if let Some(text) = extract_user_text(&v) {
                session
                    .inbound_user_messages
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(text);
            }
        }
        _ => {}
    }
}

fn extract_user_text(v: &Value) -> Option<String> {
    let message = v.get("message")?;
    if let Some(s) = message.get("content").and_then(|c| c.as_str()) {
        return Some(s.to_string());
    }
    let arr = message.get("content")?.as_array()?;
    let mut parts = Vec::new();
    for block in arr {
        if block.get("type").and_then(|t| t.as_str()) == Some("text") {
            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                parts.push(t.to_string());
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

// ── Permission hook ─────────────────────────────────────────────────────────

pub(crate) struct StdioPermissionHook {
    bridge: Arc<ControlBridge>,
}

impl StdioPermissionHook {
    pub(crate) fn new(bridge: Arc<ControlBridge>) -> Self {
        Self { bridge }
    }
}

#[async_trait]
impl PermissionHook for StdioPermissionHook {
    async fn check(&self, tool_name: &str, args: &Value) -> PermissionDecision {
        match self.bridge.ask_can_use_tool(tool_name, args, None).await {
            None => {
                PermissionDecision::Deny("permission channel closed (no control_response)".into())
            }
            Some(result) => parse_permission_result(&result),
        }
    }
}

pub(crate) fn parse_permission_result(result: &Value) -> PermissionDecision {
    match result.get("behavior").and_then(|b| b.as_str()) {
        Some("allow") => {
            if let Some(updated) = result
                .get("updatedInput")
                .or_else(|| result.get("updated_input"))
            {
                if !updated.is_null() {
                    return PermissionDecision::Transform(updated.clone());
                }
            }
            PermissionDecision::Allow
        }
        Some("deny") => {
            let message = result
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("denied by host");
            PermissionDecision::Deny(message.to_string())
        }
        _ if result.get("allowed").and_then(|a| a.as_bool()) == Some(true) => {
            PermissionDecision::Allow
        }
        _ if result.get("allowed").and_then(|a| a.as_bool()) == Some(false) => {
            let message = result
                .get("reason")
                .and_then(|m| m.as_str())
                .unwrap_or("denied by host");
            PermissionDecision::Deny(message.to_string())
        }
        _ => PermissionDecision::Deny(format!("unrecognised permission response: {result}")),
    }
}

// ── Helpers used by builder ─────────────────────────────────────────────────

/// Discover skills for a workspace (control `reload_skills` / init).
pub(crate) fn discover_loaded_skills_for_workspace(
    workspace: &Path,
) -> Vec<recursive::skills::Skill> {
    // Re-use the same path logic as builder without needing a full Config.
    let mut paths = vec![
        workspace.join(".recursive").join("skills"),
        workspace.join(".claude").join("skills"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        paths.push(PathBuf::from(&home).join(".recursive").join("skills"));
        paths.push(PathBuf::from(home).join(".claude").join("skills"));
    }
    recursive::skills::discover_skills(&paths)
}

// ── CLI→host adapters ───────────────────────────────────────────────────────

/// Forwards [`recursive::hooks::external::HookInput`] to the host as
/// `hook_callback` control requests (one per registered callback id).
pub(crate) struct ControlSdkHookForwarder {
    session: Arc<ControlSession>,
}

impl ControlSdkHookForwarder {
    pub(crate) fn new(session: Arc<ControlSession>) -> Self {
        Self { session }
    }
}

#[async_trait]
impl recursive::SdkHookForwarder for ControlSdkHookForwarder {
    async fn forward(
        &self,
        input: &recursive::hooks::external::HookInput,
    ) -> Option<recursive::hooks::HookResult> {
        let event_key = serde_json::to_string(&input.event)
            .ok()
            .map(|s| s.trim_matches('"').to_string())
            .unwrap_or_default();
        // Claude SDK uses PascalCase event names (PreToolUse etc.); we store
        // whatever the host sent in initialize. Try both camelCase wire and
        // the serde name.
        let ids = {
            let map = self
                .session
                .sdk_hook_callbacks
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            map.get(&event_key).cloned().or_else(|| {
                // Also try common Claude aliases
                let aliases: &[&str] = match event_key.as_str() {
                    "preToolCall" => &["PreToolUse", "PreToolCall"],
                    "postToolCall" => &["PostToolUse", "PostToolCall"],
                    "permissionRequest" => &["PermissionRequest"],
                    "userPromptSubmit" => &["UserPromptSubmit"],
                    "sessionStart" => &["SessionStart"],
                    "sessionEnd" => &["SessionEnd"],
                    _ => return None,
                };
                for a in aliases {
                    if let Some(v) = map.get(*a) {
                        return Some(v.clone());
                    }
                }
                None
            })
        };
        let Some(ids) = ids else {
            return None; // fall through to local hooks
        };
        if ids.is_empty() {
            return None;
        }
        let input_json = serde_json::to_value(input).unwrap_or(json!({}));
        let mut last = recursive::hooks::HookResult::continue_default();
        for id in ids {
            let resp = self
                .session
                .bridge
                .ask_hook_callback(&id, input_json.clone(), None)
                .await?;
            last = hook_result_from_host(&resp);
            if !matches!(last.action, recursive::hooks::HookAction::Continue) {
                return Some(last);
            }
        }
        Some(last)
    }
}

fn hook_result_from_host(v: &Value) -> recursive::hooks::HookResult {
    // Accept both Claude SDK shapes and Recursive external-hook shapes.
    let action_str = v
        .get("action")
        .or_else(|| v.get("decision"))
        .and_then(|a| a.as_str())
        .unwrap_or("continue");
    let action = match action_str {
        "skip" | "block" => recursive::hooks::HookAction::Skip,
        "error" | "deny" => recursive::hooks::HookAction::Error(
            v.get("message")
                .or_else(|| v.get("reason"))
                .and_then(|m| m.as_str())
                .unwrap_or("blocked by host hook")
                .to_string(),
        ),
        _ => recursive::hooks::HookAction::Continue,
    };
    recursive::hooks::HookResult {
        action,
        additional_context: v
            .get("additionalContext")
            .or_else(|| v.get("additional_context"))
            .and_then(|c| c.as_str())
            .map(str::to_string),
        updated_input: v
            .get("updatedInput")
            .or_else(|| v.get("updated_input"))
            .cloned(),
        system_message: v
            .get("systemMessage")
            .or_else(|| v.get("system_message"))
            .and_then(|m| m.as_str())
            .map(str::to_string),
        suppress_output: v
            .get("suppressOutput")
            .and_then(|b| b.as_bool())
            .unwrap_or(false),
        permission_decision: None,
        permission_decision_reason: None,
    }
}

/// Forwards MCP elicitation to the host.
pub(crate) struct ControlElicitationHandler {
    bridge: Arc<ControlBridge>,
}

impl ControlElicitationHandler {
    pub(crate) fn new(bridge: Arc<ControlBridge>) -> Self {
        Self { bridge }
    }
}

#[async_trait]
impl recursive::mcp::ElicitationHandler for ControlElicitationHandler {
    async fn elicit(&self, request: recursive::mcp::ElicitationRequest) -> Option<Value> {
        self.bridge
            .ask_elicitation(json!({
                "mcp_server_name": request.mcp_server_name,
                "message": request.message,
                "mode": request.mode,
                "url": request.url,
                "elicitation_id": request.elicitation_id,
                "requested_schema": request.requested_schema,
                "title": request.title,
                "display_name": request.display_name,
                "description": request.description,
            }))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_allow_deny_transform() {
        assert!(matches!(
            parse_permission_result(&json!({"behavior":"allow"})),
            PermissionDecision::Allow
        ));
        match parse_permission_result(&json!({
            "behavior": "allow",
            "updatedInput": {"path": "x"}
        })) {
            PermissionDecision::Transform(v) => assert_eq!(v["path"], "x"),
            other => panic!("expected Transform, got {other:?}"),
        }
        match parse_permission_result(&json!({
            "behavior": "deny",
            "message": "nope"
        })) {
            PermissionDecision::Deny(m) => assert_eq!(m, "nope"),
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn permission_mode_roundtrip() {
        for s in [
            "default",
            "acceptEdits",
            "bypassPermissions",
            "dontAsk",
            "auto",
            "plan",
        ] {
            let mode = parse_permission_mode(s).expect(s);
            assert_eq!(permission_mode_wire(&mode), s);
        }
    }

    #[test]
    fn base64_encode_smoke() {
        assert_eq!(base64_encode(b"hi"), "aGk=");
        assert_eq!(base64_encode(b"abc"), "YWJj");
    }

    #[tokio::test]
    async fn bridge_resolves_matching_response() {
        let bridge = ControlBridge::new();
        let bridge2 = bridge.clone();
        let ask = tokio::spawn(async move {
            bridge2
                .ask_can_use_tool("Write", &json!({"path":"a"}), Some("toolu_1"))
                .await
        });
        tokio::task::yield_now().await;
        let request_id = {
            let pending = bridge.pending.lock().await;
            pending.keys().next().cloned()
        }
        .expect("asker registered");
        let line = json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": { "behavior": "allow" }
            }
        });
        assert!(bridge.resolve_response(&line).await);
        let result = ask.await.expect("join").expect("response");
        assert_eq!(result["behavior"], "allow");
    }

    #[tokio::test]
    async fn interrupt_cancels_token() {
        let shutdown = CancellationToken::new();
        let session = ControlSession::new(
            ControlBridge::new(),
            shutdown.clone(),
            std::env::temp_dir(),
            "m".into(),
            PermissionMode::Default,
            None,
            None,
            None,
            None,
            vec![],
            false,
            None,
        );
        handle_host_control_request(&session, "req1", &json!({"subtype":"interrupt"})).await;
        assert!(shutdown.is_cancelled());
    }

    #[test]
    fn extract_user_text_from_blocks() {
        let v = json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{"type":"text","text":"hello"}]
            }
        });
        assert_eq!(extract_user_text(&v).as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn supported_host_subtypes_are_not_unsupported() {
        let shutdown = CancellationToken::new();
        let session = ControlSession::new(
            ControlBridge::new(),
            shutdown,
            std::env::temp_dir(),
            "m".into(),
            PermissionMode::Default,
            None,
            None,
            None,
            None,
            vec!["Read".into()],
            false,
            None,
        );
        let subtypes = [
            "interrupt",
            "initialize",
            "set_permission_mode",
            "set_model",
            "set_max_thinking_tokens",
            "rename_session",
            "set_color",
            "mcp_status",
            "get_context_usage",
            "get_session_cost",
            "list_models",
            "get_usage",
            "get_binary_version",
            "mcp_call",
            "file_suggestions",
            "mcp_message",
            "rewind_files",
            "cancel_async_message",
            "get_workspace_diff",
            "get_plan",
            "seed_read_state",
            "mcp_set_servers",
            "register_repo_root",
            "reload_plugins",
            "reload_skills",
            "mcp_reconnect",
            "mcp_toggle",
            "stop_task",
            "background_tasks",
            "apply_flag_settings",
            "get_settings",
        ];
        for subtype in subtypes {
            let req = match subtype {
                "set_permission_mode" => json!({"subtype": subtype, "mode": "default"}),
                "set_model" => json!({"subtype": subtype, "model": "m"}),
                "rename_session" => json!({"subtype": subtype, "title": "t"}),
                "file_suggestions" => json!({"subtype": subtype, "query": ""}),
                "seed_read_state" => json!({"subtype": subtype, "path": "Cargo.toml"}),
                "register_repo_root" => json!({"subtype": subtype, "directory": "/tmp"}),
                "apply_flag_settings" => json!({"subtype": subtype, "settings": {}}),
                "rewind_files" => json!({"subtype": subtype, "dry_run": true}),
                "stop_task" => json!({"subtype": subtype, "task_id": "x"}),
                "mcp_call" | "mcp_message" | "mcp_reconnect" | "mcp_toggle" | "mcp_set_servers" => {
                    json!({"subtype": subtype})
                }
                _ => json!({"subtype": subtype}),
            };
            handle_host_control_request(&session, "t", &req).await;
        }
    }

    #[test]
    fn hook_result_from_host_maps_actions() {
        let cont = hook_result_from_host(&json!({"action": "continue"}));
        assert!(matches!(
            cont.action,
            recursive::hooks::HookAction::Continue
        ));
        let skip = hook_result_from_host(&json!({"action": "block"}));
        assert!(matches!(skip.action, recursive::hooks::HookAction::Skip));
        match hook_result_from_host(&json!({"action": "deny", "message": "no"})) {
            recursive::hooks::HookResult {
                action: recursive::hooks::HookAction::Error(m),
                ..
            } => assert_eq!(m, "no"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn initialize_stores_hook_callbacks() {
        let shutdown = CancellationToken::new();
        let session = ControlSession::new(
            ControlBridge::new(),
            shutdown,
            std::env::temp_dir(),
            "m".into(),
            PermissionMode::Default,
            None,
            None,
            None,
            None,
            vec![],
            false,
            None,
        );
        handle_host_control_request(
            &session,
            "init1",
            &json!({
                "subtype": "initialize",
                "hooks": {
                    "PreToolUse": [{
                        "hookCallbackIds": ["cb_a", "cb_b"]
                    }]
                }
            }),
        )
        .await;
        let map = session
            .sdk_hook_callbacks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            map.get("PreToolUse").map(Vec::as_slice),
            Some(["cb_a".into(), "cb_b".into()].as_slice())
        );
    }

    #[test]
    fn pop_inbound_user_fifo() {
        let session = ControlSession::new(
            ControlBridge::new(),
            CancellationToken::new(),
            std::env::temp_dir(),
            "m".into(),
            PermissionMode::Default,
            None,
            None,
            None,
            None,
            vec![],
            true,
            None,
        );
        session
            .inbound_user_messages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back("one".into());
        session
            .inbound_user_messages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back("two".into());
        assert_eq!(session.pop_inbound_user().as_deref(), Some("one"));
        assert_eq!(session.pop_inbound_user().as_deref(), Some("two"));
        assert_eq!(session.pop_inbound_user(), None);
    }
}
