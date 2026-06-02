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

use crate::config::Config;
use crate::event::{AgentEvent, ChannelSink, NullSink};
use crate::llm::LlmProvider;
use crate::runtime::{AgentRuntime, AgentRuntimeBuilder};
use crate::tools::plan_mode::PlanApprovalGate;
use crate::tools::ToolRegistry;

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

// ── Authentication (API keys) ──────────────────────────────────────────────

/// API key authentication for the HTTP server.
///
/// Configured from `RECURSIVE_HTTP_AUTH_KEYS`, a comma-separated list of
/// keys the server will accept in the `X-API-Key` request header. An empty
/// key set (the default) disables auth entirely — every route is reachable
/// without credentials. This preserves zero-config behavior and keeps the
/// public default backward-compatible.
///
/// Distinct from `RECURSIVE_API_KEY` (singular): that variable holds the
/// **outbound** credential the agent uses to talk to its LLM provider.
/// `RECURSIVE_HTTP_AUTH_KEYS` (plural) holds the **inbound** credentials
/// the HTTP server accepts from clients. The names are deliberately
/// dissimilar to avoid confusion at the operator's shell.
///
/// `/health` and `/metrics` are always exempt (k8s liveness probes and
/// Prometheus scrapers must work unauthenticated).
#[derive(Clone, Default)]
pub struct AuthConfig {
    keys: Arc<Vec<String>>,
    jwt: Option<JwtConfig>,
}

impl AuthConfig {
    /// Build an `AuthConfig` from an explicit key list. Pass an empty
    /// vec to disable API-key auth (a JWT verifier may still be
    /// attached via [`AuthConfig::with_jwt`]).
    pub fn new(keys: Vec<String>) -> Self {
        Self {
            keys: Arc::new(keys),
            jwt: None,
        }
    }

    /// Attach a JWT verifier. Call after [`AuthConfig::new`] to get
    /// "X-API-Key OR Bearer JWT" semantics — either valid credential
    /// type lets a request through. Without this call the behavior
    /// is X-API-Key-only (the original g135 behavior).
    pub fn with_jwt(mut self, jwt: JwtConfig) -> Self {
        self.jwt = Some(jwt);
        self
    }

    /// Constant-time check whether `presented` is in the configured
    /// API-key set.
    ///
    /// Returns `true` if no API keys are configured. Endpoints must
    /// rely on the middleware layering, not this method, for the
    /// "auth disabled" semantics — see [`auth_middleware`].
    ///
    /// The loop runs over **every** configured key regardless of an
    /// early match, to keep the comparison constant-time and avoid
    /// leaking key-set membership timing.
    pub fn is_valid(&self, presented: &str) -> bool {
        if self.keys.is_empty() {
            return true;
        }
        let mut found = false;
        let presented_bytes = presented.as_bytes();
        for k in self.keys.iter() {
            let k_bytes = k.as_bytes();
            if k_bytes.len() != presented_bytes.len() {
                continue;
            }
            let mut diff: u8 = 0;
            for (a, b) in k_bytes.iter().zip(presented_bytes.iter()) {
                diff |= a ^ b;
            }
            if diff == 0 {
                found = true;
            }
        }
        found
    }

    /// Whether ANY auth modality is enabled — non-empty API key set
    /// OR a JWT verifier attached. When this returns `false`, the
    /// middleware is a pass-through.
    pub fn is_enabled(&self) -> bool {
        !self.keys.is_empty() || self.jwt.is_some()
    }
}

/// JWT bearer token verification config.
///
/// Verify-only: this server validates tokens minted elsewhere; it does
/// not issue them. HS256 (HMAC-SHA256 with a shared secret) is the
/// only supported algorithm in this revision — keeps secret management
/// simple (one env var). RSA/ECDSA can be added later if a deployment
/// needs JWKS-driven key rotation.
///
/// Configured from:
/// - `RECURSIVE_HTTP_AUTH_JWT_SECRET` — HMAC secret bytes (UTF-8). Empty
///   or unset disables JWT auth.
/// - `RECURSIVE_HTTP_AUTH_JWT_AUDIENCE` — optional `aud` claim that
///   tokens must contain. Unset = audience claim ignored (still valid
///   JWT spec, just less strict).
///
/// `exp` claim is always required (RFC 7519 says optional; we make it
/// mandatory to prevent unbounded-validity tokens).
#[derive(Clone)]
pub struct JwtConfig {
    decoding_key: jsonwebtoken::DecodingKey,
    validation: jsonwebtoken::Validation,
}

impl JwtConfig {
    /// Build an HS256 verifier. Returns `None` if `secret` is empty
    /// (parallels `AuthConfig`'s "empty = disabled" pattern).
    ///
    /// `audience` is optional: `Some("my-app")` requires tokens carry
    /// `"aud": "my-app"`; `None` skips audience checking entirely.
    pub fn hs256(secret: &str, audience: Option<String>) -> Option<Self> {
        if secret.is_empty() {
            return None;
        }
        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
        validation.set_required_spec_claims(&["exp"]);
        if let Some(aud) = audience {
            validation.set_audience(&[aud]);
        } else {
            validation.validate_aud = false;
        }
        Some(Self {
            decoding_key: jsonwebtoken::DecodingKey::from_secret(secret.as_bytes()),
            validation,
        })
    }

    /// Verify a token. Returns true iff signature, exp, and (when
    /// configured) audience all check out.
    pub fn is_valid(&self, token: &str) -> bool {
        jsonwebtoken::decode::<serde_json::Value>(token, &self.decoding_key, &self.validation)
            .is_ok()
    }
}

