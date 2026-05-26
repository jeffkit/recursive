//! HTTP API server for the Recursive agent.
//!
//! Provides a lightweight axum-based HTTP server that exposes the agent's
//! tool registry as a read-only JSON endpoint, a health check, a POST /run
//! endpoint that executes the agent with a given goal, session management
//! endpoints for multi-turn conversations, and SSE streaming of agent events.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::sse::{Event, Sse},
    routing::{get, post},
    Json, Router,
};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::{broadcast, RwLock};
use tokio_stream::{wrappers::BroadcastStream, StreamExt};

use crate::agent::StepEvent;
use crate::config::Config;
use crate::llm::LlmProvider;
use crate::message::Message;

// ── Session types ──────────────────────────────────────────────────────────

/// Internal session state (not directly serialized to clients).
#[derive(Clone)]
pub struct SessionState {
    pub id: String,
    pub created_at: String,
    pub transcript: Vec<Message>,
    pub system_prompt: String,
}

/// Serialized session info for list/detail endpoints.
#[derive(Clone, serde::Serialize, serde::Deserialize, Debug)]
pub struct SessionInfo {
    pub id: String,
    pub created_at: String,
    pub message_count: usize,
}

/// Request body for `POST /sessions`.
#[derive(serde::Deserialize, Debug)]
pub struct CreateSessionRequest {
    pub system_prompt: Option<String>,
}

/// Response body for `POST /sessions`.
#[derive(serde::Serialize, Debug)]
pub struct CreateSessionResponse {
    pub id: String,
    pub created_at: String,
}

/// Request body for `POST /sessions/:id/messages`.
#[derive(serde::Deserialize, Debug)]
pub struct SessionMessageRequest {
    pub content: String,
}

/// Response body for `POST /sessions/:id/messages`.
#[derive(serde::Serialize, Debug)]
pub struct SessionMessageResponse {
    pub role: String,
    pub content: String,
}

/// Detail response for `GET /sessions/:id`.
#[derive(serde::Serialize, Debug)]
pub struct SessionDetailResponse {
    pub id: String,
    pub created_at: String,
    pub messages: Vec<serde_json::Value>,
}

// ── SSE event types ──────────────────────────────────────────────────────

/// Server-Sent Event payload emitted during an agent session run.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SseEvent {
    /// A tool is being called.
    ToolCall { name: String, step: usize },
    /// A tool call completed.
    ToolResult { name: String, success: bool },
    /// The agent run completed.
    Done {
        finish_reason: String,
        total_steps: usize,
    },
    /// An error occurred.
    Error { message: String },
}

// ── App state ──────────────────────────────────────────────────────────────

/// Shared application state for the HTTP server.
#[derive(Clone)]
pub struct AppState {
    pub tools: Vec<ToolInfo>,
    pub config: Config,
    pub provider: Arc<dyn LlmProvider>,
    pub sessions: Arc<RwLock<HashMap<String, SessionState>>>,
    pub event_channels: Arc<RwLock<HashMap<String, broadcast::Sender<SseEvent>>>>,
}

/// Serializable tool info for the `/tools` endpoint.
#[derive(Clone, serde::Serialize, serde::Deserialize, Debug)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Request body for `POST /run`.
#[derive(serde::Deserialize, Debug)]
pub struct RunRequest {
    pub goal: String,
    pub max_steps: Option<u32>,
    pub system_prompt: Option<String>,
}

/// Successful response from `POST /run`.
#[derive(serde::Serialize, Debug)]
pub struct RunResponse {
    pub status: String,
    pub finish_reason: String,
    pub messages: Vec<serde_json::Value>,
    pub usage: UsageInfo,
}

/// Token/step usage information.
#[derive(serde::Serialize, Debug)]
pub struct UsageInfo {
    pub total_steps: u32,
    pub total_tokens: u64,
}

