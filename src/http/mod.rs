//! HTTP API server for the Recursive agent.
//!
//! Provides a lightweight axum-based HTTP server that exposes the agent's
//! tool registry as a read-only JSON endpoint, a health check, a POST /run
//! endpoint that executes the agent with a given goal, session management
//! endpoints for multi-turn conversations, and SSE streaming of agent events.

mod auth;
mod cold_load;
#[cfg(test)]
mod environment_binding_tests;
mod handlers;
mod rate_limit;

// Goal 395: the admission gate moved to the transport-agnostic
// `session_host` module; re-exported here so front-end call sites and
// external `recursive::http::AdmissionGate` paths keep working.
pub use crate::session_host::SessionHost;
pub use crate::session_host::{AcquireError, AdmissionGate, RunPermit};
pub use auth::{AuthConfig, JwtConfig, ENV_AUTH_JWT_SECRET, ENV_AUTH_KEYS};
pub use handlers::map_agent_event;
pub use rate_limit::{rate_limiter_from_env, RateLimiter};

use auth::{auth_config_from_env, auth_middleware};
use handlers::{
    agui_run, create_session, delete_session, fork_session, get_session, health, list_sessions,
    list_slash_commands, list_tools, metrics_handler, openapi_spec, patch_session, run_agent,
    send_session_message, session_clear_goal, session_events, session_interrupt,
    session_plan_confirm, session_plan_reject, session_set_goal,
};
use rate_limit::{metrics_middleware, rate_limit_middleware};

use axum::{
    extract::DefaultBodyLimit,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};

/// Maximum accepted request body size (1 MiB).
///
/// Prevents OOM via maliciously large JSON payloads on POST /run and
/// POST /sessions/:id/messages, both of which accept unbounded user strings.
const MAX_BODY_BYTES: usize = 1024 * 1024;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

use crate::config::Config;
use crate::llm::ChatProvider;
use crate::runtime::AgentRuntime;
use crate::storage::StorageBackend;
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
    /// Number of currently open sessions (gauge).
    pub sessions_active: AtomicU64,
    /// Number of requests rejected by rate limiting (counter).
    pub rate_limits_rejected: AtomicU64,
    /// Requests currently waiting for a run permit (gauge). `Arc`-shared so
    /// the admission gate (Goal 398) can bump it with RAII guards; read via
    /// `AdmissionGate::runs_waiting` or here for the `/metrics` exposition.
    pub runs_waiting: Arc<AtomicU64>,
    /// Runs currently holding an admission permit (gauge, Goal 392).
    /// `Arc`-shared with the admission gate; decremented by `RunPermit`'s
    /// `Drop` so every acquire site (including `?` early returns) is
    /// covered by RAII.
    pub runs_in_flight: Arc<AtomicU64>,
}

// ── Session types ──────────────────────────────────────────────────────────

/// Internal session state (not directly serialized to clients).
///
/// The [`AgentRuntime`] owns the transcript; this struct adds HTTP-layer
/// metadata (id, created_at) and the broadcast channel for SSE clients.
///
/// Clone is a handle clone: every mutable field is `Arc`-wrapped, so clones
/// share runtime / counters / gate with the value stored in the sessions
/// table (plain metadata fields — id / created_at / title — are copied).
/// `http::cold_load` relies on this to hand handlers a table entry without
/// holding the table lock.
#[derive(Clone)]
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
    /// Milliseconds since [`SESSION_EPOCH`] when this session was last active.
    /// Updated atomically on every message. Used by the session reaper.
    pub last_active_ms: Arc<AtomicU64>,
    /// Cumulative prompt tokens consumed in this session (all turns combined).
    pub prompt_tokens: Arc<AtomicU64>,
    /// Cumulative completion tokens generated in this session.
    pub completion_tokens: Arc<AtomicU64>,
}

/// Reference instant for session last_active timestamps.
/// Stored as a `OnceLock` so it's computed once at startup.
static SESSION_EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

fn session_epoch() -> std::time::Instant {
    *SESSION_EPOCH.get_or_init(std::time::Instant::now)
}

/// Read the current session timestamp as milliseconds since [`SESSION_EPOCH`].
pub fn now_session_ms() -> u64 {
    session_epoch().elapsed().as_millis() as u64
}