/// Build `AuthConfig` from env vars:
///
/// - `RECURSIVE_HTTP_AUTH_KEYS` — comma-separated API keys (g135).
/// - `RECURSIVE_HTTP_AUTH_JWT_SECRET` — HMAC secret for JWT (g136).
/// - `RECURSIVE_HTTP_AUTH_JWT_AUDIENCE` — optional `aud` claim.
///
/// All unset = auth disabled (back-compat zero-config default).
fn auth_config_from_env() -> AuthConfig {
    let raw = std::env::var("RECURSIVE_HTTP_AUTH_KEYS").unwrap_or_default();
    let keys: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let mut config = AuthConfig::new(keys);
    let jwt_secret = std::env::var("RECURSIVE_HTTP_AUTH_JWT_SECRET").unwrap_or_default();
    let jwt_audience = std::env::var("RECURSIVE_HTTP_AUTH_JWT_AUDIENCE")
        .ok()
        .filter(|s| !s.is_empty());
    if let Some(jwt) = JwtConfig::hs256(&jwt_secret, jwt_audience) {
        config = config.with_jwt(jwt);
    }
    config
}

/// Axum middleware: enforce auth on requests.
///
/// Tries `X-API-Key` first (cheap); falls back to
/// `Authorization: Bearer <jwt>`. Either valid credential lets the
/// request through.
///
/// Layered after the router so that all routes pass through it, but
/// `/health` and `/metrics` are explicitly exempted to keep liveness
/// probes and metrics scraping reachable without credentials.
///
/// When auth is disabled (no API keys AND no JWT verifier
/// configured), the middleware is a no-op pass-through — preserving
/// back-compat zero-config behavior.
async fn auth_middleware(
    State(auth): State<AuthConfig>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if !auth.is_enabled() {
        return next.run(req).await;
    }
    let path = req.uri().path();
    if path == "/health" || path == "/metrics" {
        return next.run(req).await;
    }
    // Try X-API-Key first (cheaper than JWT verify).
    if !auth.keys.is_empty() {
        if let Some(presented) = req.headers().get("x-api-key").and_then(|v| v.to_str().ok()) {
            if auth.is_valid(presented) {
                return next.run(req).await;
            }
        }
    }
    // Then try Authorization: Bearer <jwt>.
    if let Some(ref jwt) = auth.jwt {
        if let Some(authz) = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
        {
            if let Some(token) = authz.strip_prefix("Bearer ") {
                if jwt.is_valid(token) {
                    return next.run(req).await;
                }
            }
        }
    }
    let mut resp = axum::response::Response::new(axum::body::Body::from("unauthorized"));
    *resp.status_mut() = StatusCode::UNAUTHORIZED;
    resp
}

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
    ///
    /// External callers (tests, custom embedders) can construct a
    /// `RateLimiter` directly and inject it via
    /// [`build_router_with_auth_and_rate_limit`] when env-driven
    /// configuration is undesirable.
    pub fn new(capacity: u32, refill_rate: f64) -> Self {
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
///
/// The [`AgentRuntime`] owns the transcript; this struct adds HTTP-layer
/// metadata (id, created_at) and the broadcast channel for SSE clients.
pub struct SessionState {
    pub id: String,
    pub created_at: String,
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
    /// Goal-167: current task list as maintained by `todo_write` calls.
    pub todos: Vec<crate::tools::todo::TodoItem>,
    /// Current session lifecycle state: `"idle"` | `"plan_pending_approval"`.
    pub status: String,
    /// Non-null when `status` is `"plan_pending_approval"`.
    pub pending_plan: Option<String>,
    /// Goal-168: active goal state, or `null` when no goal is set.
    pub goal: Option<crate::runtime::GoalState>,
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

    let mut runtime = AgentRuntimeBuilder::new()
        .llm(state.provider.clone())
        .tools(state.tool_registry.clone())
        .system_prompt(system_prompt)
        .max_steps(max_steps)
        .build()
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    status: "error".into(),
                    error: format!("failed to build runtime: {e}"),
                }),
            )
        })?;

    let outcome = runtime.run(&body.goal).await.map_err(|e| {
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
    let messages: Vec<serde_json::Value> = runtime
        .transcript()
        .iter()
        .filter_map(|msg| serde_json::to_value(msg).ok())
        .collect();

    let finish_reason = format!("{:?}", outcome.finish_reason);

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
) -> Result<(StatusCode, Json<CreateSessionResponse>), (StatusCode, Json<ErrorResponse>)> {
    let id = generate_session_id();
    let created_at = format_timestamp(SystemTime::now());
    let system_prompt = body
        .system_prompt
        .unwrap_or_else(|| state.config.system_prompt.clone());

    let runtime = AgentRuntimeBuilder::new()
        .llm(state.provider.clone())
        .tools(state.tool_registry.clone())
        .system_prompt(system_prompt)
        .max_steps(state.config.max_steps)
        .build()
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    status: "error".into(),
                    error: format!("failed to build session runtime: {e}"),
                }),
            )
        })?;

    // Extract the gate before moving runtime into the Mutex so HTTP handlers
    // can approve/reject without acquiring the per-session runtime lock.
    let plan_approval_gate = runtime.plan_approval_gate();

    let session = SessionState {
        id: id.clone(),
        created_at: created_at.clone(),
        runtime: Arc::new(tokio::sync::Mutex::new(runtime)),
        plan_approval_gate,
        interrupt_token: Arc::new(tokio::sync::Mutex::new(None)),
    };

    state.sessions.write().await.insert(id.clone(), session);

    Ok((
        StatusCode::CREATED,
        Json(CreateSessionResponse { id, created_at }),
    ))
}

