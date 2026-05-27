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
use std::time::{Instant, SystemTime};
use tokio::sync::{broadcast, Mutex, RwLock};
use tokio_stream::{wrappers::BroadcastStream, StreamExt};

use crate::agent::StepEvent;
use crate::config::Config;
use crate::llm::LlmProvider;
use crate::message::Message;

// ── Rate limiter ───────────────────────────────────────────────────────────

/// Prometheus-compatible metrics collector using lock-free atomic counters.
#[derive(Default)]
pub struct Metrics {
    pub requests_total: AtomicU64,
    pub requests_active: AtomicU64,
    pub agent_runs_total: AtomicU64,
    pub agent_runs_success: AtomicU64,
    pub agent_runs_failed: AtomicU64,
    pub tokens_prompt_total: AtomicU64,
    pub tokens_completion_total: AtomicU64,
    pub agent_steps_total: AtomicU64,
}

/// GET /metrics — Prometheus exposition format.
async fn metrics_handler(State(state): State<Arc<AppState>>) -> String {
    let metrics = &state.metrics;
    let requests_total = metrics.requests_total.load(Ordering::Relaxed);
    let requests_active = metrics.requests_active.load(Ordering::Relaxed);
    let agent_runs_total = metrics.agent_runs_total.load(Ordering::Relaxed);
    let agent_runs_success = metrics.agent_runs_success.load(Ordering::Relaxed);
    let agent_runs_failed = metrics.agent_runs_failed.load(Ordering::Relaxed);
    let tokens_prompt_total = metrics.tokens_prompt_total.load(Ordering::Relaxed);
    let tokens_completion_total = metrics.tokens_completion_total.load(Ordering::Relaxed);
    let agent_steps_total = metrics.agent_steps_total.load(Ordering::Relaxed);

    format!(
        "# HELP recursive_requests_total Total HTTP requests\n\
         # TYPE recursive_requests_total counter\n\
         recursive_requests_total {requests_total}\n\
         # HELP recursive_requests_active Currently active HTTP requests\n\
         # TYPE recursive_requests_active gauge\n\
         recursive_requests_active {requests_active}\n\
         # HELP recursive_agent_runs_total Total agent runs\n\
         # TYPE recursive_agent_runs_total counter\n\
         recursive_agent_runs_total {agent_runs_total}\n\
         # HELP recursive_agent_runs_success Successful agent runs\n\
         # TYPE recursive_agent_runs_success counter\n\
         recursive_agent_runs_success {agent_runs_success}\n\
         # HELP recursive_agent_runs_failed Failed agent runs\n\
         # TYPE recursive_agent_runs_failed counter\n\
         recursive_agent_runs_failed {agent_runs_failed}\n\
         # HELP recursive_tokens_prompt_total Total prompt tokens consumed\n\
         # TYPE recursive_tokens_prompt_total counter\n\
         recursive_tokens_prompt_total {tokens_prompt_total}\n\
         # HELP recursive_tokens_completion_total Total completion tokens generated\n\
         # TYPE recursive_tokens_completion_total counter\n\
         recursive_tokens_completion_total {tokens_completion_total}\n\
         # HELP recursive_agent_steps_total Total agent steps executed\n\
         # TYPE recursive_agent_steps_total counter\n\
         recursive_agent_steps_total {agent_steps_total}\n"
    )
}

use std::sync::atomic::{AtomicU64, Ordering};

/// Token-bucket rate limiter keyed by client identifier (API key or remote IP).
#[derive(Clone)]
pub struct RateLimiter {
    /// Tokens remaining per client key.
    buckets: Arc<Mutex<HashMap<String, TokenBucket>>>,
    /// Max tokens per bucket.
    capacity: u32,
    /// Tokens refilled per second.
    refill_rate: f64,
}

/// A single token bucket for one client.
struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
}

