//! HTTP API server for the Recursive agent.
//!
//! Provides a lightweight axum-based HTTP server that exposes the agent's
//! tool registry as a read-only JSON endpoint, a health check, a POST /run
//! endpoint that executes the agent with a given goal, session management
//! endpoints for multi-turn conversations, and SSE streaming of agent events.

mod auth;
mod handlers;
mod rate_limit;

pub use auth::{AuthConfig, JwtConfig};
pub use handlers::map_agent_event;
pub use rate_limit::RateLimiter;

use auth::{auth_config_from_env, auth_middleware};
use handlers::{
    agui_run, create_session, delete_session, fork_session, get_session, health, list_sessions,
    list_slash_commands, list_tools, metrics_handler, openapi_spec, patch_session, run_agent,
    send_session_message, session_clear_goal, session_events, session_interrupt,
    session_plan_confirm, session_plan_reject, session_set_goal,
};
use rate_limit::{metrics_middleware, rate_limit_middleware, rate_limiter_from_env};

use axum::{
    routing::{get, post},
    Router,
};
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

use crate::config::Config;
use crate::llm::LlmProvider;
use crate::runtime::AgentRuntime;
use crate::tools::plan_mode::PlanApprovalGate;
use crate::tools::ToolRegistry;

// ── Metrics ────────────────────────────────────────────────────────────────

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

// ── Session types ──────────────────────────────────────────────────────────

/// Internal session state (not directly serialized to clients).
///
/// The [`AgentRuntime`] owns the transcript; this struct adds HTTP-layer
/// metadata (id, created_at) and the broadcast channel for SSE clients.
pub struct SessionState {
    pub id: String,
    pub created_at: String,
    /// Optional human-readable title, settable via `PATCH /sessions/:id`.
    pub title: Option<String>,
    /// Runtime is wrapped in a per-session Mutex so concurrent HTTP requests
    /// for the same session are serialized without blocking the global lock.
    pub runtime: Arc<tokio::sync::Mutex<AgentRuntime>>,
    /// Shared gate for plan-mode approval. Stored here so HTTP handlers can
    /// approve/reject without taking the runtime Mutex (which may be held
    /// by a running agent turn).
    pub plan_approval_gate: Arc<PlanApprovalGate>,
    /// Goal-170: cancellation token for the currently running agent turn.
    /// `POST /sessions/:id/interrupt` cancels this token, which causes
    /// the kernel to exit with `FinishReason::Cancelled` at the next step
    /// boundary.  Replaced with a fresh token at the start of every turn.
    pub interrupt_token: Arc<tokio::sync::Mutex<Option<tokio_util::sync::CancellationToken>>>,
    /// Approximate non-system message count, updated atomically as messages
    /// are appended. Allows `list_sessions` to read the count without taking
    /// the runtime Mutex (which may be held by a running agent turn).
    pub non_system_message_count: Arc<std::sync::atomic::AtomicUsize>,
}