/// GET /sessions — list all sessions.
async fn list_sessions(State(state): State<Arc<AppState>>) -> Json<Vec<SessionInfo>> {
    let sessions = state.sessions.read().await;
    let mut infos = Vec::with_capacity(sessions.len());
    for s in sessions.values() {
        // Exclude the system-prompt message from the user-visible count.
        let message_count = s
            .runtime
            .lock()
            .await
            .transcript()
            .iter()
            .filter(|m| m.role != crate::message::Role::System)
            .count();
        infos.push(SessionInfo {
            id: s.id.clone(),
            created_at: s.created_at.clone(),
            message_count,
        });
    }
    Json(infos)
}

/// GET /sessions/:id — get session detail with messages.
///
/// Reads plan-approval status directly from the session gate (no runtime lock
/// needed) so this endpoint stays responsive even while an agent turn is
/// blocked awaiting plan approval.  Messages and todos fall back to empty
/// vectors when the runtime is busy rather than deadlocking.
async fn get_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<SessionDetailResponse>, StatusCode> {
    let sessions = state.sessions.read().await;
    let session = sessions.get(&id).ok_or(StatusCode::NOT_FOUND)?;

    // Read plan status without locking the runtime Mutex so callers can poll
    // while the agent is suspended inside `exit_plan_mode`.
    let pending_plan = session
        .plan_approval_gate
        .pending_plan
        .read()
        .ok()
        .and_then(|g| g.clone());
    let status = if pending_plan.is_some() {
        "plan_pending_approval".to_string()
    } else {
        "idle".to_string()
    };

    // Try a non-blocking lock for messages/todos/goal; fall back to empty when busy.
    let (messages, todos, goal) = match session.runtime.try_lock() {
        Ok(runtime) => {
            let msgs = runtime
                .transcript()
                .iter()
                .filter_map(|msg| serde_json::to_value(msg).ok())
                .collect();
            let todos = runtime.current_todos();
            let goal = runtime.current_goal();
            (msgs, todos, goal)
        }
        Err(_) => (vec![], vec![], None),
    };

    Ok(Json(SessionDetailResponse {
        id: session.id.clone(),
        created_at: session.created_at.clone(),
        messages,
        todos,
        status,
        pending_plan,
        goal,
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

// ── Plan-approval endpoints ───────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct PlanConfirmRequest {
    /// Optional replacement plan text to use instead of the agent-proposed one.
    edits: Option<String>,
}

#[derive(serde::Deserialize)]
struct PlanRejectRequest {
    /// Reason shown to the agent so it can revise the plan.
    reason: Option<String>,
}

/// POST /sessions/:id/plan/confirm — approve the pending plan.
async fn session_plan_confirm(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    Json(body): Json<PlanConfirmRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let sessions = state.sessions.read().await;
    let Some(session) = sessions.get(&session_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "session not found"})),
        );
    };
    let pending = session
        .plan_approval_gate
        .pending_plan
        .read()
        .ok()
        .and_then(|g| g.clone());
    if pending.is_none() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "session is not awaiting plan approval"})),
        );
    }
    // Optionally replace the plan text before approving.
    if let Some(edited) = body.edits {
        if let Ok(mut w) = session.plan_approval_gate.pending_plan.write() {
            *w = Some(edited);
        }
    }
    session.plan_approval_gate.approve();
    (
        StatusCode::OK,
        Json(serde_json::json!({"status": "approved", "session_id": session_id})),
    )
}

/// POST /sessions/:id/plan/reject — reject the pending plan.
async fn session_plan_reject(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    Json(body): Json<PlanRejectRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let sessions = state.sessions.read().await;
    let Some(session) = sessions.get(&session_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "session not found"})),
        );
    };
    let pending = session
        .plan_approval_gate
        .pending_plan
        .read()
        .ok()
        .and_then(|g| g.clone());
    if pending.is_none() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "session is not awaiting plan approval"})),
        );
    }
    let reason = body.reason.unwrap_or_default();
    session.plan_approval_gate.reject(&reason);
    (
        StatusCode::OK,
        Json(serde_json::json!({"status": "rejected", "session_id": session_id})),
    )
}

// ── Goal-168: goal endpoints ──────────────────────────────────────────────

/// POST /sessions/:id/goal — start a condition-based autonomous loop.
async fn session_set_goal(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    Json(body): Json<SetGoalRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let runtime_arc = {
        let sessions = state.sessions.read().await;
        let Some(session) = sessions.get(&session_id) else {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "session not found"})),
            );
        };
        session.runtime.clone()
    };

    let condition = body.condition.clone();
    let max_turns = body.max_turns.unwrap_or(20);

    // Lock runtime and set goal state (non-blocking; loop runs in background).
    match runtime_arc.try_lock() {
        Ok(runtime) => {
            runtime.set_goal(condition, max_turns).await;
        }
        Err(_) => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"error": "session runtime is busy"})),
            );
        }
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({"status": "pursuing", "session_id": session_id})),
    )
}