impl RateLimiter {
    /// Create a new rate limiter with the given capacity and refill rate.
    ///
    /// - `capacity`: maximum number of tokens (burst size).
    /// - `refill_rate`: tokens added per second.
    fn new(capacity: u32, refill_rate: f64) -> Self {
        Self {
            buckets: Arc::new(Mutex::new(HashMap::new())),
            capacity,
            refill_rate,
        }
    }

    /// Check if a request from `key` is allowed.
    ///
    /// Returns `true` if the request is within the rate limit, `false` if it
    /// should be rejected (429).
    async fn check(&self, key: &str) -> bool {
        let mut buckets = self.buckets.lock().await;
        let now = Instant::now();

        let bucket = buckets.entry(key.to_string()).or_insert_with(|| {
            // New client gets a full bucket
            let tokens = self.capacity as f64;
            TokenBucket {
                tokens,
                last_refill: now,
            }
        });

        // Refill tokens based on elapsed time
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        let refill = elapsed * self.refill_rate;
        bucket.tokens = (bucket.tokens + refill).min(self.capacity as f64);
        bucket.last_refill = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Build a `RateLimiter` from environment variables.
///
/// - `RECURSIVE_RATE_LIMIT_RPM`: requests per minute (default: 60)
/// - `RECURSIVE_RATE_LIMIT_BURST`: burst capacity (default: 10)
fn rate_limiter_from_env() -> RateLimiter {
    let rpm = std::env::var("RECURSIVE_RATE_LIMIT_RPM")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(60.0);
    let burst = std::env::var("RECURSIVE_RATE_LIMIT_BURST")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(10);
    // Convert RPM to per-second refill rate
    let refill_rate = rpm / 60.0;
    RateLimiter::new(burst, refill_rate)
}

/// Extract a client key from the request for rate limiting.
///
/// Uses the `X-API-Key` header if present, otherwise falls back to the
/// remote IP address.
fn extract_client_key(req: &axum::extract::Request) -> String {
    if let Some(api_key) = req.headers().get("x-api-key") {
        if let Ok(key) = api_key.to_str() {
            return format!("apikey:{}", key);
        }
    }
    // Fall back to remote IP
    req.extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|info| format!("ip:{}", info.ip()))
        .unwrap_or_else(|| "ip:unknown".to_string())
}

/// Middleware that increments request counters.
async fn metrics_middleware(
    State(metrics): State<Arc<Metrics>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    metrics.requests_total.fetch_add(1, Ordering::Relaxed);
    metrics.requests_active.fetch_add(1, Ordering::Relaxed);
    let response = next.run(req).await;
    metrics.requests_active.fetch_sub(1, Ordering::Relaxed);
    response
}

/// Middleware that enforces rate limits on all API requests.
async fn rate_limit_middleware(
    State(limiter): State<RateLimiter>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let key = extract_client_key(&req);
    if !limiter.check(&key).await {
        let mut resp = axum::response::Response::new(axum::body::Body::from("rate limit exceeded"));
        *resp.status_mut() = StatusCode::TOO_MANY_REQUESTS;
        return resp;
    }
    next.run(req).await
}

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
    pub metrics: Arc<Metrics>,
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
    let limiter = rate_limiter_from_env();

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
        .route("/metrics", get(metrics_handler))
        .layer(axum::middleware::from_fn_with_state(
            state.metrics.clone(),
            metrics_middleware,
        ))
        .layer(axum::middleware::from_fn_with_state(
            limiter,
            rate_limit_middleware,
        ))
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
        state
            .metrics
            .agent_runs_total
            .fetch_add(1, Ordering::Relaxed);
        state
            .metrics
            .agent_runs_failed
            .fetch_add(1, Ordering::Relaxed);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                status: "error".into(),
                error: format!("agent run failed: {e}"),
            }),
        )
    })?;

    // Increment metrics
    state
        .metrics
        .agent_runs_total
        .fetch_add(1, Ordering::Relaxed);
    state
        .metrics
        .agent_runs_success
        .fetch_add(1, Ordering::Relaxed);
    state
        .metrics
        .agent_steps_total
        .fetch_add(outcome.steps as u64, Ordering::Relaxed);
    state
        .metrics
        .tokens_prompt_total
        .fetch_add(outcome.total_usage.prompt_tokens as u64, Ordering::Relaxed);
    state.metrics.tokens_completion_total.fetch_add(
        outcome.total_usage.completion_tokens as u64,
        Ordering::Relaxed,
    );

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
    let dur = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
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
async fn delete_session(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> StatusCode {
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
            name,
            output,
            step: _,
            ..
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

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Helper: create a rate limiter with very small capacity for testing.
    fn test_limiter(capacity: u32, rpm: f64) -> RateLimiter {
        RateLimiter::new(capacity, rpm / 60.0)
    }

    #[tokio::test]
    async fn test_requests_within_limit_succeed() {
        let limiter = test_limiter(5, 60.0); // 5 burst, 60 RPM
        for _ in 0..5 {
            assert!(limiter.check("client-a").await, "request should be allowed");
        }
    }

    #[tokio::test]
    async fn test_requests_exceeding_limit_get_429() {
        let limiter = test_limiter(3, 60.0); // 3 burst, 60 RPM
        for _ in 0..3 {
            assert!(limiter.check("client-b").await, "request should be allowed");
        }
        // Fourth request should be denied
        assert!(
            !limiter.check("client-b").await,
            "request should be rate limited"
        );
    }

    #[tokio::test]
    async fn test_tokens_refill_after_waiting() {
        let limiter = test_limiter(2, 60.0); // 2 burst, 60 RPM = 1 per second
                                             // Exhaust the bucket
        assert!(limiter.check("client-c").await);
        assert!(limiter.check("client-c").await);
        assert!(!limiter.check("client-c").await, "should be denied");

        // Wait for refill (1 token per second, wait 1.1s to be safe)
        tokio::time::sleep(Duration::from_millis(1100)).await;

        // Should have at least 1 token now
        assert!(
            limiter.check("client-c").await,
            "should be allowed after refill"
        );
    }

    #[tokio::test]
    async fn test_different_clients_have_independent_buckets() {
        let limiter = test_limiter(2, 60.0); // 2 burst

        // Exhaust client-d
        assert!(limiter.check("client-d").await);
        assert!(limiter.check("client-d").await);
        assert!(
            !limiter.check("client-d").await,
            "client-d should be denied"
        );

        // client-e should still have a full bucket
        assert!(
            limiter.check("client-e").await,
            "client-e should be allowed"
        );
        assert!(
            limiter.check("client-e").await,
            "client-e should be allowed"
        );
    }

    #[tokio::test]
    async fn test_extract_client_key_with_api_key() {
        let req = axum::http::Request::builder()
            .header("x-api-key", "test-key-123")
            .body(axum::body::Body::empty())
            .unwrap();
        let key = extract_client_key(&req);
        assert_eq!(key, "apikey:test-key-123");
    }

    #[tokio::test]
    async fn test_extract_client_key_without_api_key() {
        let req = axum::http::Request::builder()
            .body(axum::body::Body::empty())
            .unwrap();
        let key = extract_client_key(&req);
        // No ConnectInfo extension, so falls back to "ip:unknown"
        assert_eq!(key, "ip:unknown");
    }

    #[tokio::test]
    async fn test_rate_limiter_from_env_defaults() {
        // Unset the env vars to test defaults
        std::env::remove_var("RECURSIVE_RATE_LIMIT_RPM");
        std::env::remove_var("RECURSIVE_RATE_LIMIT_BURST");
        let limiter = rate_limiter_from_env();
        // Default: 60 RPM, 10 burst
        for _ in 0..10 {
            assert!(limiter.check("default-client").await);
        }
        assert!(!limiter.check("default-client").await, "burst exceeded");
    }
}