/// Error response body.
#[derive(serde::Serialize, Debug)]
pub struct ErrorResponse {
    pub status: String,
    pub error: String,
}

/// Build the axum [`Router`] with all API routes.
///
/// Routes:
/// - `GET /health` — returns `"ok"` (200)
/// - `GET /tools` — returns JSON array of [`ToolInfo`]
/// - `POST /run` — runs the agent with a goal and returns the outcome
/// - `POST /sessions` — create a new session
/// - `GET /sessions` — list all sessions
/// - `GET /sessions/:id` — get session detail with messages
/// - `POST /sessions/:id/messages` — send a message in a session
/// - `DELETE /sessions/:id` — remove a session
/// - `GET /sessions/:id/events` — SSE stream of agent events for a session
/// - `GET /openapi.json` — returns the OpenAPI 3.0.3 specification
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/tools", get(list_tools))
        .route("/run", post(run_agent))
        .route("/sessions", post(create_session))
        .route("/sessions", get(list_sessions))
        .route("/sessions/{id}", get(get_session))
        .route("/sessions/{id}", axum::routing::delete(delete_session))
        .route("/sessions/{id}/messages", post(send_session_message))
        .route("/sessions/{id}/events", get(session_events))
        .route("/openapi.json", get(openapi_spec))
        .with_state(Arc::new(state))
}

async fn health() -> &'static str {
    "ok"
}

async fn openapi_spec() -> Json<serde_json::Value> {
    Json(build_openapi_spec())
}