/// DELETE /sessions/:id/goal — clear the active goal.
async fn session_clear_goal(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let sessions = state.sessions.read().await;
    let Some(session) = sessions.get(&session_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "session not found"})),
        );
    };

    match session.runtime.try_lock() {
        Ok(runtime) => {
            runtime.clear_goal().await;
        }
        Err(_) => {
            // Runtime is busy; force-clear via the shared goal_state.
            let _ = runtime_goal_state_clear(&session.runtime).await;
        }
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({"status": "cleared", "session_id": session_id})),
    )
}

/// Force-clear goal state when the runtime Mutex is held.
async fn runtime_goal_state_clear(runtime: &Arc<tokio::sync::Mutex<AgentRuntime>>) {
    // Best-effort: try up to 5 times with a small delay.
    for _ in 0..5u8 {
        if let Ok(rt) = runtime.try_lock() {
            rt.clear_goal().await;
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

// ── Goal-170: interrupt endpoint ───────────────────────────────────────────

/// POST /sessions/:id/interrupt — cancel the active agent turn.
///
/// Cancels the `CancellationToken` installed at the start of the current
/// turn. The kernel exits with `FinishReason::Cancelled` at the next step
/// boundary.  If no turn is in progress the request is still `200 OK`
/// (idempotent — no harm done).
async fn session_interrupt(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let token_arc = {
        let sessions = state.sessions.read().await;
        let Some(session) = sessions.get(&session_id) else {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "session not found"})),
            );
        };
        session.interrupt_token.clone()
    };

    // Cancel the current token if one is installed.
    let token_opt = token_arc.lock().await.clone();
    if let Some(token) = token_opt {
        token.cancel();
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({"status": "interrupted", "session_id": session_id})),
    )
}

// ── Goal-169: slash commands endpoint ─────────────────────────────────────

/// GET /slash-commands — list all registered slash commands.
async fn list_slash_commands(State(state): State<Arc<AppState>>) -> Json<Vec<SlashCommandInfo>> {
    Json((*state.slash_commands).clone())
}

/// POST /sessions/:id/messages — send a message in a session.
async fn send_session_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<SessionMessageRequest>,
) -> Result<Json<SessionMessageResponse>, (StatusCode, Json<ErrorResponse>)> {
    // Get the session's runtime and interrupt token (Arc clones are cheap).
    let (runtime_arc, interrupt_token_arc) = {
        let sessions = state.sessions.read().await;
        let session = sessions.get(&id).ok_or((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                status: "error".into(),
                error: "session not found".into(),
            }),
        ))?;
        (session.runtime.clone(), session.interrupt_token.clone())
    };

    // Ensure broadcast channel exists for this session before we lock the runtime.
    let broadcast_tx = {
        let mut channels = state.event_channels.write().await;
        let tx = channels.entry(id.clone()).or_insert_with(|| {
            let (tx, _) = broadcast::channel(64);
            tx
        });
        tx.clone()
    };

    // Lock the runtime for this turn (serializes concurrent requests per session).
    let mut runtime = runtime_arc.lock().await;

    // Goal-170: install a fresh cancellation token so `POST .../interrupt`
    // can cancel this turn without affecting future turns.
    let interrupt_token = tokio_util::sync::CancellationToken::new();
    {
        let mut stored = interrupt_token_arc.lock().await;
        *stored = Some(interrupt_token.clone());
    }
    runtime.set_interrupt_token(interrupt_token);

    // Wire a ChannelSink so events are forwarded to SSE subscribers.
    let (sink, mut event_rx) = ChannelSink::new();
    runtime.set_event_sink(Arc::new(sink));

    // Spawn a forwarder: AgentEvent → SseEvent → broadcast channel.
    // SDK Phase B: track tool call start times so we can emit tool_progress
    // events with elapsed_ms when each tool finishes.
    let forward_handle = tokio::spawn(async move {
        let mut tool_start_times: std::collections::HashMap<String, std::time::Instant> =
            std::collections::HashMap::new();
        while let Some(ref agent_event) = event_rx.recv().await {
            // Record start time for each tool call so we can compute elapsed
            // when the result arrives.
            if let AgentEvent::ToolCall { id, .. } = agent_event {
                tool_start_times.insert(id.clone(), std::time::Instant::now());
            }
            if let Some(sse_event) = map_agent_event(agent_event) {
                let _ = broadcast_tx.send(sse_event);
            }
            // After forwarding the tool_result, emit tool_progress with timing.
            if let AgentEvent::ToolResult { id, name, .. } = agent_event {
                let elapsed_ms = tool_start_times
                    .remove(id)
                    .map(|start| start.elapsed().as_millis() as u64)
                    .unwrap_or(0);
                let _ = broadcast_tx.send(SseEvent::ToolProgress {
                    tool_use_id: id.clone(),
                    tool_name: name.clone(),
                    elapsed_ms,
                });
            }
        }
    });

    // Run the agent turn (transcript is managed internally by AgentRuntime).
    let run_result = runtime.run(&body.content).await;

    // Clear the interrupt token slot — the turn is done.
    {
        let mut stored = interrupt_token_arc.lock().await;
        *stored = None;
    }

    // Disconnect the sink so the forwarder drains and exits.
    runtime.set_event_sink(Arc::new(NullSink));
    let _ = forward_handle.await;

    let _outcome = run_result.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                status: "error".into(),
                error: format!("agent run failed: {e}"),
            }),
        )
    })?;

    // Extract the last assistant message from the runtime's transcript.
    let last_assistant = runtime
        .transcript()
        .iter()
        .rev()
        .find(|m| m.role == crate::message::Role::Assistant)
        .map(|m| m.content.clone())
        .unwrap_or_default();

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
                SseEvent::Message { .. } => "message",
                SseEvent::PartialMessage { .. } => "partial_message",
                SseEvent::ToolCall { .. } => "tool_call",
                SseEvent::ToolResult { .. } => "tool_result",
                SseEvent::Done { .. } => "done",
                SseEvent::Error { .. } => "error",
                SseEvent::PlanProposed { .. } => "plan_proposed",
                SseEvent::GoalContinuing { .. } => "goal_continuing",
                SseEvent::GoalAchieved { .. } => "goal_achieved",
                SseEvent::ToolProgress { .. } => "tool_progress",
            };
            let data = serde_json::to_string(&sse_event).unwrap_or_default();
            Some(Ok(Event::default().event(event_type).data(data)))
        }
        Err(_) => None,
    });

    Ok(Sse::new(stream))
}