/// Serialized session info for list/detail endpoints.
#[derive(Clone, serde::Serialize, serde::Deserialize, Debug)]
pub struct SessionInfo {
    pub id: String,
    pub created_at: String,
    pub message_count: usize,
    /// Optional human-readable title, set via `PATCH /sessions/:id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Request body for `POST /sessions`.
#[derive(serde::Deserialize, Debug)]
pub struct CreateSessionRequest {
    pub system_prompt: Option<String>,
    /// Append additional text to the server's default system prompt instead of
    /// replacing it. Ignored when `system_prompt` is also provided.
    pub append_system_prompt: Option<String>,
    /// Human-readable display name for the session (shown in session list /
    /// resume picker).
    pub session_name: Option<String>,
    /// Maximum number of steps (tool calls) allowed in this session.
    pub max_steps: Option<u32>,
    /// Extended-thinking token budget for models that support it (e.g.
    /// Anthropic claude-3-7). `0` disables thinking.
    pub thinking_budget: Option<u32>,
    /// Permission mode: `"default"`, `"auto"`, `"strict"`, or `"bypass"`.
    pub permission_mode: Option<String>,
    /// Maximum total API spend in USD for this session. Agent stops after any
    /// turn that would exceed this limit.
    pub max_budget_usd: Option<f64>,
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
    /// Optional human-readable title (set via `PATCH /sessions/:id`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub messages: Vec<serde_json::Value>,
    /// Goal-167: current task list as maintained by `todo_write` calls.
    pub todos: Vec<crate::tools::todo::TodoItem>,
    /// Current session lifecycle state: `"idle"` | `"plan_pending_approval"`.
    pub status: String,
    /// Non-null when `status` is `"plan_pending_approval"`.
    pub pending_plan: Option<String>,
    /// Goal-168: active goal state, or `null` when no goal is set.
    pub goal: Option<crate::runtime::GoalState>,
    /// First user message in the session (for quick display).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_prompt: Option<String>,
    /// Most recent user message in the session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_prompt: Option<String>,
}

// ── Goal-168: goal endpoint types ────────────────────────────────────────

/// Request body for `POST /sessions/:id/goal`.
#[derive(serde::Deserialize, Debug)]
pub struct SetGoalRequest {
    /// The completion condition (free-form text).
    pub condition: String,
    /// Hard cap on autonomous turns. Defaults to 20.
    pub max_turns: Option<u32>,
}

/// Response body for goal mutation endpoints.
#[derive(serde::Serialize, Debug)]
pub struct GoalResponse {
    pub status: String,
}

// ── Goal-169: slash commands endpoint types ───────────────────────────────

/// One slash command entry in `GET /slash-commands`.
#[derive(Clone, serde::Serialize, Debug)]
pub struct SlashCommandInfo {
    pub name: String,
    pub description: String,
    pub source: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub argument_hint: String,
}

// ── SSE event types ──────────────────────────────────────────────────────

/// A single block of message content (mirrors Claude Agent SDK's
/// `TextBlock` / `ToolUseBlock`). Emitted as part of [`SseEvent::Message`]
/// so SDK clients can iterate `for block in msg.content` without doing a
/// second round-trip to the session detail endpoint.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SseContentBlock {
    /// A run of plain text from the assistant.
    Text { text: String },
    /// A request from the assistant to call a tool.
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

/// Server-Sent Event payload emitted during an agent session run.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SseEvent {
    /// A full role-tagged message, with content broken into typed blocks.
    /// Modeled on Claude Agent SDK's `AssistantMessage` / `UserMessage`
    /// streaming shape so the TS / Python SDKs can yield typed messages
    /// without falling back to the session detail endpoint.
    Message {
        role: String,
        content: Vec<SseContentBlock>,
    },
    /// A partial text delta during streaming. Concatenate `text` deltas
    /// keyed by `step` to reconstruct the eventual `Message::Text` block.
    PartialMessage { text: String, step: usize },
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
    /// Agent proposed a plan and is waiting for human review.
    PlanProposed { plan: String },
    /// Goal-168: judge found condition not yet met; loop continues.
    GoalContinuing { reason: String, turns: u32 },
    /// Goal-168: judge confirmed condition met.
    GoalAchieved { condition: String, turns: u32 },
    /// SDK Phase B: a tool call just completed; elapsed_ms is wall-clock time
    /// from when the ToolCall event was received to when ToolResult arrived.
    /// Emitted in addition to (and after) the `tool_result` event.
    ToolProgress {
        tool_use_id: String,
        tool_name: String,
        elapsed_ms: u64,
    },
}

// ── App state ──────────────────────────────────────────────────────────────