/// Build a static OpenAPI 3.0.3 specification describing all API endpoints.
pub fn build_openapi_spec() -> serde_json::Value {
    serde_json::json!({
        "openapi": "3.0.3",
        "info": {
            "title": "Recursive Agent API",
            "version": "0.4.0",
            "description": "HTTP API for the Recursive coding agent"
        },
        "paths": {
            "/health": {
                "get": {
                    "summary": "Health check",
                    "description": "Returns 'ok' if the server is running.",
                    "responses": {
                        "200": {
                            "description": "Server is healthy",
                            "content": {
                                "text/plain": {
                                    "schema": { "type": "string", "example": "ok" }
                                }
                            }
                        }
                    }
                }
            },
            "/tools": {
                "get": {
                    "summary": "List registered tools",
                    "description": "Returns the JSON array of tools available to the agent.",
                    "responses": {
                        "200": {
                            "description": "Array of tool descriptors",
                            "content": {
                                "application/json": {
                                    "schema": {
                                        "type": "array",
                                        "items": { "$ref": "#/components/schemas/ToolInfo" }
                                    }
                                }
                            }
                        }
                    }
                }
            },
            "/run": {
                "post": {
                    "summary": "Run the agent",
                    "description": "Execute the agent with a goal and return the outcome.",
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": { "$ref": "#/components/schemas/RunRequest" }
                            }
                        }
                    },
                    "responses": {
                        "200": {
                            "description": "Agent completed successfully",
                            "content": {
                                "application/json": {
                                    "schema": { "$ref": "#/components/schemas/RunResponse" }
                                }
                            }
                        },
                        "400": {
                            "description": "Invalid request (e.g. empty goal)",
                            "content": {
                                "application/json": {
                                    "schema": { "$ref": "#/components/schemas/ErrorResponse" }
                                }
                            }
                        },
                        "422": { "description": "Request body failed deserialization" },
                        "500": {
                            "description": "Internal server error",
                            "content": {
                                "application/json": {
                                    "schema": { "$ref": "#/components/schemas/ErrorResponse" }
                                }
                            }
                        }
                    }
                }
            },
            "/sessions": {
                "get": {
                    "summary": "List sessions",
                    "description": "Returns all active sessions.",
                    "responses": {
                        "200": {
                            "description": "Array of session info objects",
                            "content": {
                                "application/json": {
                                    "schema": {
                                        "type": "array",
                                        "items": { "$ref": "#/components/schemas/SessionInfo" }
                                    }
                                }
                            }
                        }
                    }
                },
                "post": {
                    "summary": "Create a session",
                    "description": "Create a new multi-turn conversation session.",
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": { "$ref": "#/components/schemas/CreateSessionRequest" }
                            }
                        }
                    },
                    "responses": {
                        "201": {
                            "description": "Session created",
                            "content": {
                                "application/json": {
                                    "schema": { "$ref": "#/components/schemas/CreateSessionResponse" }
                                }
                            }
                        }
                    }
                }
            },
            "/sessions/{id}": {
                "get": {
                    "summary": "Get session detail",
                    "description": "Returns session metadata and full message transcript.",
                    "parameters": [{
                        "name": "id",
                        "in": "path",
                        "required": true,
                        "schema": { "type": "string" }
                    }],
                    "responses": {
                        "200": {
                            "description": "Session detail with messages",
                            "content": {
                                "application/json": {
                                    "schema": { "$ref": "#/components/schemas/SessionDetailResponse" }
                                }
                            }
                        },
                        "404": { "description": "Session not found" }
                    }
                },
                "delete": {
                    "summary": "Delete a session",
                    "description": "Remove a session and its transcript.",
                    "parameters": [{
                        "name": "id",
                        "in": "path",
                        "required": true,
                        "schema": { "type": "string" }
                    }],
                    "responses": {
                        "204": { "description": "Session deleted" },
                        "404": { "description": "Session not found" }
                    }
                }
            },
            "/sessions/{id}/messages": {
                "post": {
                    "summary": "Send a message",
                    "description": "Send a user message in a session and get the assistant response.",
                    "parameters": [{
                        "name": "id",
                        "in": "path",
                        "required": true,
                        "schema": { "type": "string" }
                    }],
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": { "$ref": "#/components/schemas/SessionMessageRequest" }
                            }
                        }
                    },
                    "responses": {
                        "200": {
                            "description": "Assistant response",
                            "content": {
                                "application/json": {
                                    "schema": { "$ref": "#/components/schemas/SessionMessageResponse" }
                                }
                            }
                        },
                        "404": {
                            "description": "Session not found",
                            "content": {
                                "application/json": {
                                    "schema": { "$ref": "#/components/schemas/ErrorResponse" }
                                }
                            }
                        },
                        "500": {
                            "description": "Internal server error",
                            "content": {
                                "application/json": {
                                    "schema": { "$ref": "#/components/schemas/ErrorResponse" }
                                }
                            }
                        }
                    }
                }
            },
            "/sessions/{id}/events": {
                "get": {
                    "summary": "Subscribe to session events",
                    "description": "SSE stream of real-time agent events for a session.",
                    "parameters": [{
                        "name": "id",
                        "in": "path",
                        "required": true,
                        "schema": { "type": "string" }
                    }],
                    "responses": {
                        "200": {
                            "description": "SSE event stream",
                            "content": {
                                "text/event-stream": {
                                    "schema": { "type": "string" }
                                }
                            }
                        },
                        "404": { "description": "Session not found" }
                    }
                }
            },
            "/openapi.json": {
                "get": {
                    "summary": "OpenAPI specification",
                    "description": "Returns this OpenAPI 3.0.3 spec as JSON.",
                    "responses": {
                        "200": {
                            "description": "OpenAPI spec document",
                            "content": {
                                "application/json": {
                                    "schema": { "type": "object" }
                                }
                            }
                        }
                    }
                }
            }
        },
        "components": {
            "schemas": {
                "ToolInfo": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "description": { "type": "string" },
                        "parameters": { "type": "object" }
                    },
                    "required": ["name", "description", "parameters"]
                },
                "RunRequest": {
                    "type": "object",
                    "properties": {
                        "goal": { "type": "string" },
                        "max_steps": { "type": "integer", "nullable": true },
                        "system_prompt": { "type": "string", "nullable": true }
                    },
                    "required": ["goal"]
                },
                "RunResponse": {
                    "type": "object",
                    "properties": {
                        "status": { "type": "string" },
                        "finish_reason": { "type": "string" },
                        "messages": { "type": "array", "items": { "type": "object" } },
                        "usage": { "$ref": "#/components/schemas/UsageInfo" }
                    },
                    "required": ["status", "finish_reason", "messages", "usage"]
                },
                "UsageInfo": {
                    "type": "object",
                    "properties": {
                        "total_steps": { "type": "integer" },
                        "total_tokens": { "type": "integer" }
                    },
                    "required": ["total_steps", "total_tokens"]
                },
                "ErrorResponse": {
                    "type": "object",
                    "properties": {
                        "status": { "type": "string" },
                        "error": { "type": "string" }
                    },
                    "required": ["status", "error"]
                },
                "CreateSessionRequest": {
                    "type": "object",
                    "properties": {
                        "system_prompt": { "type": "string", "nullable": true }
                    }
                },
                "CreateSessionResponse": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "created_at": { "type": "string" }
                    },
                    "required": ["id", "created_at"]
                },
                "SessionInfo": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "created_at": { "type": "string" },
                        "message_count": { "type": "integer" }
                    },
                    "required": ["id", "created_at", "message_count"]
                },
                "SessionDetailResponse": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "created_at": { "type": "string" },
                        "messages": { "type": "array", "items": { "type": "object" } }
                    },
                    "required": ["id", "created_at", "messages"]
                },
                "SessionMessageRequest": {
                    "type": "object",
                    "properties": {
                        "content": { "type": "string" }
                    },
                    "required": ["content"]
                },
                "SessionMessageResponse": {
                    "type": "object",
                    "properties": {
                        "role": { "type": "string" },
                        "content": { "type": "string" }
                    },
                    "required": ["role", "content"]
                }
            }
        }
    })
}