/// Goal 399: safe default execution budgets for sessions created over HTTP.
///
/// Returns `(max_steps, wall_timeout_secs)` as plain numbers so callers can
/// apply them through the normal `AgentRuntimeBuilder` setters. Precedence:
/// explicit env override → safe default; an explicit `0` disables the
/// respective limit (compatibility escape hatch back to unbounded runs).
///
/// - `RECURSIVE_HTTP_MAX_STEPS` (default 100) — caps LLM steps per run.
/// - `RECURSIVE_HTTP_WALL_TIMEOUT_SECS` (default 1800) — wall-clock budget
///   per turn; exceeding it finishes with `FinishReason::WallClockExceeded`
///   (data, not an error — invariant #7).
///
/// These defaults apply only to HTTP-created sessions; CLI/TUI assembly is
/// unaffected (they keep `Config::wall_timeout_secs` / `Config::max_steps`).
pub fn http_session_budget_from_env() -> (usize, u64) {
    fn parse_env_or(name: &str, default: u64) -> u64 {
        std::env::var(name)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(default)
    }
    let max_steps = parse_env_or("RECURSIVE_HTTP_MAX_STEPS", 100) as usize;
    let wall_timeout_secs = parse_env_or("RECURSIVE_HTTP_WALL_TIMEOUT_SECS", 1800);
    (max_steps, wall_timeout_secs)
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
#[derive(serde::Serialize, serde::Deserialize, Debug)]
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
    /// Total prompt tokens consumed across all turns in this session.
    pub prompt_tokens: u64,
    /// Total completion tokens generated across all turns in this session.
    pub completion_tokens: u64,
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
    pub provider: Arc<dyn ChatProvider>,
    /// Goal 395: transport-agnostic session host — owns the session
    /// registry, the run-admission gate (Goal 398) and the session TTL.
    /// Session SSE channels stay here: they are an HTTP transport concept.
    pub host: Arc<SessionHost<SessionState>>,
    /// Per-session SSE broadcast channels.
    pub event_channels: Arc<RwLock<HashMap<String, broadcast::Sender<SseEvent>>>>,
    pub metrics: Arc<Metrics>,
    /// Goal-169: registered slash commands (built-in + skill-backed).
    /// Pre-built at startup for cheap `GET /slash-commands` responses.
    pub slash_commands: Arc<Vec<SlashCommandInfo>>,
    /// Shared rate limiter for all API requests. Stored on `AppState` so the
    /// session reaper can prune idle token buckets.
    pub rate_limiter: RateLimiter,
    /// Discovered skills for skill_index injection into the system prompt.
    /// Empty if no skills found. Goal-312.
    pub skills: Vec<crate::skills::Skill>,
    /// Goal 396: shared transcript persistence backend. Chosen once at HTTP
    /// startup (CLI default: `LocalStorageBackend` under the per-workspace
    /// user dir) and injected into every session runtime via
    /// `AgentRuntimeBuilder::storage`. The host layer calls
    /// `save_transcript` on session teardown only — DELETE, idle eviction,
    /// and graceful shutdown — never per turn (that would be an O(N²)
    /// full-transcript rewrite on the hot path).
    ///
    /// Goal 397 cold-load reads this same backend to restore sessions after a
    /// restart (`cold_load::get_or_load_session`).
    pub storage: Arc<dyn StorageBackend>,
}

/// Serializable tool info for the `/tools` endpoint.
#[derive(Clone, serde::Serialize, serde::Deserialize, Debug)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Goal 403 (issue §3): construct the container environment **per session**.
///
/// In the container sandbox tier every session (and one-shot `/run` / `/agui`
/// run) gets its OWN `ContainerToolSetProvider` registry — i.e. its own
/// sandbox container — instead of sharing the registry built once at server
/// startup. Every other tier (none / policy) keeps sharing the process-wide
/// startup registry via a plain clone.
///
/// Process-level configuration on the startup registry (permissions,
/// headless, hook runner) is carried over so the per-session rebuild does
/// not silently drop it.
async fn rebind_per_session_registry(
    base: &ToolRegistry,
    #[allow(unused_variables)] config: &Config,
    #[allow(unused_variables)] skills: &[crate::skills::Skill],
) -> Result<ToolRegistry, String> {
    #[cfg(feature = "cloud-runtime")]
    {
        if matches!(
            crate::SandboxMode::from_env(),
            Ok(Some(crate::SandboxMode::Container))
        ) {
            let provider = crate::tools::ContainerToolSetProvider::new(
                config.workspace.clone(),
                config.shell_timeout_secs,
                skills.to_vec(),
            );
            // Issue #31 §C: container creation failure is a per-session
            // error (503/500 at the handler), never a process exit.
            let mut reg = provider
                .build_registry_result()
                .await
                .map_err(|e| e.to_string())?;
            if let Some(sp) = base.shared_permissions() {
                reg = reg.with_shared_permissions(sp);
            }
            // Issue #69: the container tier rebuilds the registry per session,
            // bypassing the startup narrowing in the HTTP entry — reapply
            // `allow_tools` here so `RECURSIVE_ALLOW_TOOLS` holds on every tier.
            if !config.allow_tools.is_empty() {
                reg.retain_tools(&config.allow_tools);
            }
            return Ok(reg
                .with_headless(base.headless)
                .with_hook_runner(base.hook_runner.clone()));
        }
    }
    Ok(base.clone())
}

impl AppState {
    /// The tool registry a NEW session / run should be built with.
    ///
    /// Container tier: a fresh registry with its own container (issue §3 —
    /// one container per session, not one per process). Other tiers: a
    /// handle-clone of the shared startup registry (previous behaviour).
    /// Issue #31 §C: Result-shaped — container creation failure is a
    /// per-session error mapped by handlers to 503/500, not a process exit.
    ///
    /// Issue #65: the surface contracts (operator allow-list, coordinator
    /// pruning) are re-applied here because the container tier rebuilds the
    /// registry from scratch — filtering only the startup registry would
    /// silently hand rebuilt sessions the full toolset again. On the clone
    /// path this is an idempotent re-filter. (MCP tools are still lost on a
    /// container rebuild — the provider builds a fresh local registry —
    /// which stays a documented container-tier gap, not a silent contract
    /// violation.)
    pub async fn session_tool_registry(&self) -> Result<ToolRegistry, String> {
        let mut registry =
            rebind_per_session_registry(&self.tool_registry, &self.config, &self.skills).await?;
        crate::coordinator::filter_registry(&mut registry);
        if !self.config.allow_tools.is_empty() {
            registry.retain_tools(&self.config.allow_tools);
        }
        Ok(registry)
    }
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

// ── Goal 295: standardized JSON error envelope ───────────────────────────

/// JSON body shape for [`ApiError`] responses.
///
/// Every endpoint that returns a 4xx/5xx via [`ApiError`] produces a body
/// of exactly `{"error": "<message>"}`, so clients can write a single
/// error handler that always parses the response as JSON.
#[derive(serde::Serialize)]
struct ErrorBody {
    error: String,
}

/// A standardized JSON error response for all API endpoints.
///
/// Replaces bare `Err(StatusCode::NOT_FOUND)` returns in handlers so every
/// error carries a parseable `{"error": "..."}` body. Handlers that already
/// return `(StatusCode, Json(json!(...)))` are intentionally left alone —
/// they already produce a JSON body; wrapping them in [`ApiError`] would
/// only add a layer.
///
/// Use [`ApiError::with_retry_after`] to attach a `Retry-After` header
/// (Goal-313: needed by `session_clear_goal` when the runtime is busy).
#[derive(Debug)]
pub(super) struct ApiError {
    status: StatusCode,
    message: String,
    /// Optional `Retry-After: <secs>` header value. `Some(secs)` causes
    /// [`IntoResponse`] to inject the header into the response.
    retry_after_secs: Option<u32>,
}

impl ApiError {
    /// Build an [`ApiError`] from an arbitrary status + message.
    pub(super) fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            retry_after_secs: None,
        }
    }

    /// 404 Not Found with a message.
    pub(super) fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    /// 409 Conflict with a message.
    pub(super) fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }

    /// 500 Internal Server Error with a message.
    pub(super) fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }

    /// 400 Bad Request with a message.
    pub(super) fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    /// 403 Forbidden with a message.
    pub(super) fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }

    /// Attach a `Retry-After: <secs>` header to this error response.
    ///
    /// Goal-313: lets `session_clear_goal` preserve the `Retry-After: 5`
    /// hint it used to emit when the runtime Mutex was held by an
    /// in-flight turn, while still routing through [`ApiError`] for the
    /// JSON envelope.
    pub(super) fn with_retry_after(mut self, secs: u32) -> Self {
        self.retry_after_secs = Some(secs);
        self
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let body = Json(ErrorBody {
            error: self.message,
        });
        let mut resp = (self.status, body).into_response();
        if let Some(secs) = self.retry_after_secs {
            // `secs` is u32 (max ~136 years), so the decimal repr fits
            // comfortably in a single-digit-to-ten-char HeaderValue.
            if let Ok(val) = axum::http::HeaderValue::from_str(&secs.to_string()) {
                resp.headers_mut()
                    .insert(axum::http::header::RETRY_AFTER, val);
            }
        }
        resp
    }
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