// ── Event mapping ────────────────────────────────────────────────────────

/// Map an [`AgentEvent`] to an [`SseEvent`] for broadcasting to SSE clients.
///
/// Returns `None` for events that have no SSE equivalent (latency, tokens, etc.).
pub fn map_agent_event(event: &AgentEvent) -> Option<SseEvent> {
    match event {
        // Streaming token deltas — clients reconstruct the final text by
        // concatenating deltas keyed on `step`.
        AgentEvent::PartialToken { text, step } => Some(SseEvent::PartialMessage {
            text: text.clone(),
            step: *step,
        }),
        // A canonical persisted message — emit it as a typed Message event so
        // SDK consumers iterating `Run.stream()` get role-tagged content
        // (assistant text, tool_use blocks). User and tool messages flow
        // through here too; we only forward roles that are useful to a
        // streaming consumer.
        //
        // We deliberately do NOT also map `AgentEvent::AssistantText` — the
        // runtime emits both `AssistantText` (per-step) and `MessageAppended`
        // (once per committed message), so consuming both would produce
        // duplicate Message events on every assistant turn.
        AgentEvent::MessageAppended { message, .. }
        | AgentEvent::MessageAppendedWithAudit { message, .. } => {
            sse_message_from_canonical(message)
        }
        AgentEvent::ToolCall { name, step, .. } => Some(SseEvent::ToolCall {
            name: name.clone(),
            step: *step,
        }),
        AgentEvent::ToolResult { name, output, .. } => {
            let success = !output.starts_with("ERROR: ");
            Some(SseEvent::ToolResult {
                name: name.clone(),
                success,
            })
        }
        AgentEvent::TurnFinished { reason, steps } => Some(SseEvent::Done {
            finish_reason: reason.clone(),
            total_steps: *steps,
        }),
        AgentEvent::PlanProposed { plan_text, .. } => Some(SseEvent::PlanProposed {
            plan: plan_text.clone(),
        }),
        // Goal-168: forward goal-loop progress events.
        AgentEvent::GoalContinuing { reason, turns } => Some(SseEvent::GoalContinuing {
            reason: reason.clone(),
            turns: *turns,
        }),
        AgentEvent::GoalAchieved { condition, turns } => Some(SseEvent::GoalAchieved {
            condition: condition.clone(),
            turns: *turns,
        }),
        // AssistantText, Latency, Usage, Compacted, PlanConfirmed,
        // PlanRejected don't have SSE equivalents (AssistantText is
        // intentionally suppressed in favour of MessageAppended above).
        _ => None,
    }
}

/// Convert a canonical [`crate::message::Message`] into an [`SseEvent::Message`].
///
/// `system` and `tool` messages are filtered out — system messages carry
/// internal seeds the SDK consumer never asked for, and tool *result*
/// messages are already represented by [`SseEvent::ToolResult`].
fn sse_message_from_canonical(msg: &crate::message::Message) -> Option<SseEvent> {
    use crate::message::Role;
    let role = match msg.role {
        Role::Assistant => "assistant",
        Role::User => "user",
        Role::System | Role::Tool => return None,
    };

    let mut content: Vec<SseContentBlock> = Vec::new();
    if !msg.content.is_empty() {
        content.push(SseContentBlock::Text {
            text: msg.content.clone(),
        });
    }
    for tc in &msg.tool_calls {
        content.push(SseContentBlock::ToolUse {
            id: tc.id.clone(),
            name: tc.name.clone(),
            input: tc.arguments.clone(),
        });
    }
    if content.is_empty() {
        return None;
    }
    Some(SseEvent::Message {
        role: role.into(),
        content,
    })
}

// ── AG-UI endpoint ───────────────────────────────────────────────────────

/// State machine that the AG-UI converter uses to coordinate
/// `TextMessageStart/Content/End` framing across multiple AgentEvents.
///
/// We open a TextMessage on the first `AssistantText`/`PartialToken` we see
/// after every "neutral" point (run start, after `TextMessageEnd`, after
/// tool-call events) and close it explicitly when we emit a fully-formed
/// `AssistantText`, when a `ToolCall` arrives, or when the run finishes.
#[derive(Default)]
struct AguiConverter {
    /// `Some(message_id)` when a TextMessageStart has been emitted but no
    /// TextMessageEnd yet. Used as the `messageId` for streaming
    /// `PartialToken` deltas and as the `parentMessageId` for tool calls.
    open_message_id: Option<String>,
    /// Last fully-emitted (or currently-open) assistant message id. Used as
    /// the `parent_message_id` on ToolCallStart even after the message has
    /// been closed, so a client can attribute the tool call back to the
    /// triggering assistant turn.
    last_assistant_message_id: Option<String>,
}