async fn list_tools(State(state): State<Arc<AppState>>) -> Json<Vec<ToolInfo>> {
    Json(state.tools.clone())
}

async fn run_agent(
    State(state): State<Arc<AppState>>,
    Json(body): Json<RunRequest>,
) -> Result<Json<RunResponse>, (StatusCode, Json<ErrorResponse>)> {
    // Validate: goal must not be empty
    if body.goal.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                status: "error".into(),
                error: "missing or empty 'goal' field".into(),
            }),
        ));
    }

    let max_steps = body.max_steps.unwrap_or(state.config.max_steps as u32) as usize;
    let system_prompt = body
        .system_prompt
        .unwrap_or_else(|| state.config.system_prompt.clone());

    // Build and run the agent
    let mut agent = crate::Agent::builder()
        .llm(state.provider.clone())
        .system_prompt(system_prompt)
        .max_steps(max_steps)
        .build()
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    status: "error".into(),
                    error: format!("failed to build agent: {e}"),
                }),
            )
        })?;

    let outcome = agent.run(&body.goal).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                status: "error".into(),
                error: format!("agent run failed: {e}"),
            }),
        )
    })?;

    // Serialize transcript messages to JSON values
    let messages: Vec<serde_json::Value> = outcome
        .transcript
        .iter()
        .filter_map(|msg| serde_json::to_value(msg).ok())
        .collect();

    let finish_reason = format!("{:?}", outcome.finish);

    Ok(Json(RunResponse {
        status: "success".into(),
        finish_reason,
        messages,
        usage: UsageInfo {
            total_steps: outcome.steps as u32,
            total_tokens: outcome.total_usage.total_tokens as u64,
        },
    }))
}

// ── Session endpoints ──────────────────────────────────────────────────────

/// Generate a session ID using blake3 hash of timestamp + counter.
fn generate_session_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let input = format!("{}-{}", now.as_nanos(), count);
    let hash = blake3::hash(input.as_bytes());
    // Use first 16 hex chars for a short-ish but unique ID
    hash.to_hex()[..16].to_string()
}

