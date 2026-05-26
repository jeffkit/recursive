//! HTTP API server for the Recursive agent.
//!
//! Provides a lightweight axum-based HTTP server that exposes the agent's
//! tool registry as a read-only JSON endpoint, a health check, a POST /run
//! endpoint that executes the agent with a given goal, and session management
//! endpoints for multi-turn conversations.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::RwLock;

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

// ── App state ──────────────────────────────────────────────────────────────

/// Shared application state for the HTTP server.
#[derive(Clone)]
pub struct AppState {
    pub tools: Vec<ToolInfo>,
    pub config: Config,
    pub provider: Arc<dyn LlmProvider>,
    pub sessions: Arc<RwLock<HashMap<String, SessionState>>>,
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
        .with_state(Arc::new(state))
}

async fn health() -> &'static str {
    "ok"
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

    // Build agent with session context
    let mut agent = crate::Agent::builder()
        .llm(state.provider.clone())
        .system_prompt(system_prompt)
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