impl AguiConverter {
    fn new() -> Self {
        Self::default()
    }

    /// Translate one [`AgentEvent`] into zero or more AG-UI events,
    /// updating internal framing state as a side effect.
    fn convert(&mut self, ev: &AgentEvent) -> Vec<agui_protocol::Event> {
        use agui_protocol as ag;
        let mut out = Vec::new();
        match ev {
            AgentEvent::AssistantText { text, .. } => {
                // Close any in-flight streamed message first.
                if let Some(id) = self.open_message_id.take() {
                    out.push(ag::Event::TextMessageEnd(ag::TextMessageEnd {
                        message_id: id,
                        base: ag::BaseEvent::default(),
                    }));
                }
                let id = uuid::Uuid::new_v4().to_string();
                out.push(ag::Event::TextMessageStart(ag::TextMessageStart {
                    message_id: id.clone(),
                    role: Some("assistant".into()),
                    base: ag::BaseEvent::default(),
                }));
                out.push(ag::Event::TextMessageContent(ag::TextMessageContent {
                    message_id: id.clone(),
                    delta: text.clone(),
                    base: ag::BaseEvent::default(),
                }));
                out.push(ag::Event::TextMessageEnd(ag::TextMessageEnd {
                    message_id: id.clone(),
                    base: ag::BaseEvent::default(),
                }));
                self.last_assistant_message_id = Some(id);
                self.open_message_id = None;
            }
            AgentEvent::PartialToken { text, .. } => {
                let id = if let Some(id) = self.open_message_id.clone() {
                    id
                } else {
                    let id = uuid::Uuid::new_v4().to_string();
                    out.push(ag::Event::TextMessageStart(ag::TextMessageStart {
                        message_id: id.clone(),
                        role: Some("assistant".into()),
                        base: ag::BaseEvent::default(),
                    }));
                    self.open_message_id = Some(id.clone());
                    self.last_assistant_message_id = Some(id.clone());
                    id
                };
                out.push(ag::Event::TextMessageContent(ag::TextMessageContent {
                    message_id: id,
                    delta: text.clone(),
                    base: ag::BaseEvent::default(),
                }));
            }
            AgentEvent::ToolCall {
                id,
                name,
                arguments,
                ..
            } => {
                // Close any in-flight streamed assistant message first; the
                // assistant turn is "done" the moment a tool call lands.
                if let Some(open) = self.open_message_id.take() {
                    out.push(ag::Event::TextMessageEnd(ag::TextMessageEnd {
                        message_id: open,
                        base: ag::BaseEvent::default(),
                    }));
                }
                out.push(ag::Event::ToolCallStart(ag::ToolCallStart {
                    tool_call_id: id.clone(),
                    tool_call_name: name.clone(),
                    parent_message_id: self.last_assistant_message_id.clone(),
                    base: ag::BaseEvent::default(),
                }));
                out.push(ag::Event::ToolCallArgs(ag::ToolCallArgs {
                    tool_call_id: id.clone(),
                    delta: arguments.clone(),
                    base: ag::BaseEvent::default(),
                }));
                out.push(ag::Event::ToolCallEnd(ag::ToolCallEnd {
                    tool_call_id: id.clone(),
                    base: ag::BaseEvent::default(),
                }));
            }
            AgentEvent::ToolResult { id, output, .. } => {
                // AG-UI requires a `messageId` on ToolCallResult; reuse the
                // most recent assistant message id as the conversational
                // anchor (mirrors what OpenAI's tool message shape does).
                let message_id = self
                    .last_assistant_message_id
                    .clone()
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                out.push(ag::Event::ToolCallResult(ag::ToolCallResult {
                    tool_call_id: id.clone(),
                    message_id,
                    content: output.clone(),
                    role: Some("tool".into()),
                    base: ag::BaseEvent::default(),
                }));
            }
            AgentEvent::TurnFinished { .. } => {
                // Close any in-flight streamed message before signalling
                // run completion to the client.
                if let Some(open) = self.open_message_id.take() {
                    out.push(ag::Event::TextMessageEnd(ag::TextMessageEnd {
                        message_id: open,
                        base: ag::BaseEvent::default(),
                    }));
                }
                // Actual RunFinished is emitted by the caller (it knows
                // the thread/run ids); we just flush state here.
            }
            // TODO(g141, g140): map permission_request / checkpoint_post /
            // heartbeat / file_artifact onto Custom events here.
            // Other variants (Latency, Usage, Compacted, PlanProposed,
            // PlanConfirmed, PlanRejected) have no AG-UI standard
            // equivalent and are intentionally dropped.
            _ => {}
        }
        out
    }
}

/// Stateless wrapper: maps a single [`AgentEvent`] to AG-UI events
/// using a fresh converter. Useful in tests; production code uses
/// [`AguiConverter::convert`] directly so framing state survives
/// across the whole run.
#[cfg(test)]
fn agui_events_for(ev: &AgentEvent) -> Vec<agui_protocol::Event> {
    AguiConverter::new().convert(ev)
}