/// Format a SystemTime as a basic ISO-8601 string (without chrono).
fn format_timestamp(t: SystemTime) -> String {
    let dur = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    // Basic formatting: seconds since epoch as a simple numeric timestamp
    // For a more human-readable format we do manual UTC conversion
    let days = secs / 86400;
    let remaining = secs % 86400;
    let hours = remaining / 3600;
    let minutes = (remaining % 3600) / 60;
    let seconds = remaining % 60;

    // Days since 1970-01-01
    let (year, month, day) = days_to_ymd(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hours, minutes, seconds
    )
}

/// Convert days since epoch to (year, month, day).
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Simplified civil calendar calculation
    let mut y = 1970;
    let mut remaining = days;
    loop {
        let days_in_year = if is_leap(y) { 366 } else { 365 };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        y += 1;
    }
    let months_days: [u64; 12] = if is_leap(y) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut m = 1;
    for &md in &months_days {
        if remaining < md {
            break;
        }
        remaining -= md;
        m += 1;
    }
    (y, m, remaining + 1)
}

fn is_leap(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// POST /sessions — create a new session.
async fn create_session(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateSessionRequest>,
) -> (StatusCode, Json<CreateSessionResponse>) {
    let id = generate_session_id();
    let created_at = format_timestamp(SystemTime::now());
    let system_prompt = body
        .system_prompt
        .unwrap_or_else(|| state.config.system_prompt.clone());

    let session = SessionState {
        id: id.clone(),
        created_at: created_at.clone(),
        transcript: Vec::new(),
        system_prompt,
    };

    state.sessions.write().await.insert(id.clone(), session);

    (
        StatusCode::CREATED,
        Json(CreateSessionResponse { id, created_at }),
    )
}

/// GET /sessions — list all sessions.
async fn list_sessions(State(state): State<Arc<AppState>>) -> Json<Vec<SessionInfo>> {
    let sessions = state.sessions.read().await;
    let infos: Vec<SessionInfo> = sessions
        .values()
        .map(|s| SessionInfo {
            id: s.id.clone(),
            created_at: s.created_at.clone(),
            message_count: s.transcript.len(),
        })
        .collect();
    Json(infos)
}

/// GET /sessions/:id — get session detail with messages.
async fn get_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<SessionDetailResponse>, StatusCode> {
    let sessions = state.sessions.read().await;
    let session = sessions.get(&id).ok_or(StatusCode::NOT_FOUND)?;

    let messages: Vec<serde_json::Value> = session
        .transcript
        .iter()
        .filter_map(|msg| serde_json::to_value(msg).ok())
        .collect();

    Ok(Json(SessionDetailResponse {
        id: session.id.clone(),
        created_at: session.created_at.clone(),
        messages,
    }))
}

/// DELETE /sessions/:id — remove a session.
async fn delete_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> StatusCode {
    let mut sessions = state.sessions.write().await;
    if sessions.remove(&id).is_some() {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

/// POST /sessions/:id/messages — send a message in a session.
async fn send_session_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<SessionMessageRequest>,
) -> Result<Json<SessionMessageResponse>, (StatusCode, Json<ErrorResponse>)> {
    // Read session state
    let (system_prompt, transcript) = {
        let sessions = state.sessions.read().await;
        let session = sessions.get(&id).ok_or((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                status: "error".into(),
                error: "session not found".into(),
            }),
        ))?;
        (session.system_prompt.clone(), session.transcript.clone())
    };

    // Set up event forwarding: mpsc from agent -> broadcast for SSE clients
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();

    // Build agent with session context and event sender
    let mut agent = crate::Agent::builder()
        .llm(state.provider.clone())
        .system_prompt(system_prompt)
        .events(event_tx)
        .build()
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    status: "error".into(),
                    error: format!("failed to build agent: {e}"),
                }),
            )
        })?;

    // Load existing transcript for multi-turn (only if there are prior messages)
    if !transcript.is_empty() {
        agent.set_transcript(transcript);
    }

    // Ensure broadcast channel exists for this session
    let broadcast_tx = {
        let mut channels = state.event_channels.write().await;
        let tx = channels.entry(id.clone()).or_insert_with(|| {
            let (tx, _) = broadcast::channel(64);
            tx
        });
        tx.clone()
    };

    // Spawn a task to forward StepEvents from mpsc to broadcast channel
    let forward_handle = tokio::spawn(async move {
        while let Some(step_event) = event_rx.recv().await {
            if let Some(sse_event) = map_step_event(&step_event) {
                let _ = broadcast_tx.send(sse_event);
            }
        }
    });

    // Run agent with the new message
    let outcome = agent.run(&body.content).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                status: "error".into(),
                error: format!("agent run failed: {e}"),
            }),
        )
    })?;

    // Drop the event sender so the forwarder task can finish
    agent.set_events(None);
    drop(agent);

    // Wait for the forwarder to finish draining events
    let _ = forward_handle.await;

    // Extract the last assistant message
    let last_assistant = outcome
        .transcript
        .iter()
        .rev()
        .find(|m| m.role == crate::message::Role::Assistant)
        .map(|m| m.content.clone())
        .unwrap_or_default();

    // Store updated transcript back into session
    {
        let mut sessions = state.sessions.write().await;
        if let Some(session) = sessions.get_mut(&id) {
            session.transcript = outcome.transcript;
        }
    }

    Ok(Json(SessionMessageResponse {
        role: "assistant".into(),
        content: last_assistant,
    }))
}