/// Run a `Router` on an already-bound [`tokio::net::TcpListener`] until
/// `shutdown` resolves, then stop gracefully.
///
/// This encapsulates the `axum::serve` call so callers — notably the
/// `recursive http` CLI subcommand — can serve the API without taking a
/// direct `axum` dependency. `shutdown` is any future that completes when
/// the server should stop (e.g. `CancellationToken::cancelled()` wrapped in
/// an `async move` block fired on SIGINT/SIGTERM).
pub async fn serve_with_graceful_shutdown(
    listener: tokio::net::TcpListener,
    router: Router,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
}

/// Build the HTTP router with an explicit `AuthConfig`.
///
/// Tests use this to inject a known auth state without touching
/// process-global env vars. Production code paths use [`build_router`].
///
/// The rate limiter is taken from `AppState.rate_limiter`. Callers should
/// populate it via [`rate_limiter_from_env`] for production or with
/// [`RateLimiter::new`] for deterministic tests. Tests that need
/// race-free rate-limit assertions should use
/// [`build_router_with_auth_and_rate_limit`] instead.
pub fn build_router_with_auth(state: AppState, auth: AuthConfig) -> Router {
    let limiter = state.rate_limiter.clone();
    build_router_with_auth_and_rate_limit(state, auth, limiter)
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
    let state_arc = Arc::new(state);

    // Public sub-router: no auth or rate-limit middleware. Routes
    // added here are implicitly reachable without credentials. This
    // makes the bypass **structural** (the route lives outside the
    // auth layer entirely) instead of a string-compare in
    // `auth_middleware` — the previous implementation, which
    // silently let `/openapi.json` 401 because it was added to the
    // router but not to the bypass list.
    let public = Router::new()
        .route("/health", get(health))
        .route("/openapi.json", get(openapi_spec))
        .route("/metrics", get(metrics_handler));

    // Protected sub-router: every other route goes through auth and
    // rate-limit. The rate-limit layer is the **outermost** of the
    // two so it runs first — unauthenticated (brute-force) requests
    // are counted against the IP-based bucket and cannot bypass
    // limits by rotating API keys (SEC-006).
    let protected = Router::new()
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
        .route("/sessions/{id}/goal", post(session_set_goal))
        .route(
            "/sessions/{id}/goal",
            axum::routing::delete(session_clear_goal),
        )
        .route("/sessions/{id}/interrupt", post(session_interrupt))
        .route("/sessions/{id}/fork", post(fork_session))
        .route("/slash-commands", get(list_slash_commands))
        .route("/agui", post(agui_run))
        .layer(axum::middleware::from_fn_with_state(auth, auth_middleware))
        .layer(axum::middleware::from_fn_with_state(
            (limiter.clone(), state_arc.metrics.clone()),
            rate_limit_middleware,
        ));

    // Top router: merge public + protected, then add the cross-cutting
    // layers (metrics, body-limit) that apply to both. metrics counts
    // every request (public + protected); body-limit caps every body.
    Router::new()
        .merge(public)
        .merge(protected)
        .layer(axum::middleware::from_fn_with_state(
            state_arc.metrics.clone(),
            metrics_middleware,
        ))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state_arc)
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
                    "description": "Returns all active sessions wrapped in an envelope \
                        with the un-paginated `total` count plus the `sessions` page slice. \
                        Clients use `total` to render \"page X of Y\" / scrollbars without \
                        having to fetch every page just to count sessions.",
                    "responses": {
                        "200": {
                            "description": "Envelope with total session count and the paginated slice",
                            "content": {
                                "application/json": {
                                    "schema": { "$ref": "#/components/schemas/SessionList" }
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
            "/metrics": {
                "get": {
                    "summary": "Prometheus metrics",
                    "description": "Returns Prometheus-compatible metrics exposition. \
                        Includes the standard counters (requests, agent runs, tokens) plus \
                        `recursive_sessions_active` (gauge, count of currently open sessions) \
                        and `recursive_rate_limits_rejected_total` (counter, requests rejected \
                        by rate limiting). Added in G292, documented in G298.",
                    "responses": {
                        "200": {
                            "description": "Prometheus text format",
                            "content": {
                                "text/plain": {
                                    "schema": { "type": "string" }
                                }
                            }
                        }
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
                        "message_count": { "type": "integer" },
                        "title": { "type": "string", "nullable": true }
                    },
                    "required": ["id", "created_at", "message_count"]
                },
                "SessionList": {
                    "type": "object",
                    "description": "Response envelope for GET /sessions. `total` is the count of all sessions before pagination, so clients can compute total pages without fetching every page.",
                    "properties": {
                        "total": {
                            "type": "integer",
                            "description": "Number of sessions known to the server (before applying limit/offset)."
                        },
                        "sessions": {
                            "type": "array",
                            "items": { "$ref": "#/components/schemas/SessionInfo" },
                            "description": "The paginated slice of session info objects."
                        }
                    },
                    "required": ["total", "sessions"]
                },
                "SessionDetailResponse": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "created_at": { "type": "string" },
                        "title": { "type": "string", "nullable": true },
                        "messages": { "type": "array", "items": { "type": "object" } },
                        "todos": { "type": "array", "items": { "type": "object" } },
                        "status": {
                            "type": "string",
                            "description": "Session lifecycle state: idle | plan_pending_approval"
                        },
                        "pending_plan": { "type": "string", "nullable": true },
                        "goal": { "type": "object", "nullable": true },
                        "first_prompt": { "type": "string", "nullable": true },
                        "last_prompt": { "type": "string", "nullable": true },
                        "prompt_tokens": {
                            "type": "integer",
                            "description": "Cumulative prompt tokens for this session"
                        },
                        "completion_tokens": {
                            "type": "integer",
                            "description": "Cumulative completion tokens for this session"
                        }
                    },
                    "required": ["id", "created_at", "messages", "status", "todos", "prompt_tokens", "completion_tokens"]
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

/// Spawn a background task that periodically evicts idle sessions.
///
/// Every `check_interval` seconds the reaper runs one
/// [`SessionHost::evict_idle`] sweep (Goal 395): sessions idle beyond the
/// configured TTL are removed under a short write lock, and their runtime is
/// closed **outside every sessions lock** — a slow close (e.g. transcript
/// persistence, Goal 396) can therefore never freeze the other session
/// endpoints. Busy sessions (runtime locked by an in-flight turn) are
/// skipped in place and picked up by a later sweep.
///
/// Goal 396: closing a session also persists its transcript through
/// `AppState.storage` — the save is a full-overwrite write and, like the
/// close itself, runs outside every host lock.
pub fn spawn_session_reaper(
    state: Arc<AppState>,
    check_interval: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(check_interval).await;
            // One sweep: evict idle sessions, close each runtime, persist its
            // transcript (Goal 396) and prune the per-session channels.
            evict_idle_sessions(&state).await;
            // Prune idle rate-limit buckets (goal-290). Runs every
            // reaper tick so the bucket map doesn't grow unboundedly.
            state.rate_limiter.prune().await;
        }
    })
}

/// One host-layer eviction sweep.
///
/// Goal 395 moved the sweep into [`SessionHost::evict_idle`] (busy sessions are
/// skipped in place and picked up by a later sweep); this keeps Goal 396's
/// named entry point for the eviction tests and grafts 396's behaviour onto the
/// new host API: after the runtime is closed — outside every host lock — the
/// transcript is persisted through `AppState.storage`.
///
/// Returns the ids that were evicted.
pub(super) async fn evict_idle_sessions(state: &AppState) -> Vec<String> {
    let evicted = state
        .host
        .evict_idle(
            |session, ttl| {
                let last_ms = session.last_active_ms.load(Ordering::Relaxed);
                std::time::Duration::from_millis(now_session_ms() - last_ms) >= ttl
            },
            |session| session.runtime.try_lock().is_err(),
            |session| {
                let storage = state.storage.clone();
                async move {
                    if let Ok(mut rt) = session.runtime.try_lock() {
                        rt.close(None).await;
                        // Issue #31 §B: explicit environment teardown on
                        // idle eviction (idempotent; runs outside every
                        // sessions lock — this closure is phase-3 by
                        // contract).
                        rt.destroy_environment().await;
                        let transcript = rt.transcript().to_vec();
                        drop(rt);
                        if let Err(e) = storage.save_transcript(&session.id, &transcript).await {
                            tracing::warn!(
                                session_id = %session.id,
                                error = %e,
                                "reaper: failed to persist session transcript"
                            );
                        }
                    }
                }
            },
            |id| {
                state
                    .metrics
                    .sessions_active
                    .fetch_sub(1, Ordering::Relaxed);
                tracing::info!("reaper: evicted idle session {id}");
            },
        )
        .await;
    // Prune stale event_channels for evicted sessions (goal-290 housekeeping).
    if !evicted.is_empty() {
        let mut channels = state.event_channels.write().await;
        for id in &evicted {
            channels.remove(id);
        }
    }
    evicted
}

/// Goal 396: persist every live session's transcript — the graceful
/// shutdown path (`recursive http` calls this after the axum server stops).
///
/// Drains the session map under one write lock, then closes and saves each
/// runtime outside the lock. Sessions whose runtime is still mid-turn are
/// skipped (logged); a backend error on one session never stops the others.
/// Returns the number of transcripts persisted.
pub async fn flush_all_sessions(state: &AppState) -> usize {
    let drained: Vec<SessionState> = {
        let host_sessions = state.host.sessions();
        let mut sessions = host_sessions.write().await;
        sessions.drain().map(|(_, v)| v).collect()
    };
    let mut persisted = 0;
    for session in drained {
        if let Ok(mut rt) = session.runtime.try_lock() {
            rt.close(None).await;
            // Issue #31 §B: graceful-shutdown teardown — destroy the
            // session's environment (idempotent), then persist.
            rt.destroy_environment().await;
            let transcript = rt.transcript().to_vec();
            drop(rt);
            match state
                .storage
                .save_transcript(&session.id, &transcript)
                .await
            {
                Ok(()) => persisted += 1,
                Err(e) => tracing::warn!(
                    session_id = %session.id,
                    error = %e,
                    "shutdown: failed to persist session transcript"
                ),
            }
        } else {
            tracing::warn!(
                session_id = %session.id,
                "shutdown: session still busy, transcript not persisted"
            );
        }
    }
    persisted
}

// =====================================================================
// Goal 272 — auth bypass is structural (route-level merge), not string-
// compare inside auth_middleware. These tests pin that invariant via
// source-grep so a future refactor that re-introduces the string-compare
// breaks CI instead of silently 401-ing /openapi.json.
//
// We use the source-grep form because the existing test harness
// (tests/http.rs) requires a full AppState builder; a runtime test here
// would add a fixture crate. Per the g268 post-mortem, deterministic
// source-level assertions are preferred over runtime tests for invariants
// that are already enforced by the type system + a structural merge.
// =====================================================================
#[cfg(test)]
mod goal_272_route_level_auth_bypass {

    #[test]
    fn public_subrouter_has_no_auth_layer() {
        let src = include_str!("mod.rs");
        // Slice the public sub-router block: from
        // `let public = Router::new()` to `let protected = Router::new()`.
        let between = src
            .split("let public = Router::new()")
            .nth(1)
            .expect("public sub-router block must exist");
        let public_block = between
            .split("let protected = Router::new()")
            .next()
            .expect("public block must end before protected");
        assert!(
            !public_block.contains("auth_middleware"),
            "public sub-router must NOT include auth_middleware layer"
        );
        assert!(
            !public_block.contains("rate_limit_middleware"),
            "public sub-router must NOT include rate_limit_middleware layer"
        );
        // All three public routes must be present.
        assert!(public_block.contains("/health"));
        assert!(public_block.contains("/openapi.json"));
        assert!(public_block.contains("/metrics"));
    }

    #[test]
    fn auth_middleware_no_longer_short_circuits_on_path() {
        // Previously `if path == "/health" || path == "/metrics"` lived
        // inside `auth_middleware`. The string-compare is gone now.
        let auth_src = include_str!("auth.rs");
        assert!(
            !auth_src.contains("path == \"/health\""),
            "auth_middleware must not hardcode /health bypass"
        );
        assert!(
            !auth_src.contains("path == \"/metrics\""),
            "auth_middleware must not hardcode /metrics bypass"
        );
        assert!(
            !auth_src.contains("path == \"/openapi.json\""),
            "auth_middleware must not hardcode /openapi.json bypass"
        );
    }

    #[test]
    fn protected_routes_built_with_auth_layer() {
        let src = include_str!("mod.rs");
        let protected_block = src
            .split("let protected = Router::new()")
            .nth(1)
            .expect("protected sub-router block must exist");
        let protected_end = protected_block
            .split(".merge(protected)")
            .next()
            .expect("protected merge call must exist");
        assert!(
            protected_end.contains("auth_middleware"),
            "protected sub-router must include auth_middleware layer"
        );
        assert!(
            protected_end.contains("rate_limit_middleware"),
            "protected sub-router must include rate_limit_middleware layer"
        );
    }
}

#[cfg(test)]
mod budget_tests {
    use super::http_session_budget_from_env;

    /// Goal 399: safe defaults, explicit env overrides, the explicit-0
    /// escape hatch, and garbage-value leniency. Kept as ONE test because
    /// both env vars are process globals: parallel assertions on the same
    /// pair would race (same pattern as the `effective_step_limit_*` env
    /// tests in run_core.rs).
    #[test]
    fn http_session_budget_defaults_env_overrides_and_zero_escape() {
        // 1) Defaults with both vars unset.
        std::env::remove_var("RECURSIVE_HTTP_MAX_STEPS");
        std::env::remove_var("RECURSIVE_HTTP_WALL_TIMEOUT_SECS");
        assert_eq!(
            http_session_budget_from_env(),
            (100, 1800),
            "defaults must be max_steps=100, wall=1800s"
        );

        // 2) Explicit overrides are honoured.
        std::env::set_var("RECURSIVE_HTTP_MAX_STEPS", "42");
        std::env::set_var("RECURSIVE_HTTP_WALL_TIMEOUT_SECS", "77");
        assert_eq!(http_session_budget_from_env(), (42, 77));

        // 3) Explicit 0 restores unbounded execution (compat switch).
        std::env::set_var("RECURSIVE_HTTP_MAX_STEPS", "0");
        std::env::set_var("RECURSIVE_HTTP_WALL_TIMEOUT_SECS", "0");
        assert_eq!(http_session_budget_from_env(), (0, 0));

        // 4) Unparseable/empty values fall back to the safe defaults instead
        //    of failing startup (mirrors `rate_limiter_from_env` leniency).
        std::env::set_var("RECURSIVE_HTTP_MAX_STEPS", "not-a-number");
        std::env::set_var("RECURSIVE_HTTP_WALL_TIMEOUT_SECS", "");
        assert_eq!(http_session_budget_from_env(), (100, 1800));

        // Restore so unrelated tests observe a clean environment.
        std::env::remove_var("RECURSIVE_HTTP_MAX_STEPS");
        std::env::remove_var("RECURSIVE_HTTP_WALL_TIMEOUT_SECS");
    }
}

// =====================================================================
// Goal 396 — host-layer transcript persistence. Pins the three
// teardown-path contracts: idle eviction persists each session's own
// transcript (no cross-session bleed), the save runs OUTSIDE the
// sessions write lock (probed from inside the backend), and the
// graceful-shutdown flush persists and drains every live session.
// =====================================================================
#[cfg(test)]
mod goal_396_persistence_tests {
    use super::*;
    use crate::llm::{Completion, MockProvider};
    use crate::message::{Message, Role};
    use crate::runtime::AgentRuntimeBuilder;
    use crate::storage::StorageBackend;
    use std::path::PathBuf;

    type SessionsMap = HashMap<String, SessionState>;

    /// One recorded save: (session_id, message count, lock-was-free flag).
    type SaveRecord = (String, usize, Option<bool>);

    /// Fake backend that records saves and optionally probes the host's
    /// sessions-map write lock at save time — before any await — so a
    /// host that persisted under the lock fails the test.
    struct RecordingStorage {
        saves: std::sync::Mutex<Vec<SaveRecord>>,
        probe_sessions: Option<Arc<RwLock<SessionsMap>>>,
    }

    impl RecordingStorage {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                saves: std::sync::Mutex::new(Vec::new()),
                probe_sessions: None,
            })
        }

        fn with_probe(sessions: Arc<RwLock<SessionsMap>>) -> Arc<Self> {
            Arc::new(Self {
                saves: std::sync::Mutex::new(Vec::new()),
                probe_sessions: Some(sessions),
            })
        }

        fn saves(&self) -> Vec<SaveRecord> {
            self.saves.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl StorageBackend for RecordingStorage {
        async fn load_transcript(&self, _session_id: &str) -> crate::error::Result<Vec<Message>> {
            Ok(vec![])
        }

        async fn save_transcript(
            &self,
            session_id: &str,
            messages: &[Message],
        ) -> crate::error::Result<()> {
            let lock_was_free = self.probe_sessions.as_ref().map(|m| m.try_write().is_ok());
            self.saves.lock().unwrap().push((
                session_id.to_string(),
                messages.len(),
                lock_was_free,
            ));
            Ok(())
        }

        async fn load_memory(&self, _key: &str) -> crate::error::Result<Option<String>> {
            Ok(None)
        }

        async fn save_memory(&self, _key: &str, _value: &str) -> crate::error::Result<()> {
            Ok(())
        }
    }

    fn test_config() -> crate::config::Config {
        crate::config::Config {
            workspace: PathBuf::from("."),
            api_base: String::new(),
            api_key: None,
            model: String::new(),
            provider_type: "openai".into(),
            preset: None,
            max_steps: 32,
            max_tokens: 65536,
            temperature: 0.2,
            system_prompt: String::new(),
            retry_max: 2,
            retry_initial_backoff_secs: 1,
            retry_max_backoff_secs: 8,
            shell_timeout_secs: 300,
            headless: false,
            memory_summary_limit: 5,
            thinking_budget: None,
            session_name: None,
            max_budget_usd: None,
            extra_dirs: Vec::new(),
            extra_readonly_dirs: Vec::new(),
            allow_tools: Vec::new(),
            context_window_override: None,
            subagent_max_depth: 2,
            subagent_enabled: false,
            allow_bypass_permissions: false,
            max_search_rounds: 3,
            stuck_window: 10,
            stuck_error_rate: 0.8,
            max_concurrent_runs: 8,
            goal_eval_transcript_tail: 12,
            web_search_provider: None,
            web_search_api_key: None,
            web_search_jina_key: None,
            wall_timeout_secs: 0,
        }
    }

    /// A session whose transcript is `n` user/assistant exchanges plus a
    /// trailing system prompt, so two sessions have distinguishable saves.
    fn test_session(id: &str, exchanges: usize) -> SessionState {
        let provider: Arc<dyn ChatProvider> = Arc::new(MockProvider::new(vec![Completion {
            content: "ok".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let mut runtime = AgentRuntimeBuilder::new()
            .llm(provider)
            .system_prompt(format!("system-for-{id}"))
            .build()
            .expect("runtime build must succeed");
        let mut transcript = Vec::new();
        for i in 0..exchanges {
            transcript.push(Message {
                role: Role::User,
                content: format!("{id}-user-{i}"),
                tool_calls: vec![],
                tool_call_id: None,
                reasoning_content: None,
                is_compaction_summary: false,
            });
            transcript.push(Message {
                role: Role::Assistant,
                content: format!("{id}-assistant-{i}"),
                tool_calls: vec![],
                tool_call_id: None,
                reasoning_content: None,
                is_compaction_summary: false,
            });
        }
        runtime.set_transcript(transcript);

        SessionState {
            id: id.to_string(),
            created_at: "2026-09-27T00:00:00Z".to_string(),
            title: None,
            runtime: Arc::new(tokio::sync::Mutex::new(runtime)),
            plan_approval_gate: Arc::new(crate::tools::plan_mode::PlanApprovalGate::new()),
            interrupt_token: Arc::new(tokio::sync::Mutex::new(None)),
            non_system_message_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            last_active_ms: Arc::new(AtomicU64::new(now_session_ms())),
            prompt_tokens: Arc::new(AtomicU64::new(0)),
            completion_tokens: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Session host with the given idle TTL — the eviction tests need the
    /// same sessions map the storage probe observes.
    fn test_host(ttl_secs: u64) -> Arc<SessionHost<SessionState>> {
        Arc::new(SessionHost::new(
            std::time::Duration::from_secs(ttl_secs),
            AdmissionGate::new(
                8,
                std::time::Duration::ZERO,
                Arc::new(AtomicU64::new(0)),
                Arc::new(AtomicU64::new(0)),
            ),
        ))
    }

    async fn test_state(
        host: Arc<SessionHost<SessionState>>,
        storage: Arc<dyn StorageBackend>,
    ) -> AppState {
        AppState {
            tools: vec![],
            tool_registry: crate::tools::ToolRegistry::local(),
            config: test_config(),
            provider: Arc::new(MockProvider::new(vec![])),
            host,
            event_channels: Arc::new(RwLock::new(HashMap::new())),
            metrics: Arc::new(Metrics::default()),
            slash_commands: Arc::new(Vec::new()),
            rate_limiter: RateLimiter::new(10, 1.0),
            skills: vec![],
            storage,
        }
    }

    /// Issue #65: `session_tool_registry` is the choke point every handler
    /// builds sessions from, so the operator allow-list must hold there —
    /// including on a registry that arrives unfiltered (the container tier
    /// rebuilds one from scratch per session; the clone path re-filters
    /// idempotently).
    #[tokio::test]
    async fn session_tool_registry_applies_allow_tools() {
        let host = test_host(0);
        let storage: Arc<dyn StorageBackend> = RecordingStorage::new();
        let mut config = test_config();
        config.allow_tools = vec!["Read".into()];
        let state = AppState {
            tools: vec![],
            tool_registry: crate::tools::build_standard_tools(std::path::Path::new("."), &[], 30),
            config,
            provider: Arc::new(MockProvider::new(vec![])),
            host,
            event_channels: Arc::new(RwLock::new(HashMap::new())),
            metrics: Arc::new(Metrics::default()),
            slash_commands: Arc::new(Vec::new()),
            rate_limiter: RateLimiter::new(10, 1.0),
            skills: vec![],
            storage,
        };

        let registry = state.session_tool_registry().await.expect("registry");
        assert!(
            registry.find_by_name("Read").is_some(),
            "allow-listed tools must survive"
        );
        for dropped in ["Write", "Edit", "Bash", "TodoWrite"] {
            assert!(
                registry.find_by_name(dropped).is_none(),
                "{dropped} is outside RECURSIVE_ALLOW_TOOLS and must not leak \
                 into per-session registries"
            );
        }
    }

    #[tokio::test]
    async fn evict_persists_each_sessions_transcript_outside_the_lock() {
        let host = test_host(0);
        let sessions = host.sessions();
        let storage = RecordingStorage::with_probe(sessions.clone());
        let state = test_state(host, storage.clone()).await;

        state
            .host
            .insert("s-a".into(), test_session("s-a", 2))
            .await;
        state
            .host
            .insert("s-b".into(), test_session("s-b", 1))
            .await;

        let evicted = evict_idle_sessions(&state).await;
        let mut ids = evicted.clone();
        ids.sort();
        assert_eq!(ids, vec!["s-a", "s-b"], "both idle sessions must evict");

        let mut saves = storage.saves();
        saves.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(saves.len(), 2, "exactly one save per evicted session");
        // Per-session isolation: each save carries that session's exchange
        // count (2 user+assistant pairs for s-a, 1 for s-b), never a mix.
        assert_eq!(saves[0].0, "s-a");
        assert_eq!(saves[0].1, 4, "s-a transcript must hold its own 4 messages");
        assert_eq!(saves[1].0, "s-b");
        assert_eq!(saves[1].1, 2, "s-b transcript must hold its own 2 messages");
        // Lock-scope rule: every save observed the sessions map writable.
        for (id, _, lock_was_free) in &saves {
            assert_eq!(
                *lock_was_free,
                Some(true),
                "save for {id} must run outside the sessions write lock"
            );
        }
        assert!(
            state.host.sessions().read().await.is_empty(),
            "evicted sessions must be gone from the map"
        );
    }

    #[tokio::test]
    async fn evict_skips_busy_session_in_place_without_persistence() {
        let host = test_host(0);
        let storage = RecordingStorage::new();
        let state = test_state(host, storage.clone()).await;

        state
            .host
            .insert("busy".into(), test_session("busy", 1))
            .await;
        // Hold the runtime lock — an in-flight turn. Clone the Arc so the
        // sessions read guard drops before the sweep (else the sweep's
        // write lock would deadlock against this test's read lock).
        let rt_arc = state
            .host
            .sessions()
            .read()
            .await
            .get("busy")
            .unwrap()
            .runtime
            .clone();
        let _guard = rt_arc.lock().await;

        let evicted = evict_idle_sessions(&state).await;
        assert!(
            evicted.is_empty(),
            "busy session is skipped in place (Goal 395), not evicted"
        );
        assert!(
            state.host.contains_key("busy").await,
            "busy session stays in the table for a later sweep"
        );
        assert!(
            storage.saves().is_empty(),
            "no transcript must be persisted while the runtime lock is held"
        );
    }

    #[tokio::test]
    async fn flush_all_persists_and_drains_every_session() {
        let host = test_host(0);
        let sessions = host.sessions();
        let storage = RecordingStorage::with_probe(sessions.clone());
        let state = test_state(host, storage.clone()).await;

        state
            .host
            .insert("f-1".into(), test_session("f-1", 1))
            .await;
        state
            .host
            .insert("f-2".into(), test_session("f-2", 3))
            .await;

        let persisted = flush_all_sessions(&state).await;
        assert_eq!(persisted, 2, "both live sessions must be persisted");

        let mut saves = storage.saves();
        saves.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(saves[0].0, "f-1");
        assert_eq!(saves[0].1, 2);
        assert_eq!(saves[1].0, "f-2");
        assert_eq!(saves[1].1, 6);
        assert!(
            saves.iter().all(|(_, _, free)| *free == Some(true)),
            "shutdown flush saves must also run outside the sessions lock"
        );
        assert!(
            state.host.sessions().read().await.is_empty(),
            "flush must drain the session map"
        );
    }
}

// =====================================================================
// Goal 403 (issue §3 / E2): the HTTP entry must honor
// RECURSIVE_SANDBOX=container the same way the CLI does. Verified at
// the source level in two parts:
//   1. the `recursive http` entry builds its startup registry through
//      `cli::builder::build_tools` (which dispatches on SandboxMode), and
//   2. every per-session build point substitutes a FRESH
//      ContainerToolSetProvider registry (one container per session) via
//      `AppState::session_tool_registry` — the registry built at startup
//      must NOT be shared into sessions in the container tier.
// Wiring a live container into unit tests is out of scope here; the
// builder dispatch itself is covered by the container provider tests and
// the gated integration tests (tests/sandbox_container.rs).
// =====================================================================
#[cfg(test)]
mod goal_403_http_sandbox_entry {
    #[test]
    fn http_entry_builds_tools_via_shared_builder_not_local_registry() {
        let src = include_str!("../../crates/recursive-cli/src/main.rs");
        let http_block = src
            .split("Cmd::Http { addr } => {")
            .nth(1)
            .expect("HTTP entry block must exist");
        assert!(
            http_block.contains("cli::builder::build_tools"),
            "HTTP entry must build its tool registry through cli::builder::build_tools \
             (which dispatches RECURSIVE_SANDBOX=container to ContainerToolSetProvider), \
             not a local-only registry"
        );
    }

    /// Issues #70 / #65: the base registry alone is not enough — the HTTP
    /// entry must run the shared cross-cutting tail (`finish_tool_surface`)
    /// so MCP tools (#70) and the coordinator / allow-list pruning (#65)
    /// land on the registry that backs `/tools` and every session.
    #[test]
    fn http_entry_applies_the_shared_tool_surface_tail() {
        let src = include_str!("../../crates/recursive-cli/src/main.rs");
        let http_block = src
            .split("Cmd::Http { addr } => {")
            .nth(1)
            .expect("HTTP entry block must exist")
            .split("Cmd::Run { goal } => {")
            .next()
            .expect("HTTP entry block must be terminated by the Run arm");
        assert!(
            http_block.contains("finish_tool_surface"),
            "HTTP entry must run cli::builder::finish_tool_surface on top of \
             build_tools — a base-only registry silently drops MCP tools \
             (#70) and ignores RECURSIVE_ALLOW_TOOLS (#65)"
        );
        assert!(
            http_block.contains("apply_operator_allow_list"),
            "HTTP entry must apply the operator allow-list AFTER sub-agent \
             registration — tools registered post-prune (agent/send_message/\
             list_workers) would otherwise escape RECURSIVE_ALLOW_TOOLS (#65)"
        );
    }

    #[test]
    fn http_entry_applies_allow_tools_narrowing() {
        let src = include_str!("../../crates/recursive-cli/src/main.rs");
        let http_block = src
            .split("Cmd::Http { addr } => {")
            .nth(1)
            .expect("HTTP entry block must exist");
        assert!(
            http_block.contains("tools.retain_tools(&config.allow_tools)"),
            "HTTP entry must apply config.allow_tools narrowing to its startup \
             registry (issue #69: RECURSIVE_ALLOW_TOOLS had no effect on \
             `recursive http`)"
        );
    }

    #[test]
    fn session_rebind_reapplies_allow_tools_in_container_tier() {
        // Issue #69: the container tier rebuilds the registry per session,
        // bypassing the startup narrowing — rebind must reapply it.
        let src = include_str!("mod.rs").replace("\r\n", "\n");
        let block = src
            .split("async fn rebind_per_session_registry")
            .nth(1)
            .and_then(|rest| rest.split("impl AppState").next())
            .expect("rebind_per_session_registry must exist");
        assert!(
            block.contains("reg.retain_tools(&config.allow_tools)"),
            "rebind_per_session_registry must reapply allow_tools narrowing to \
             the per-session container registry"
        );
    }

    #[test]
    fn builder_dispatches_container_tier_to_container_provider() {
        // The builder's Container arm must construct a
        // ContainerToolSetProvider and build the registry through the
        // ToolSetProvider — that is what makes the source-level assertion
        // above non-vacuous.
        let src = include_str!("../../crates/recursive-cli/src/cli/builder.rs");
        let container_arm = src
            .split("Some(recursive::SandboxMode::Container) => {")
            .nth(1)
            .and_then(|rest| rest.split("Some(recursive::SandboxMode::Policy)").next())
            .expect("builder must have a Container match arm");
        assert!(
            container_arm.contains("ContainerToolSetProvider"),
            "builder Container arm must dispatch to ContainerToolSetProvider"
        );
        assert!(
            container_arm.contains("build_registry"),
            "builder Container arm must build the registry via the provider"
        );
        // No silent fallback: the non-cloud-runtime build must refuse.
        let no_feature = src
            .split("#[cfg(not(feature = \"cloud-runtime\"))]")
            .nth(1)
            .and_then(|rest| rest.split("Some(recursive::SandboxMode::Policy)").next())
            .expect("non-cloud-runtime container arm must exist");
        assert!(
            no_feature.contains("std::process::exit(2)"),
            "container tier without cloud-runtime must exit(2), not degrade to local"
        );
    }

    #[test]
    fn sessions_rebind_their_own_registry_in_container_tier() {
        // Windows checkout 是 CRLF（git autocrlf），而 include_str! 原样嵌入文件——
        // 多行源码断言必须在归一化行尾之后再匹配，否则只在 windows-latest 上挂
        // （2026-09-29 实测：main 的 windows 矩阵因此变红，ubuntu/macOS 正常）。
        let src = include_str!("handlers.rs").replace("\r\n", "\n");
        assert!(
            !src.contains("state.tool_registry.clone()"),
            "per-session build points must go through \
             AppState::session_tool_registry(), never clone the shared \
             startup registry directly (container tier = one container \
             per session, issue §3)"
        );
        assert!(
            src.contains("state\n        .session_tool_registry()\n        .await"),
            "per-session runtimes must be built from session_tool_registry() \
             (issue #31 made it async/Result-shaped)"
        );
        // Issue #31: the Result-shaped registry build must NOT fall back to
        // the shared startup registry on failure — container-tier failures
        // are per-session errors (503), never a silent share.
        assert!(
            !src.contains("tool_registry.clone().await"),
            "session_tool_registry() failures must propagate, not clone the shared registry"
        );
    }
}