/// POST /agui — drive an agent run via the AG-UI protocol and stream
/// AG-UI events back as SSE.
async fn agui_run(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Result<
    Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>,
    (StatusCode, Json<ErrorResponse>),
> {
    use agui_protocol as ag;

    // Parse the body into a typed RunAgentInput. We accept Json<Value>
    // up top so we can return a clean 400 with a helpful message
    // instead of axum's default 422 on shape errors.
    let input: ag::RunAgentInput = serde_json::from_value(body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                status: "error".into(),
                error: format!("invalid AG-UI RunAgentInput: {e}"),
            }),
        )
    })?;

    // Derive the user goal: prefer the last user message, else fall back
    // to the first context item value.
    let goal = input
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .and_then(|m| m.content.clone())
        .or_else(|| input.context.first().map(|c| c.value.clone()))
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    status: "error".into(),
                    error: "RunAgentInput must contain at least one user \
                            message or a non-empty context item"
                        .into(),
                }),
            )
        })?;

    let mut runtime = AgentRuntimeBuilder::new()
        .llm(state.provider.clone())
        .tools(state.tool_registry.clone())
        .system_prompt(state.config.system_prompt.clone())
        .max_steps(state.config.max_steps)
        .build()
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    status: "error".into(),
                    error: format!("failed to build runtime: {e}"),
                }),
            )
        })?;

    // Wire per-turn workspace checkpoints. The AG-UI thread is the
    // natural session boundary, so we use a sanitised version of the
    // thread_id as the checkpoint chain id. Failures (no git on PATH,
    // bad workspace path, etc.) only log a warning — the run still
    // proceeds without checkpoints.
    if let Ok(repo) = crate::ShadowRepo::open(&state.config.workspace) {
        let session_id = sanitize_thread_id_for_session(&input.thread_id);
        if let Ok(session_dir) = crate::user_sessions_dir(&state.config.workspace) {
            let log_dir = session_dir.join(format!("agui-{session_id}"));
            let _ = std::fs::create_dir_all(&log_dir);
            let log_path = log_dir.join("checkpoints.jsonl");
            let touched = runtime.kernel().tools().touched_files();
            if let Err(e) =
                runtime.enable_checkpoints(Arc::new(repo), session_id, log_path, touched)
            {
                tracing::warn!("agui: enable_checkpoints failed, continuing without: {e}");
            }
        }
    } else {
        tracing::debug!("agui: shadow git unavailable, no per-turn checkpoints");
    }

    let (sink, mut event_rx) = ChannelSink::new();
    runtime.set_event_sink(Arc::new(sink));

    // Channel that carries fully-converted AG-UI Events to the SSE stream.
    let (sse_tx, sse_rx) = tokio::sync::mpsc::unbounded_channel::<ag::Event>();
    let thread_id = input.thread_id.clone();
    let run_id = input.run_id.clone();

    // Emit RunStarted up front so clients can render the run shell
    // before the first model token arrives.
    let _ = sse_tx.send(ag::Event::RunStarted(ag::RunStarted {
        thread_id: thread_id.clone(),
        run_id: run_id.clone(),
        base: ag::BaseEvent::default(),
    }));

    // Converter task: forward AgentEvents → AG-UI Events. Owns the
    // AguiConverter so framing state survives across the whole run.
    // It does NOT emit RunFinished — the driver task does that after
    // it can also surface the optional checkpoint_post Custom event.
    let conv_tx = sse_tx.clone();
    let converter_handle = tokio::spawn(async move {
        let mut conv = AguiConverter::new();
        while let Some(agent_event) = event_rx.recv().await {
            for ev in conv.convert(&agent_event) {
                if conv_tx.send(ev).is_err() {
                    return;
                }
            }
        }
    });

    // Drive the agent on a background task so the response stream can
    // flush bytes to the client incrementally. Order of events emitted
    // by the driver after run() returns:
    //   1. Wait for the converter to drain all AgentEvents.
    //   2. If a checkpoint id was produced, emit
    //      Custom("agui-tui/checkpoint_post").
    //   3. Emit RunFinished — always last.
    let metrics = state.metrics.clone();
    let drv_thread = thread_id.clone();
    let drv_run = run_id.clone();
    tokio::spawn(async move {
        let outcome = runtime.run(&goal).await;
        // Replace the sink so the converter task's recv() sees a closed
        // channel and exits cleanly.
        runtime.set_event_sink(Arc::new(NullSink));

        // Snapshot what we need from the outcome before metrics consume it.
        let (checkpoint_id, finished_turn): (Option<String>, Option<usize>) = match &outcome {
            Ok(o) => (
                o.checkpoint_id.as_ref().map(|c| c.0.clone()),
                runtime.turn_index().checked_sub(1),
            ),
            Err(_) => (None, None),
        };

        match outcome {
            Ok(o) => {
                metrics.agent_runs_total.fetch_add(1, Ordering::Relaxed);
                metrics.agent_runs_success.fetch_add(1, Ordering::Relaxed);
                metrics
                    .agent_steps_total
                    .fetch_add(o.steps as u64, Ordering::Relaxed);
                metrics
                    .tokens_prompt_total
                    .fetch_add(o.total_usage.prompt_tokens as u64, Ordering::Relaxed);
                metrics
                    .tokens_completion_total
                    .fetch_add(o.total_usage.completion_tokens as u64, Ordering::Relaxed);
            }
            Err(_) => {
                metrics.agent_runs_total.fetch_add(1, Ordering::Relaxed);
                metrics.agent_runs_failed.fetch_add(1, Ordering::Relaxed);
            }
        }

        // Wait for the converter task to translate the last AgentEvent
        // before we emit anything else, so checkpoint_post and
        // RunFinished are guaranteed to arrive last.
        let _ = converter_handle.await;

        if let (Some(cp), Some(turn)) = (checkpoint_id, finished_turn) {
            let _ = sse_tx.send(ag::Event::Custom(ag::Custom {
                name: "agui-tui/checkpoint_post".into(),
                value: serde_json::json!({
                    "turn": turn,
                    "postId": cp,
                }),
                base: ag::BaseEvent::default(),
            }));
        }

        let _ = sse_tx.send(ag::Event::RunFinished(ag::RunFinished {
            thread_id: drv_thread,
            run_id: drv_run,
            result: None,
            base: ag::BaseEvent::default(),
        }));
    });

    let stream = tokio_stream::wrappers::UnboundedReceiverStream::new(sse_rx).map(|ev| {
        let data = serde_json::to_string(&ev).unwrap_or_else(|_| "{}".into());
        Ok::<_, Infallible>(Event::default().data(data))
    });

    Ok(Sse::new(stream))
}