/// Shared application state for the HTTP server.
#[derive(Clone)]
pub struct AppState {
    pub tools: Vec<ToolInfo>,
    /// Live tool registry used to construct per-request and per-session
    /// AgentRuntimes. Without this, runtimes get an empty registry and
    /// every tool_call from the LLM resolves to "tool not found".
    pub tool_registry: ToolRegistry,
    pub config: Config,
    pub provider: Arc<dyn LlmProvider>,
    /// Session state keyed by session ID.
    pub sessions: Arc<RwLock<HashMap<String, SessionState>>>,
    /// Per-session SSE broadcast channels.
    pub event_channels: Arc<RwLock<HashMap<String, broadcast::Sender<SseEvent>>>>,
    pub metrics: Arc<Metrics>,
    /// Goal-169: registered slash commands (built-in + skill-backed).
    /// Pre-built at startup for cheap `GET /slash-commands` responses.
    pub slash_commands: Arc<Vec<SlashCommandInfo>>,
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
    /// Append additional text to the server's default system prompt instead of
    /// replacing it. Ignored when `system_prompt` is also provided.
    pub append_system_prompt: Option<String>,
    /// Extended-thinking token budget for models that support it (e.g.
    /// Anthropic claude-3-7). `0` disables thinking.
    pub thinking_budget: Option<u32>,
    /// Permission mode: `"default"`, `"auto"`, `"strict"`, or `"bypass"`.
    pub permission_mode: Option<String>,
    /// Maximum total API spend in USD for this run.
    pub max_budget_usd: Option<f64>,
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

/// Query parameters for `GET /sessions`.
#[derive(serde::Deserialize, Debug, Default)]
pub struct ListSessionsQuery {
    /// Maximum number of sessions to return (default: all).
    pub limit: Option<usize>,
    /// Number of sessions to skip before returning results (default: 0).
    pub offset: Option<usize>,
}

// ── Router builders ────────────────────────────────────────────────────────

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
///
/// Auth is sourced from the `RECURSIVE_HTTP_AUTH_KEYS` env var. For tests
/// that need a deterministic auth state (no env-var races across parallel
/// test threads), use [`build_router_with_auth`] instead.
pub fn build_router(state: AppState) -> Router {
    build_router_with_auth(state, auth_config_from_env())
}

/// Build the HTTP router with an explicit `AuthConfig`.
///
/// Tests use this to inject a known auth state without touching
/// process-global env vars. Production code paths use [`build_router`].
///
/// The rate limiter is sourced from env (`rate_limiter_from_env`).
/// Tests that need deterministic rate-limit behavior should use
/// [`build_router_with_auth_and_rate_limit`] instead.
pub fn build_router_with_auth(state: AppState, auth: AuthConfig) -> Router {
    build_router_with_auth_and_rate_limit(state, auth, rate_limiter_from_env())
}

/// Build the HTTP router with an explicit `AuthConfig` AND an explicit
/// `RateLimiter`.
///
/// This is the lowest-level constructor — both of the layered
/// middleware states are caller-supplied. Production code paths
/// invoke [`build_router`] (env-driven on both); tests that need
/// race-free rate-limit assertions use this directly.
pub fn build_router_with_auth_and_rate_limit(
    state: AppState,
    auth: AuthConfig,
    limiter: RateLimiter,
) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/tools", get(list_tools))
        .route("/run", post(run_agent))
        .route("/sessions", post(create_session))
        .route("/sessions", get(list_sessions))
        .route("/sessions/{id}", get(get_session))
        .route("/sessions/{id}", axum::routing::delete(delete_session))
        .route("/sessions/{id}", axum::routing::patch(patch_session))
        .route("/sessions/{id}/messages", post(send_session_message))
        .route("/sessions/{id}/events", get(session_events))
        .route("/sessions/{id}/plan/confirm", post(session_plan_confirm))
        .route("/sessions/{id}/plan/reject", post(session_plan_reject))
        // Goal-168: goal loop endpoints.
        .route("/sessions/{id}/goal", post(session_set_goal))
        .route(
            "/sessions/{id}/goal",
            axum::routing::delete(session_clear_goal),
        )
        // Goal-170: interrupt the current agent turn.
        .route("/sessions/{id}/interrupt", post(session_interrupt))
        // SDK Phase C: fork a session.
        .route("/sessions/{id}/fork", post(fork_session))
        // Goal-169: slash commands listing.
        .route("/slash-commands", get(list_slash_commands))
        .route("/agui", post(agui_run))
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
        .layer(axum::middleware::from_fn_with_state(auth, auth_middleware))
        .with_state(Arc::new(state))
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
            "/agui": {
                "post": {
                    "summary": "Run an AG-UI agent",
                    "description": "Drive a recursive agent run via the AG-UI protocol \
                        (https://docs.ag-ui.com). Body is an AG-UI RunAgentInput; the \
                        response is an SSE stream of AG-UI events (RunStarted, \
                        TextMessageStart/Content/End, ToolCall*, RunFinished, ...).",
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": { "type": "object" },
                                "description": "AG-UI RunAgentInput payload"
                            }
                        }
                    },
                    "responses": {
                        "200": {
                            "description": "AG-UI SSE event stream",
                            "content": {
                                "text/event-stream": {
                                    "schema": { "type": "string" }
                                }
                            }
                        },
                        "400": { "description": "Invalid AG-UI RunAgentInput" }
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
                        "system_prompt": { "type": "string", "nullable": true },
                        "append_system_prompt": { "type": "string", "nullable": true },
                        "thinking_budget": { "type": "integer", "nullable": true },
                        "permission_mode": { "type": "string", "enum": ["default", "auto", "strict", "bypass"], "nullable": true },
                        "max_budget_usd": { "type": "number", "nullable": true }
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
                        "system_prompt": { "type": "string", "nullable": true },
                        "append_system_prompt": { "type": "string", "nullable": true },
                        "session_name": { "type": "string", "nullable": true },
                        "max_steps": { "type": "integer", "nullable": true },
                        "thinking_budget": { "type": "integer", "nullable": true },
                        "permission_mode": { "type": "string", "enum": ["default", "auto", "strict", "bypass"], "nullable": true },
                        "max_budget_usd": { "type": "number", "nullable": true }
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