// ── SSE endpoint ─────────────────────────────────────────────────────────

/// GET /sessions/:id/events — subscribe to SSE stream of agent events.
async fn session_events(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    // Verify session exists
    {
        let sessions = state.sessions.read().await;
        if !sessions.contains_key(&id) {
            return Err(StatusCode::NOT_FOUND);
        }
    }

    // Get or create broadcast channel for this session
    let rx = {
        let mut channels = state.event_channels.write().await;
        let tx = channels.entry(id.clone()).or_insert_with(|| {
            let (tx, _) = broadcast::channel(64);
            tx
        });
        tx.subscribe()
    };

    let stream = BroadcastStream::new(rx).filter_map(|result| match result {
        Ok(sse_event) => {
            let event_type = match &sse_event {
                SseEvent::ToolCall { .. } => "tool_call",
                SseEvent::ToolResult { .. } => "tool_result",
                SseEvent::Done { .. } => "done",
                SseEvent::Error { .. } => "error",
            };
            let data = serde_json::to_string(&sse_event).unwrap_or_default();
            Some(Ok(Event::default().event(event_type).data(data)))
        }
        Err(_) => None,
    });

    Ok(Sse::new(stream))
}

// ── Event mapping ────────────────────────────────────────────────────────

/// Map an agent `StepEvent` to an `SseEvent` for broadcasting.
///
/// Returns `None` for events that don't have an SSE equivalent (e.g., latency,
/// partial tokens, usage stats).
pub fn map_step_event(event: &StepEvent) -> Option<SseEvent> {
    match event {
        StepEvent::ToolCall { call, step } => Some(SseEvent::ToolCall {
            name: call.name.clone(),
            step: *step,
        }),
        StepEvent::ToolResult {
            name, output, step: _, ..
        } => {
            let success = !output.starts_with("ERROR: ");
            Some(SseEvent::ToolResult {
                name: name.clone(),
                success,
            })
        }
        StepEvent::Finished { reason, steps } => Some(SseEvent::Done {
            finish_reason: format!("{:?}", reason),
            total_steps: *steps,
        }),
        // Other events don't map to SSE events
        _ => None,
    }
}