/// Map an arbitrary AG-UI thread id onto a checkpoint session id that
/// satisfies `validate_session_id` in the checkpoint module
/// (alphanumerics + `-` `_` `.`, no leading dot, no `..`, no path
/// separators). Disallowed chars become `-`.
fn sanitize_thread_id_for_session(thread: &str) -> String {
    let mut out: String = thread
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    // Drop a leading dot so we don't produce a hidden dir.
    while out.starts_with('.') {
        out.replace_range(..1, "-");
    }
    // Collapse `..` so we don't produce ref-traversal sequences.
    while out.contains("..") {
        out = out.replace("..", "-.");
    }
    if out.is_empty() {
        out.push_str("default");
    }
    out
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

    #[test]
    fn agui_events_for_assistant_text_emits_start_content_end() {
        use agui_protocol as ag;
        let ev = AgentEvent::AssistantText {
            text: "hi".into(),
            step: 0,
        };
        let out = agui_events_for(&ev);
        assert_eq!(out.len(), 3, "got {out:?}");
        assert!(matches!(out[0], ag::Event::TextMessageStart(_)));
        assert!(matches!(out[1], ag::Event::TextMessageContent(_)));
        assert!(matches!(out[2], ag::Event::TextMessageEnd(_)));
    }

    // ── SDK Phase B: tool_progress forwarder ─────────────────────────────

    /// Verify that the stateful forwarder logic correctly emits ToolProgress
    /// after ToolResult with the right tool_name.  We simulate the forwarder's
    /// HashMap bookkeeping without spinning up a full Tokio task.
    #[test]
    fn tool_progress_emitted_after_tool_result() {
        use crate::event::AgentEvent;
        use std::collections::HashMap;
        use std::time::Instant;

        let mut tool_start_times: HashMap<String, Instant> = HashMap::new();
        let mut emitted: Vec<SseEvent> = Vec::new();

        // Simulate ToolCall arrival
        let call_event = AgentEvent::ToolCall {
            name: "run_shell".to_string(),
            id: "tc-1".to_string(),
            arguments: "{}".to_string(),
            step: 0,
        };
        if let AgentEvent::ToolCall { id, .. } = &call_event {
            tool_start_times.insert(id.clone(), Instant::now());
        }
        if let Some(ev) = map_agent_event(&call_event) {
            emitted.push(ev);
        }

        // Simulate ToolResult arrival (no sleep needed — elapsed_ms ≥ 0)
        let result_event = AgentEvent::ToolResult {
            id: "tc-1".to_string(),
            name: "run_shell".to_string(),
            output: "ok".to_string(),
            step: 0,
        };
        if let Some(ev) = map_agent_event(&result_event) {
            emitted.push(ev);
        }
        if let AgentEvent::ToolResult { id, name, .. } = &result_event {
            let elapsed_ms = tool_start_times
                .remove(id)
                .map(|start| start.elapsed().as_millis() as u64)
                .unwrap_or(0);
            emitted.push(SseEvent::ToolProgress {
                tool_use_id: id.clone(),
                tool_name: name.clone(),
                elapsed_ms,
            });
        }

        // Expect: ToolCall, ToolResult, ToolProgress
        assert_eq!(emitted.len(), 3, "expected 3 events");
        assert!(matches!(emitted[0], SseEvent::ToolCall { .. }));
        assert!(matches!(emitted[1], SseEvent::ToolResult { .. }));
        let SseEvent::ToolProgress {
            tool_use_id,
            tool_name,
            elapsed_ms,
        } = &emitted[2]
        else {
            panic!("third event should be ToolProgress");
        };
        assert_eq!(tool_use_id, "tc-1");
        assert_eq!(tool_name, "run_shell");
        let _ = elapsed_ms; // ≥ 0 is trivially true for u64
    }

    /// Verify that tool_start_times does NOT grow if a ToolResult arrives
    /// without a matching ToolCall (e.g. replayed events).
    #[test]
    fn tool_progress_elapsed_is_zero_for_unmatched_result() {
        use crate::event::AgentEvent;
        use std::collections::HashMap;
        use std::time::Instant;

        let mut tool_start_times: HashMap<String, Instant> = HashMap::new();

        let result_event = AgentEvent::ToolResult {
            id: "tc-orphan".to_string(),
            name: "read_file".to_string(),
            output: "data".to_string(),
            step: 0,
        };
        let elapsed_ms = if let AgentEvent::ToolResult { id, .. } = &result_event {
            tool_start_times
                .remove(id)
                .map(|start| start.elapsed().as_millis() as u64)
                .unwrap_or(0)
        } else {
            unreachable!()
        };
        // No panic; elapsed defaults to 0 when no matching ToolCall.
        assert_eq!(elapsed_ms, 0);
    }
}
