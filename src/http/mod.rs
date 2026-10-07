//! HTTP API server for the Recursive agent.
//!
//! Provides a lightweight axum-based HTTP server that exposes the agent's
//! tool registry as a read-only JSON endpoint, a health check, a POST /run
//! endpoint that executes the agent with a given goal, session management
//! endpoints for multi-turn conversations, and SSE streaming of agent events.

mod agui;
mod auth;
mod cold_load;
#[cfg(test)]
mod environment_binding_tests;
mod handlers;
mod metrics;
mod rate_limit;
mod session_mirror;
pub mod triggers;
mod usage;

// Goal 395: the admission gate moved to the transport-agnostic
// `session_host` module; re-exported here so front-end call sites and
// external `recursive::http::AdmissionGate` paths keep working.
pub use crate::session_host::SessionHost;
pub use crate::session_host::{AcquireError, AdmissionGate, RunPermit};
pub use auth::{
    AuthConfig, AuthIdentity, JwtConfig, DEFAULT_KEY_SUBJECT, ENV_AUTH_ADMINS, ENV_AUTH_JWT_SECRET,
    ENV_AUTH_KEYS, ENV_AUTH_KEY_OWNERS,
};
pub use handlers::map_agent_event;
// Issue #113: labelled counter / histogram families, the bounded finish-reason
// labels and the run-event metrics sink.
pub use metrics::{
    finish_reason_label, run_status_label, CounterFamily, HistogramFamily, HistogramSnapshot,
    MetricsSink, FINISH_REASON_ERROR,
};
pub use rate_limit::{rate_limiter_from_env, RateLimiter};
// Issue #114: per-session usage / cost accounting shared by the HTTP handlers
// and visible to SDK consumers constructing [`SessionState`] directly.
pub use usage::{SessionUsage, UsageResponse, UsageTotals};

use auth::{auth_config_from_env, auth_middleware};
use handlers::{
    agui_cancel, agui_run, create_session, delete_session, fork_session, get_session,
    get_session_usage, health, list_presets, list_sessions, list_skills, list_slash_commands,
    list_tools, metrics_handler, openapi_spec, patch_session, readyz, run_agent,
    send_session_message, session_clear_goal, session_events, session_interrupt,
    session_plan_confirm, session_plan_reject, session_set_goal,
};
use rate_limit::{metrics_middleware, rate_limit_middleware};

use triggers::{
    create_trigger, delete_trigger, fire_webhook, get_trigger, list_triggers, patch_trigger,
};

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
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

use crate::config::Config;
use crate::llm::ChatProvider;
use crate::runtime::AgentRuntime;
use crate::storage::StorageBackend;
use crate::tools::plan_mode::PlanApprovalGate;
use crate::tools::ToolRegistry;
use crate::Message;
use std::path::Path;

// ── Metrics ────────────────────────────────────────────────────────────────

/// Prometheus-compatible metrics collector.
///
/// Scalar gauges / totals stay `AtomicU64`; anything the exposition needs to
/// split by a dimension is a [`CounterFamily`] or [`HistogramFamily`]
/// (issue #113). `Default` is implemented by hand because the histograms need
/// their bucket ladders configured — `#[derive(Default)]` would silently build
/// them with no buckets at all.
pub struct Metrics {
    /// Issue #113: HTTP requests by **matched route template** (`route`) and
    /// response status (`status`). The route label is axum's `MatchedPath`
    /// (`/sessions/{id}/messages`), never the concrete path — a session id
    /// would make the cardinality unbounded. This is what makes a per-route
    /// 5xx / 503 (admission saturation) *rate* computable.
    pub requests_by_route: CounterFamily,
    /// Requests currently being served (gauge).
    pub requests_active: AtomicU64,
    pub agent_runs_total: AtomicU64,
    pub agent_runs_success: AtomicU64,
    pub agent_runs_failed: AtomicU64,
    pub tokens_prompt_total: AtomicU64,
    pub tokens_completion_total: AtomicU64,
    /// Issue #113: completed runs by terminal `finish_reason` (bounded
    /// vocabulary, see [`finish_reason_label`]). `agent_runs_success` counts
    /// every turn that returned an outcome; this family says *how* each one
    /// ended, so a `budget_exceeded` stop is visible instead of hiding inside
    /// "success".
    pub agent_runs_finished: CounterFamily,
    /// Issue #113: billed USD per `model`, stored in **micro-USD** (`1e-6
    /// USD`) because [`CounterFamily`] holds integers. Exposed (scaled) as
    /// `recursive_cost_usd_total{model="…"}`. Unpriced models contribute
    /// nothing — only runs priced by `crate::llm::pricing_for` move it.
    pub cost_micro_usd_by_model: CounterFamily,
    /// Issue #115: tokens burned by runs that ended in an error. A failed
    /// turn still spent its completed steps' tokens; before this counter the
    /// failure path recorded nothing, so quota/budget calibration was
    /// systematically optimistic.
    pub tokens_wasted_on_failure_total: AtomicU64,
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
    /// Issue #123: timestamp (ms since [`SESSION_EPOCH`]) of the most recent
    /// successful agent run — the cheapest honest proxy for "the LLM endpoint
    /// answered at least once". `0` = never succeeded (stamps are always
    /// non-zero — see `handlers::now_stamp_ms`). Exposed by `/readyz` and as
    /// `recursive_llm_last_success_ms` on `/metrics`.
    pub last_llm_success_ms: AtomicU64,
    /// Issue #123: failed **LLM** runs since the last success. Bumped only by
    /// `record_llm_failure` — a cancellation, tool or storage fault says
    /// nothing about the endpoint and must not take a healthy pod out of
    /// rotation. Cleared by `record_llm_success` and decayed by `/readyz`
    /// once the streak is older than `READYZ_LLM_FAILURE_WINDOW_MS` (a pod
    /// removed from its Service endpoints receives no runs, so nothing else
    /// could ever clear it). `/readyz` reports **not ready** while this is at
    /// or above `READYZ_MAX_LLM_FAILURES`, so a dead key / unreachable
    /// gateway stops taking traffic instead of returning 200 forever.
    pub llm_failures_consecutive: AtomicU64,
    /// Issue #123: timestamp (ms since [`SESSION_EPOCH`]) of the most recent
    /// failure that counted against [`Self::llm_failures_consecutive`]; `0` =
    /// none recorded. `/readyz` uses it to tell a live failure streak from a
    /// stale one that must no longer fail the probe.
    pub last_llm_failure_ms: AtomicU64,
    /// Issue #123: when `/readyz` last ran its storage round-trip probe, and
    /// what the probe said. Probe bookkeeping (not exposed on `/metrics`) kept
    /// next to the other readiness state so every `AppState` owns its own
    /// verdict — the endpoint is public, so an anonymous scraper must not be
    /// able to drive one storage write per request.
    pub readyz_storage_probed_ms: AtomicU64,
    /// See [`Self::readyz_storage_probed_ms`]. Only meaningful once
    /// `readyz_storage_probed_ms != 0`.
    pub readyz_storage_ok: AtomicBool,
    /// Issue #123: transcript/memory persistence rounds that failed — or were
    /// skipped with data loss (a session still busy at graceful shutdown).
    /// Previously each of these only emitted a `tracing` line, so a
    /// read-only/full backend silently dropped data.
    pub persist_failures: AtomicU64,
    /// Issue #123: sessions removed by the idle reaper (counter).
    pub sessions_evicted: AtomicU64,
    // ── Issue #113: histograms ─────────────────────────────────────────
    /// Per-`model` LLM latency of a completed run, fed from
    /// [`crate::runtime::RuntimeOutcome::llm_latency_ms`] — the runtime
    /// measured it all along, but it only ever reached the `/run` response
    /// body, never a Prometheus series.
    pub llm_latency_ms: HistogramFamily,
    /// Per-`finish_reason` step count of a completed run.
    pub run_steps: HistogramFamily,
    /// How long a request waited for an admission permit, by outcome
    /// (`admitted` / `timeout`). The queue depth gauges said *that* a pool was
    /// saturated; this says how long the latency actually was. `/agui` is
    /// excluded: its non-blocking acquire waits zero by construction.
    pub admission_wait_ms: HistogramFamily,
    // ── Issue #113: runtime-event counters (see [`MetricsSink`]) ───────
    /// Tool results that came back as errors, by tool name.
    pub tool_errors: CounterFamily,
    /// LLM retries by reason (`rate_limited` / `server_error` / `timeout` /
    /// `network` / `empty_body`) — a retry storm was previously invisible.
    pub llm_retries: CounterFamily,
    /// Compaction attempts by kind (`summary` / `micro` / `boundary`).
    pub compactions: CounterFamily,
    /// Compactions that were skipped, by reason (`circuit_breaker` / `error`).
    pub compaction_skipped: CounterFamily,
}

/// Upper bounds (milliseconds) for [`Metrics::llm_latency_ms`].
const LLM_LATENCY_MS_BUCKETS: &[u64] = &[
    25, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 30_000, 60_000, 120_000,
];
/// Upper bounds (steps) for [`Metrics::run_steps`].
const RUN_STEPS_BUCKETS: &[u64] = &[1, 2, 3, 5, 8, 13, 21, 34, 55, 100, 200];
/// Upper bounds (milliseconds) for [`Metrics::admission_wait_ms`]. The default
/// admission timeout is 30 s, so every bounded wait lands inside the ladder.
const ADMISSION_WAIT_MS_BUCKETS: &[u64] = &[
    1, 5, 10, 25, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 30_000,
];

impl Default for Metrics {
    fn default() -> Self {
        Self {
            requests_by_route: CounterFamily::default(),
            requests_active: AtomicU64::new(0),
            agent_runs_total: AtomicU64::new(0),
            agent_runs_success: AtomicU64::new(0),
            agent_runs_failed: AtomicU64::new(0),
            tokens_prompt_total: AtomicU64::new(0),
            tokens_completion_total: AtomicU64::new(0),
            agent_runs_finished: CounterFamily::default(),
            cost_micro_usd_by_model: CounterFamily::default(),
            tokens_wasted_on_failure_total: AtomicU64::new(0),
            agent_steps_total: AtomicU64::new(0),
            sessions_active: AtomicU64::new(0),
            rate_limits_rejected: AtomicU64::new(0),
            runs_waiting: Arc::new(AtomicU64::new(0)),
            runs_in_flight: Arc::new(AtomicU64::new(0)),
            last_llm_success_ms: AtomicU64::new(0),
            llm_failures_consecutive: AtomicU64::new(0),
            last_llm_failure_ms: AtomicU64::new(0),
            readyz_storage_probed_ms: AtomicU64::new(0),
            readyz_storage_ok: AtomicBool::new(false),
            persist_failures: AtomicU64::new(0),
            sessions_evicted: AtomicU64::new(0),
            llm_latency_ms: HistogramFamily::new(LLM_LATENCY_MS_BUCKETS),
            run_steps: HistogramFamily::new(RUN_STEPS_BUCKETS),
            admission_wait_ms: HistogramFamily::new(ADMISSION_WAIT_MS_BUCKETS),
            tool_errors: CounterFamily::default(),
            llm_retries: CounterFamily::default(),
            compactions: CounterFamily::default(),
            compaction_skipped: CounterFamily::default(),
        }
    }
}

impl Metrics {
    /// Issue #114 / #113: fold a completed run's USD cost into the per-model
    /// counter, using the same pricing the per-session accounting bills with.
    /// An unpriced model contributes nothing (`pricing_for` → `None` →
    /// $0.00), and the float → integer cast saturates, so a NaN / negative
    /// value (which the pricing tables never produce) cannot poison the total.
    pub fn record_cost_usd(&self, model: &str, usage: &crate::llm::TokenUsage) {
        let usd = crate::llm::pricing_for(model)
            .map(|p| p.cost_usd(*usage))
            .unwrap_or(0.0);
        let micro_usd = (usd * 1_000_000.0).round() as u64;
        self.cost_micro_usd_by_model.add(&[model], micro_usd);
    }

    /// Total billed USD across all completed runs (all models combined).
    pub fn cost_usd_total(&self) -> f64 {
        self.cost_micro_usd_by_model.total() as f64 / 1_000_000.0
    }
}

// ── Session types ──────────────────────────────────────────────────────────

/// Internal session state (not directly serialized to clients).
///
/// The [`AgentRuntime`] owns the transcript; this struct adds HTTP-layer
/// metadata (id, created_at) and the broadcast channel for SSE clients.
///
/// Clone is a handle clone: every mutable field is `Arc`-wrapped, so clones
/// share runtime / counters / gate with the value stored in the sessions
/// table (plain metadata fields — id / created_at / title / owner / tenant —
/// are copied).
/// `http::cold_load` relies on this to hand handlers a table entry without
/// holding the table lock.
#[derive(Clone)]
pub struct SessionState {
    pub id: String,
    pub created_at: String,
    /// Optional human-readable title, settable via `PATCH /sessions/:id`.
    pub title: Option<String>,
    /// Issue #85: the subject that created this session (JWT `sub` or an API
    /// key's configured subject). Every `/sessions/:id*` route asserts the
    /// caller's identity against it; `/sessions` filters on it.
    ///
    /// `None` is a session restored from metadata written before the identity
    /// model — it belongs to nobody, so only an admin identity may reach it.
    pub owner: Option<String>,
    /// Issue #85: the owner's tenant (JWT `tenant` claim), part of the
    /// ownership key — two tenants can mint the same `sub`.
    pub tenant: Option<String>,
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
    /// Issue #114: cumulative token usage + cost accounting for this session
    /// (all turns combined), updated lock-free at the end of every turn. The
    /// snapshot is persisted to the storage backends' key/value space after
    /// each turn, so it survives a restart and a cold load can restore it.
    pub usage: Arc<SessionUsage>,
    /// Issue #117: monotonic per-session counter shared with each turn's
    /// [`crate::event::EnvelopeSink`], so event `seq` numbers keep climbing
    /// across turns instead of restarting at 0 every turn.
    pub event_seq: Arc<AtomicU64>,
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

/// Per-session run overrides resolved from a request (issue #94).
///
/// These are provider/kernel settings rather than server-wide ones, so they
/// live on the session runtime the request creates instead of in
/// [`AppState::config`]. `Default` = "inherit the server's `Config`".
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionOverrides {
    /// Per-turn USD ceiling. `None` falls back to `Config::max_budget_usd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_budget_usd: Option<f64>,
    /// Anthropic extended-thinking budget. `None` falls back to
    /// `Config::thinking_budget`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_budget: Option<u32>,
}

impl SessionOverrides {
    /// True when neither override is set — used by `skip_serializing_if` so
    /// sessions that opt into nothing keep byte-identical stored metadata.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
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
    /// Anthropic claude-3-7). `0` disables thinking. Issue #94: this is a
    /// *provider* setting, so setting it builds a provider for this session
    /// with `thinking.budget_tokens = n` instead of reusing the server's.
    pub thinking_budget: Option<u32>,
    /// Permission mode: `"default"`, `"auto"`, `"strict"`, or `"bypass"`.
    pub permission_mode: Option<String>,
    /// Maximum API spend in USD **per turn** of this session (issue #94).
    /// The agent stops with `budget_exceeded` at the first step boundary
    /// where the turn's accumulated spend reaches this limit — it does not
    /// cap the session's total spend across turns. `None` / `0` = no limit.
    pub max_budget_usd: Option<f64>,
    /// Issue #127: agent preset this session runs under (default `standard`,
    /// or `RECURSIVE_AGENT_PRESET`). An unknown id is rejected with 400 — a
    /// preset is the session's runtime wiring, never a silent fallback.
    /// `GET /presets` lists what is available.
    #[serde(default)]
    pub preset: Option<String>,
}

/// Response body for `POST /sessions`.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct CreateSessionResponse {
    pub id: String,
    pub created_at: String,
}

/// One entry of `GET /presets` (issue #127): a session preset with its full
/// capability inventory — including the capabilities that are off by default —
/// and the context management it resolves to under the server's current
/// environment. This is what makes "the mechanism exists but nothing turns it
/// on" discoverable instead of folklore.
#[derive(serde::Serialize, Debug)]
pub struct PresetInfo {
    pub id: String,
    pub description: String,
    pub capabilities: Vec<crate::preset::Capability>,
    /// Issue #128 item 3: the system-prompt tokens a session created under
    /// this preset pays on every request (same `bytes/4` estimator the
    /// context breakdown uses — compare presets, don't bill with it).
    pub system_prompt_tokens: u32,
    pub resolved: crate::preset::ResolvedPreset,
}

/// Request body for `POST /sessions/:id/messages`.
#[derive(serde::Deserialize, Debug)]
pub struct SessionMessageRequest {
    pub content: String,
    /// Issue #105: optional outbound delivery of this turn's result.
    /// When set, the final assistant text is POSTed (webhook) or appended
    /// (file) after the turn completes — best-effort: delivery failure is
    /// logged and reported via the `notify_result` response field, never
    /// failing the turn.
    #[serde(default)]
    pub notify: Option<crate::notify::NotifyTarget>,
}

/// Response body for `POST /sessions/:id/messages`.
#[derive(serde::Serialize, Debug)]
pub struct SessionMessageResponse {
    pub role: String,
    pub content: String,
    /// Issue #105: delivery outcome when `notify` was requested
    /// (`"notified via webhook"` / an error description). Absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notify_result: Option<String>,
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
    /// Issue #98: the session's effective permission mode (`"default"` /
    /// `"auto"` / `"strict"` / `"bypass"`), read from the live tool registry.
    /// Absent while the session is busy (runtime lock held).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// Issue #127: the agent preset this session was assembled from, read off
    /// the live runtime (so a restored session reports the preset it was
    /// rebuilt with). Absent while the session is busy (runtime lock held).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
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
    /// keyed by `step` within a turn to reconstruct the eventual
    /// `Message::Text` block; use the enclosing [`SseFrame::id`] to order
    /// deltas across turns (issue #117), since `step` restarts every turn.
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
    /// SDK Phase B: a tool call just completed; `elapsed_ms` is the wall-clock
    /// time that tool itself spent executing, as measured by the runtime
    /// (issue #118). It excludes the permission-approval wait and any
    /// queueing behind other tools of the same step, and is forwarded as-is
    /// rather than re-derived from event arrival times.
    /// Emitted in addition to (and after) the `tool_result` event.
    ToolProgress {
        tool_use_id: String,
        tool_name: String,
        elapsed_ms: u64,
    },
    /// Issue #114: per-step provider-reported token usage, including the
    /// cache hit / miss split. Previously the underlying
    /// [`crate::event::AgentEvent::Usage`] had no SSE mapping and was
    /// silently dropped, so streaming clients could not see cost accumulate
    /// until the run ended. Emitted once per LLM step.
    Usage {
        input_tokens: u32,
        output_tokens: u32,
        /// Tokens served from the provider's prompt cache.
        cache_hit_tokens: u32,
        /// Tokens billed at the full input rate.
        cache_miss_tokens: u32,
        step: usize,
    },
}

/// One SSE frame: an [`SseEvent`] plus the `id:` that pins it to the session
/// timeline (issue #117).
///
/// The `id` is the [`crate::event::EventMeta::id`] of the originating
/// [`crate::event::AgentEvent`] — `<ts_ms>-<turn>-<seq>` — so a client can order/dedupe
/// frames (and echo it back via `Last-Event-ID`) without reconstructing a
/// timeline from the per-turn `step` numbers, which restart every turn.
/// Frames *derived* from one event (e.g. `tool_progress`) append a suffix —
/// `<ts_ms>-<turn>-<seq>:progress` — so they stay ordered next to their origin.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct SseFrame {
    /// The SSE `id:` field for this frame.
    pub id: String,
    pub event: SseEvent,
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
    /// Per-session SSE broadcast channels. Each entry carries an [`SseFrame`]
    /// so subscribers get the frame's timeline `id:` alongside the payload.
    pub event_channels: Arc<RwLock<HashMap<String, broadcast::Sender<SseFrame>>>>,
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
    /// startup by [`crate::storage::http_storage_backend`] — `S3StorageBackend`
    /// when `RECURSIVE_S3_BUCKET` is set and the `cloud-runtime` feature is
    /// compiled in (issue #92), otherwise `LocalStorageBackend` under the
    /// per-workspace user dir. Injected into every session runtime via
    /// `AgentRuntimeBuilder::storage`. The host layer calls
    /// `save_transcript` on session teardown only — DELETE, idle eviction,
    /// and graceful shutdown — never per turn (that would be an O(N²)
    /// full-transcript rewrite on the hot path).
    ///
    /// Goal 397 cold-load reads this same backend to restore sessions after a
    /// restart (`cold_load::get_or_load_session`).
    pub storage: Arc<dyn StorageBackend>,
    /// Issue #66: cancellation tokens for in-flight `/agui` runs, keyed by
    /// AG-UI thread id. `agui_run` inserts a fresh token before spawning its
    /// driver task and removes the entry on completion; the disconnect guard
    /// on the SSE body and `POST /agui/{thread_id}/cancel` both cancel it.
    /// AG-UI runs have no `SessionState` row, so they cannot reuse the
    /// per-session `interrupt_token` slot.
    pub agui_active_runs:
        Arc<std::sync::Mutex<HashMap<String, tokio_util::sync::CancellationToken>>>,
    /// Issue #121: root of the native session mirror
    /// (`<root>/<workspace-slug>/<session-id>/`), or `None` to disable
    /// mirroring. Resolved once at startup — the teardown paths
    /// ([`evict_idle_sessions`] / [`flush_all_sessions`]) must not re-resolve
    /// it from the environment, which can move under a running server (tests
    /// inject a tempdir or `None` instead of pinning process-global env).
    pub session_mirror_root: Option<std::path::PathBuf>,
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
            // The provider builds a fresh local registry, so re-attach the
            // already-spawned MCP tools (they hold process-wide clients) —
            // otherwise every `mcp__*` tool silently vanishes on the rebuild.
            for tool in base.mcp_tools() {
                reg.register_mut(tool);
            }
            if let Some(sp) = base.shared_permissions() {
                reg = reg.with_shared_permissions(sp);
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
    /// handle-clone of the shared startup registry (previous behaviour), with
    /// a per-session permission-hook slot (issue #134 review) so a
    /// request-scoped hook cannot escape into other sessions.
    /// Issue #31 §C: Result-shaped — container creation failure is a
    /// per-session error mapped by handlers to 503/500, not a process exit.
    ///
    /// Issue #65: the surface contracts (operator allow-list, coordinator
    /// pruning) are re-applied here because the container tier rebuilds the
    /// registry from scratch — filtering only the startup registry would
    /// silently hand rebuilt sessions the full toolset again. On the clone
    /// path this is an idempotent re-filter. MCP tools survive the rebuild:
    /// `rebind_per_session_registry` re-attaches the process-wide MCP tools
    /// to the fresh container registry before the surface filters run.
    pub async fn session_tool_registry(&self) -> Result<ToolRegistry, String> {
        // Issue #134 review: each session gets its OWN permission-hook slot.
        // On the clone path the registry would otherwise share the
        // process-wide base's slot, so a request-scoped hook — AG-UI installs
        // the client-tool / `interrupt_before` deny hook per run — would
        // outlive its run and silently deny those tool names in the base and
        // in every later session. Views of THIS session (e.g. `run_code`'s
        // invoker registry) still share the slot, so a late install reaches
        // them.
        let mut registry =
            rebind_per_session_registry(&self.tool_registry, &self.config, &self.skills)
                .await?
                .isolate_permission_hook();
        crate::coordinator::filter_registry(&mut registry);
        if !self.config.allow_tools.is_empty() {
            registry.retain_tools(&self.config.allow_tools);
        }
        Ok(registry)
    }
}

/// Minimal `Config` for tests that need a `Config` without a full
/// environment: any field the code under test reads must be overridden by the
/// caller. Every field of `Config` must be listed here, so a newly added field
/// breaks compilation instead of silently defaulting.
#[cfg(test)]
pub(crate) fn test_config_stub() -> crate::config::Config {
    crate::config::Config {
        workspace: std::path::PathBuf::from("."),
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
    /// Anthropic claude-3-7). `0` disables thinking. Issue #94: honoured by
    /// building a provider for this run with
    /// `thinking = {type: "enabled", budget_tokens: n}`.
    pub thinking_budget: Option<u32>,
    /// Permission mode: `"default"`, `"auto"`, `"strict"`, or `"bypass"`.
    pub permission_mode: Option<String>,
    /// Maximum API spend in USD for this run (issue #94). The run finishes
    /// with `finish_reason: "budget_exceeded"` at the first step boundary
    /// where the accumulated spend reaches this limit. `None` / `0` = no cap.
    pub max_budget_usd: Option<f64>,
}

/// Response from `POST /run` (issue #113 redefined `status`).
///
/// `status` is derived from `finish_reason`: `"success"` only when the model
/// answered (`no_more_tool_calls`), otherwise the terminal reason
/// (`"budget_exceeded"`, `"stuck"`, `"cancelled"`, `"wall_clock_exceeded"`, …)
/// — it used to be a hardcoded `"success"` for every termination. A finish
/// reason is data, not an error (invariant #7), so the HTTP status stays 200.
#[derive(serde::Serialize, Debug)]
pub struct RunResponse {
    pub status: String,
    pub finish_reason: String,
    pub messages: Vec<serde_json::Value>,
    pub usage: UsageInfo,
}

/// Token/step usage information for the one-shot `POST /run` response.
///
/// Issue #114: carries the full billing breakdown — the provider's cache hit
/// / miss split plus the computed USD — so a caller can bill the run without
/// a second request. `cost_usd` is `null` when the model has no pricing
/// entry (never a silent `0.0`).
#[derive(serde::Serialize, Debug)]
pub struct UsageInfo {
    pub total_steps: u32,
    pub total_tokens: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Input tokens served from the provider's prompt cache.
    pub cache_hit_tokens: u64,
    /// Input tokens billed at the full rate (`prompt = hit + miss`).
    pub cache_miss_tokens: u64,
    pub reasoning_tokens: u64,
    /// Measured LLM latency for the run, in milliseconds.
    pub llm_latency_ms: u64,
    /// Model the run was billed against.
    pub model: String,
    pub cost_usd: Option<f64>,
}

impl UsageInfo {
    /// Build the response block from a completed turn's outcome.
    pub fn from_turn(model: &str, outcome: &crate::runtime::RuntimeOutcome) -> Self {
        let totals = UsageTotals::from_token_usage(&outcome.total_usage);
        Self {
            total_steps: outcome.steps as u32,
            total_tokens: totals.total_tokens,
            prompt_tokens: totals.prompt_tokens,
            completion_tokens: totals.completion_tokens,
            cache_hit_tokens: totals.cache_hit_tokens,
            cache_miss_tokens: totals.cache_miss_tokens,
            reasoning_tokens: totals.reasoning_tokens,
            llm_latency_ms: outcome.llm_latency_ms,
            model: model.to_string(),
            cost_usd: usage::cost_usd(model, &totals),
        }
    }
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

    /// 401 Unauthorized with a message.
    pub(super) fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, message)
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
/// - `GET /health` / `GET /healthz` — liveness, returns `"ok"` (200)
/// - `GET /readyz` — readiness probe: storage write+read round-trip (verdict
///   cached, see `READYZ_PROBE_TTL_MS`), recent LLM failures and admission
///   saturation; `200` when ready, `503` otherwise (issue #123)
/// - `GET /tools` — returns JSON array of [`ToolInfo`]
/// - `POST /run` — runs the agent with a goal and returns the outcome
/// - `POST /sessions` — create a new session
/// - `GET /sessions` — list all sessions
/// - `GET /sessions/:id` — get session detail with messages
/// - `POST /sessions/:id/messages` — send a message in a session
/// - `GET /sessions/:id/usage` — cumulative token usage + USD cost (issue #114)
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
///
/// Connect info IS installed (`into_make_service_with_connect_info`): the
/// rate limiter's socket-IP fallback (#107) reads the peer address from the
/// `ConnectInfo<SocketAddr>` request extension, which only exists when the
/// serving make-service installs it. Plain `axum::serve(listener, router)`
/// silently omits it, collapsing every header-less request into the single
/// `ip:unknown` bucket.
pub async fn serve_with_graceful_shutdown(
    listener: tokio::net::TcpListener,
    router: Router,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
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
        // Issue #123: split liveness from readiness. `/health` stays the
        // backward-compatible liveness alias; `/healthz` is the k8s-native
        // spelling; `/readyz` probes the dependencies a "半死" server still
        // answers 200 for (storage writability, recent LLM failures, admission
        // saturation) and returns 503 when one is down. It stays unauthenticated
        // (a k8s probe carries no key), so its storage probe verdict is cached
        // (`READYZ_PROBE_TTL_MS`) instead of written once per request.
        .route("/healthz", get(health))
        .route("/readyz", get(readyz))
        .route("/openapi.json", get(openapi_spec))
        .route("/metrics", get(metrics_handler));

    // Protected sub-router: every other route goes through auth and
    // rate-limit. The rate-limit layer is the **outermost** of the
    // two so it runs first — every request is counted before auth does
    // any work. The limiter runs before authentication, so it cannot
    // validate the credential it keys on: a request carrying a non-empty
    // `x-api-key` is bucketed by that (hashed) header value, and rotating
    // it still mints fresh buckets. What cannot be bypassed is the
    // credential-less identity: with no `x-api-key` the bucket comes from
    // the socket IP (or the trusted-proxy XFF entry on the right), which
    // the client does not choose.
    let protected = Router::new()
        .route("/tools", get(list_tools))
        .route("/presets", get(list_presets))
        .route("/run", post(run_agent))
        .route("/triggers", post(create_trigger))
        .route("/triggers", get(list_triggers))
        .route("/triggers/{id}", get(get_trigger))
        .route("/triggers/{id}", axum::routing::patch(patch_trigger))
        .route("/triggers/{id}", axum::routing::delete(delete_trigger))
        .route("/webhooks/{id}", post(fire_webhook))
        .route("/sessions", post(create_session))
        .route("/sessions", get(list_sessions))
        .route("/sessions/{id}", get(get_session))
        .route("/sessions/{id}", axum::routing::delete(delete_session))
        .route("/sessions/{id}", axum::routing::patch(patch_session))
        .route("/sessions/{id}/messages", post(send_session_message))
        .route("/sessions/{id}/usage", get(get_session_usage))
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
        .route("/skills", get(list_skills))
        .route("/agui", post(agui_run))
        .route("/agui/{thread_id}/cancel", post(agui_cancel))
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
    let mut spec = serde_json::json!({
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
            "/healthz": {
                "get": {
                    "summary": "Liveness check",
                    "description": "Alias of /health for k8s liveness probes: returns 'ok' (200) whenever the process is up.",
                    "responses": {
                        "200": {
                            "description": "Process is alive",
                            "content": {
                                "text/plain": {
                                    "schema": { "type": "string", "example": "ok" }
                                }
                            }
                        }
                    }
                }
            },
            "/readyz": {
                "get": {
                    "summary": "Readiness check",
                    "description": "Probes the dependencies a half-dead server still answers 200 for: a storage write+read round-trip (verdict cached for ~5s, so scraping this public route cannot drive a write per request), a recent LLM failure streak, and admission-gate saturation. Returns 200 with a per-check JSON body when ready, 503 otherwise.",
                    "responses": {
                        "200": {
                            "description": "Server is ready to serve runs",
                            "content": {
                                "application/json": {
                                    "schema": { "type": "object" }
                                }
                            }
                        },
                        "503": {
                            "description": "One or more readiness checks failed",
                            "content": {
                                "application/json": {
                                    "schema": { "type": "object" }
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
            "/presets": {
                "get": {
                    "summary": "List agent presets",
                    "description": "Returns the built-in session presets (issue #127) with their capability inventory, each preset's `system_prompt_tokens` (the fixed per-request system cost, issue #128) and the context management each resolves to under the server's current environment. `POST /sessions` accepts a preset id in its `preset` field.",
                    "responses": {
                        "200": {
                            "description": "Array of preset descriptors",
                            "content": {
                                "application/json": {
                                    "schema": {
                                        "type": "array",
                                        "items": { "$ref": "#/components/schemas/PresetInfo" }
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
                    "description": "Remove a session. By default the Goal-396 \
                        semantics apply: the transcript is snapshotted to the \
                        storage backend and a tombstone prevents cold load from \
                        resurrecting the session. With `purge=true` every \
                        persisted copy is erased instead (transcript snapshot, \
                        tombstone, metadata and usage records, shadow-git \
                        checkpoint chain) — issue #102. \
                        Purge is idempotent and also works for sessions that \
                        are only present in storage (idle-evicted or deleted \
                        earlier without purge).",
                    "parameters": [{
                        "name": "id",
                        "in": "path",
                        "required": true,
                        "schema": { "type": "string" }
                    }, {
                        "name": "purge",
                        "in": "query",
                        "required": false,
                        "description": "Erase every persisted copy of the session \
                            (true delete) instead of keeping the transcript \
                            snapshot behind a tombstone.",
                        "schema": { "type": "boolean", "default": false }
                    }],
                    "responses": {
                        "204": { "description": "Session deleted (or already gone, for purge)" },
                        "400": { "description": "Malformed session id (path separators, \
                            `..`, or characters outside [A-Za-z0-9._-])" },
                        "404": { "description": "Session not found (non-purge deletes only)" }
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
                        "409": {
                            "description": "A run is already active for this session",
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
            "/sessions/{id}/usage": {
                "get": {
                    "summary": "Get session usage and cost",
                    "description": "Issue #114: cumulative token usage for the session \
                        (prompt / completion / cache hit / cache miss / reasoning split) \
                        and the USD cost billed from the same pricing the CLI and AG-UI \
                        channels use. Survives a server restart: the accumulator is \
                        persisted after every turn and restored on cold load. `cost_usd` \
                        is summed per turn at the model that ran it, so a restart onto a \
                        different model does not reprice history; `model` is the model new \
                        turns bill at.",
                    "parameters": [{
                        "name": "id",
                        "in": "path",
                        "required": true,
                        "schema": { "type": "string" }
                    }],
                    "responses": {
                        "200": {
                            "description": "Session usage and cost",
                            "content": {
                                "application/json": {
                                    "schema": { "$ref": "#/components/schemas/UsageResponse" }
                                }
                            }
                        },
                        "404": { "description": "Session not found" }
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
            "/agui/{thread_id}/cancel": {
                "post": {
                    "summary": "Cancel the in-flight AG-UI run for a thread",
                    "description": "Issue #66 §3.3: asks the run to stop. The kernel exits \
                        with FinishReason::Cancelled at the next step boundary or mid-LLM-call; \
                        the driver then persists the partial transcript and emits RunFinished \
                        with an Error outcome carrying code \"cancelled\". The SSE body also \
                        cancels its run automatically when the client disconnects. Idempotent: \
                        an unknown (or already-finished) thread answers 200 with \
                        \"cancelled\": false.",
                    "parameters": [{
                        "name": "thread_id",
                        "in": "path",
                        "required": true,
                        "schema": { "type": "string" }
                    }],
                    "responses": {
                        "200": {
                            "description": "Cancel requested (or nothing to cancel)",
                            "content": {
                                "application/json": {
                                    "schema": { "type": "object" }
                                }
                            }
                        }
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
                        by rate limiting). Added in G292, documented in G298. Issue #123 adds \
                        the capacity/data-loss series `recursive_sse_clients`, \
                        `recursive_agui_runs`, `recursive_persist_failures`, \
                        `recursive_sessions_evicted`, `recursive_llm_last_success_ms` and \
                        `recursive_llm_failures_consecutive`. \
                        Issue #113 adds the labelled series `recursive_requests_total` \
                        (`route` = matched route template, `status`), \
                        `recursive_agent_runs_finished_total` (`finish_reason`), \
                        `recursive_cost_usd_total` (`model`), `recursive_tool_errors_total` \
                        (`tool`), `recursive_llm_retries_total` (`reason`), \
                        `recursive_compactions_total` (`kind`) / \
                        `recursive_compaction_skipped_total` (`reason`), plus the histograms \
                        `recursive_llm_latency_ms` (`model`), `recursive_run_steps` \
                        (`finish_reason`) and `recursive_admission_wait_ms` (`result`).",
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
            },
            "/skills": {
                "get": {
                    "summary": "List loaded skills",
                    "description": "Returns every skill the server loaded: filesystem-discovered \
                        ones (`source: \"filesystem\"`) and service-level source-delivered ones \
                        (`source: \"content\"` — RECURSIVE_SKILL_SOURCE_URL, never written to disk).",
                    "responses": {
                        "200": {
                            "description": "Array of skill descriptors",
                            "content": {
                                "application/json": {
                                    "schema": {
                                        "type": "array",
                                        "items": { "$ref": "#/components/schemas/SkillInfo" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        },
        "components": {
            "schemas": {
                "SkillInfo": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "description": { "type": "string" },
                        "mode": {
                            "type": "string",
                            "enum": ["always", "trigger", "globs", "manual"]
                        },
                        "refs": { "type": "integer" },
                        "sections": { "type": "integer" },
                        "source": {
                            "type": "string",
                            "enum": ["content", "filesystem"],
                            "description": "content = service-level source, never on disk; filesystem = directory discovery."
                        }
                    },
                    "required": ["name", "description", "mode", "refs", "sections", "source"]
                },
                "ToolInfo": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "description": { "type": "string" },
                        "parameters": { "type": "object" }
                    },
                    "required": ["name", "description", "parameters"]
                },
                "PresetInfo": {
                    "type": "object",
                    "description": "An agent preset (issue #127): its capability inventory plus the context management it resolves to under the server's current environment.",
                    "properties": {
                        "id": { "type": "string" },
                        "description": { "type": "string" },
                        "capabilities": {
                            "type": "array",
                            "description": "Every capability the preset declares, including the ones that are off by default.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "name": { "type": "string" },
                                    "default": { "type": "string", "enum": ["enabled", "disabled"] },
                                    "toggle": { "type": "string" },
                                    "note": { "type": "string" }
                                },
                                "required": ["name", "default", "toggle", "note"]
                            }
                        },
                        "system_prompt_tokens": {
                            "type": "integer",
                            "description": "Issue #128: the system-prompt tokens this preset sends on every request (bytes/4 estimate — compare presets, don't bill with it)."
                        },
                        "resolved": { "type": "object" }
                    },
                    "required": ["id", "description", "capabilities", "system_prompt_tokens", "resolved"]
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
                        "status": {
                            "type": "string",
                            "description": "issue #113: derived from finish_reason — `success` only for `no_more_tool_calls`, otherwise the terminal reason (`budget_exceeded`, `stuck`, `cancelled`, `wall_clock_exceeded`, …)."
                        },
                        "finish_reason": { "type": "string" },
                        "messages": { "type": "array", "items": { "type": "object" } },
                        "usage": { "$ref": "#/components/schemas/UsageInfo" }
                    },
                    "required": ["status", "finish_reason", "messages", "usage"]
                },
                "UsageInfo": {
                    "type": "object",
                    "description": "Token/step usage for a POST /run response (issue #114 adds the cache split and USD cost).",
                    "properties": {
                        "total_steps": { "type": "integer" },
                        "total_tokens": { "type": "integer" },
                        "prompt_tokens": { "type": "integer" },
                        "completion_tokens": { "type": "integer" },
                        "cache_hit_tokens": { "type": "integer" },
                        "cache_miss_tokens": { "type": "integer" },
                        "reasoning_tokens": { "type": "integer" },
                        "llm_latency_ms": { "type": "integer" },
                        "model": { "type": "string" },
                        "cost_usd": { "type": "number", "nullable": true }
                    },
                    "required": ["total_steps", "total_tokens"]
                },
                "UsageResponse": {
                    "type": "object",
                    "description": "GET /sessions/:id/usage (issue #114): cumulative token usage with the cache split plus the USD cost. `cost_usd` is null while nothing billable has accrued and `model` has no pricing entry; `model` is the model new turns bill at.",
                    "properties": {
                        "session_id": { "type": "string" },
                        "model": { "type": "string", "description": "Model this session's new turns are billed at." },
                        "prompt_tokens": { "type": "integer" },
                        "completion_tokens": { "type": "integer" },
                        "cache_hit_tokens": { "type": "integer" },
                        "cache_miss_tokens": { "type": "integer" },
                        "reasoning_tokens": { "type": "integer" },
                        "total_tokens": { "type": "integer" },
                        "llm_latency_ms": { "type": "integer" },
                        "cost_usd": { "type": "number", "nullable": true }
                    },
                    "required": [
                        "session_id", "model", "prompt_tokens", "completion_tokens",
                        "cache_hit_tokens", "cache_miss_tokens", "total_tokens"
                    ]
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
                        "max_budget_usd": { "type": "number", "nullable": true },
                        "preset": { "type": "string", "nullable": true, "description": "issue #127: agent preset id; unknown ids are rejected with 400. See GET /presets." }
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
                        },
                        "permission_mode": {
                            "type": "string",
                            "nullable": true,
                            "description": "issue #98: effective permission mode (default | auto | strict | bypass); null while the session is busy"
                        },
                        "preset": {
                            "type": "string",
                            "nullable": true,
                            "description": "issue #127: agent preset this session was assembled from; null while the session is busy"
                        }
                    },
                    "required": ["id", "created_at", "messages", "status", "todos", "prompt_tokens", "completion_tokens"]
                },
                "SessionMessageRequest": {
                    "type": "object",
                    "properties": {
                        "content": { "type": "string" },
                        "notify": {
                            "$ref": "#/components/schemas/NotifyTarget",
                            "description": "Issue #105: deliver this turn's final text out-of-band; the outcome is reported in `notify_result` and a delivery failure never fails the turn."
                        }
                    },
                    "required": ["content"]
                },
                "SessionMessageResponse": {
                    "type": "object",
                    "properties": {
                        "role": { "type": "string" },
                        "content": { "type": "string" },
                        "notify_result": { "type": "string", "description": "Delivery outcome when `notify` was requested." }
                    },
                    "required": ["role", "content"]
                }
            }
        }
    });
    // Issue #105: merge the trigger/webhook endpoint paths and schemas
    // (authored in `triggers.rs` so the feature's spec stays beside the
    // handlers).
    if let Some(paths_obj) = spec.get_mut("paths").and_then(|p| p.as_object_mut()) {
        if let Some(trigger_paths) = triggers::trigger_openapi_paths().as_object_mut() {
            for (k, v) in trigger_paths {
                paths_obj.insert(k.clone(), v.clone());
            }
        }
    }
    if let Some(schemas_obj) = spec
        .pointer_mut("/components/schemas")
        .and_then(|s| s.as_object_mut())
    {
        for (k, v) in triggers::trigger_openapi_schemas() {
            schemas_obj.insert(k, v);
        }
    }
    if let Some(components) = spec.get_mut("components").and_then(|c| c.as_object_mut()) {
        components.insert(
            "parameters".to_string(),
            serde_json::json!({
                "TriggerId": {
                    "name": "id",
                    "in": "path",
                    "required": true,
                    "schema": { "type": "string" },
                    "description": "Trigger id (trig-xxxxxxxx)."
                }
            }),
        );
    }
    spec
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
            // Issue #102: drop transcripts older than the configured
            // retention window. Runs after eviction so a session evicted in
            // this tick is only eligible once it is genuinely old.
            if let Some(max_age) = session_retention_from_env() {
                purge_expired_transcripts(&state, max_age).await;
            }
            // Prune idle rate-limit buckets (goal-290). Runs every
            // reaper tick so the bucket map doesn't grow unboundedly.
            state.rate_limiter.prune().await;
        }
    })
}

/// Issue #121: mirror a closing HTTP session into the native session layout so
/// the CLI (`recursive sessions list`, the resume picker) sees it — the same
/// treatment #57 gave AG-UI threads ([`crate::agui_session`]). See
/// [`session_mirror`]; the flat `StorageBackend` transcript stays authoritative
/// for cold load, so this is best-effort.
///
/// Both call sites pass [`crate::session::SessionStatus::Completed`]: the
/// mirror only runs after the runtime was closed, so a mirrored session is
/// never in flight.
fn mirror_closing_session(
    mirror_root: Option<&Path>,
    workspace: &Path,
    model: &str,
    provider: &str,
    session: &SessionState,
    transcript: &[Message],
    status: crate::session::SessionStatus,
) {
    // Issue #121: `None` = mirroring disabled for this server (see
    // `AppState::session_mirror_root`).
    let Some(root) = mirror_root else {
        return;
    };
    let totals = session.usage.snapshot();
    let prompt = totals.prompt_tokens;
    let completion = totals.completion_tokens;
    let cost = if prompt == 0 && completion == 0 {
        None
    } else {
        Some(crate::session::SessionCost {
            total_input_tokens: prompt,
            total_output_tokens: completion,
            ..Default::default()
        })
    };
    session_mirror::mirror_session(
        root,
        &session_mirror::MirrorInput {
            workspace,
            id: &session.id,
            created_at: &session.created_at,
            transcript,
            model,
            provider,
            preset: None,
            name: session.title.as_deref(),
            cost,
            status,
        },
    );
}

/// Per-workspace transcript retention window (issue #102).
///
/// `RECURSIVE_SESSION_RETENTION_DAYS` bounds how long a persisted transcript
/// may sit on disk. Unset, `0`, or unparseable disables retention — opt-in, so
/// an existing deployment never starts deleting data on upgrade.
///
/// The "owner" of a transcript here is its **workspace**: this build has no
/// user/account notion, and every session under one workspace's data dir
/// belongs to the same operator, so a per-workspace window is the finest
/// scope expressible today.
pub(super) fn session_retention_from_env() -> Option<std::time::Duration> {
    let days = std::env::var("RECURSIVE_SESSION_RETENTION_DAYS")
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    (days > 0).then(|| std::time::Duration::from_secs(days.saturating_mul(86_400)))
}

/// One retention sweep: ask the storage backend to drop sessions older than
/// `max_age`. Returns the number of session records removed.
///
/// The window is a parameter rather than an env read so the sweep is testable
/// without racing process-global env; the reaper supplies
/// [`session_retention_from_env`]. Live sessions are passed as `keep` so the
/// sweep can never reap the only copy a crash could recover (see
/// [`StorageBackend::purge_expired_sessions`]). Backends that don't implement
/// enumeration (object stores enforce retention through a lifecycle policy)
/// answer `Ok(0)` — the trait default.
pub(super) async fn purge_expired_transcripts(
    state: &AppState,
    max_age: std::time::Duration,
) -> usize {
    let keep: HashSet<String> = state.host.sessions().read().await.keys().cloned().collect();
    match state.storage.purge_expired_sessions(max_age, &keep).await {
        Ok(0) => 0,
        Ok(n) => {
            tracing::info!(removed = n, "retention: purged expired session records");
            n
        }
        Err(e) => {
            tracing::warn!(error = %e, "retention sweep failed");
            0
        }
    }
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
                let metrics = state.metrics.clone();
                let workspace = state.config.workspace.clone();
                let model = state.config.model.clone();
                let provider = state.config.provider_type.clone();
                let mirror_root = state.session_mirror_root.clone();
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
                            // Issue #123: this is real data loss, so it is
                            // counted (not just logged) — one reaper sweep
                            // that cannot write must be visible to
                            // operators, not swallowed.
                            metrics.persist_failures.fetch_add(1, Ordering::Relaxed);
                            tracing::warn!(
                                session_id = %session.id,
                                error = %e,
                                "reaper: failed to persist session transcript"
                            );
                        }
                        // Issue #121: also mirror into the native session
                        // layout so the CLI (`sessions list` / resume) sees
                        // this HTTP session.
                        mirror_closing_session(
                            mirror_root.as_deref(),
                            &workspace,
                            &model,
                            &provider,
                            &session,
                            &transcript,
                            crate::session::SessionStatus::Completed,
                        );
                    }
                }
            },
            |id| {
                state
                    .metrics
                    .sessions_active
                    .fetch_sub(1, Ordering::Relaxed);
                state
                    .metrics
                    .sessions_evicted
                    .fetch_add(1, Ordering::Relaxed);
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
                Err(e) => {
                    state
                        .metrics
                        .persist_failures
                        .fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        session_id = %session.id,
                        error = %e,
                        "shutdown: failed to persist session transcript"
                    );
                }
            }
            // Issue #121: mirror into the native session layout too.
            mirror_closing_session(
                state.session_mirror_root.as_deref(),
                &state.config.workspace,
                &state.config.model,
                &state.config.provider_type,
                &session,
                &transcript,
                crate::session::SessionStatus::Completed,
            );
        } else {
            // Issue #123: a session still mid-turn at shutdown loses
            // everything since its last teardown save. That is data loss,
            // so it is an `error` (not a `warn`) and it is counted — a
            // clean shutdown that quietly drops an in-flight transcript
            // was previously invisible in `/metrics`.
            state
                .metrics
                .persist_failures
                .fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                session_id = %session.id,
                "shutdown: session still busy, transcript not persisted (data loss)"
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

// =====================================================================
// Issue #121 — the native session mirror is opt-in per `AppState`.
//
// `session_mirror_root: None` disables mirroring, which is what every test
// fixture uses (a teardown must never write into the developer's real
// session store, and pinning process-global env to say so perturbs
// unrelated tests in the same binary). That makes the *production* wiring
// load-bearing: this pins that the `recursive http` entry resolves the
// sessions root at startup, so a refactor cannot silently leave HTTP
// sessions invisible to `recursive sessions list`.
// =====================================================================
#[cfg(test)]
mod goal_121_session_mirror_wiring {
    #[test]
    fn http_entry_enables_the_native_session_mirror() {
        let src = include_str!("../../crates/recursive-cli/src/main.rs");
        assert!(
            src.contains("session_mirror_root: Some("),
            "the HTTP entry must resolve `user_sessions_dir` at startup and \
             set `session_mirror_root: Some(...)` — `None` disables the mirror \
             (issue #121)"
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
    #[derive(Default)]
    struct RecordingStorage {
        saves: std::sync::Mutex<Vec<SaveRecord>>,
        /// Retention windows passed to `purge_expired_sessions` (issue #102).
        purges: std::sync::Mutex<Vec<std::time::Duration>>,
        /// Live-session ids passed alongside each retention window.
        purge_keep: std::sync::Mutex<Vec<Vec<String>>>,
        /// What `purge_expired_sessions` reports as "removed".
        purge_result: std::sync::atomic::AtomicUsize,
        probe_sessions: Option<Arc<RwLock<SessionsMap>>>,
    }

    impl RecordingStorage {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        fn with_probe(sessions: Arc<RwLock<SessionsMap>>) -> Arc<Self> {
            Arc::new(Self {
                probe_sessions: Some(sessions),
                ..Default::default()
            })
        }

        fn saves(&self) -> Vec<SaveRecord> {
            self.saves.lock().unwrap().clone()
        }

        fn purges(&self) -> Vec<std::time::Duration> {
            self.purges.lock().unwrap().clone()
        }

        fn purge_keep(&self) -> Vec<Vec<String>> {
            self.purge_keep.lock().unwrap().clone()
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

        async fn delete_transcript(&self, _session_id: &str) -> crate::error::Result<()> {
            Ok(())
        }

        async fn load_memory(&self, _key: &str) -> crate::error::Result<Option<String>> {
            Ok(None)
        }

        async fn save_memory(&self, _key: &str, _value: &str) -> crate::error::Result<()> {
            Ok(())
        }

        async fn delete_memory(&self, _key: &str) -> crate::error::Result<()> {
            Ok(())
        }

        async fn purge_expired_sessions(
            &self,
            max_age: std::time::Duration,
            keep: &std::collections::HashSet<String>,
        ) -> crate::error::Result<usize> {
            self.purges.lock().unwrap().push(max_age);
            let mut keep: Vec<String> = keep.iter().cloned().collect();
            keep.sort();
            self.purge_keep.lock().unwrap().push(keep);
            Ok(self.purge_result.load(std::sync::atomic::Ordering::Relaxed))
        }
    }

    /// Issue #123: a backend whose every write fails — models a read-only or
    /// full disk so the data-loss counters can be pinned.
    struct FailingStorage;

    #[async_trait::async_trait]
    impl StorageBackend for FailingStorage {
        async fn load_transcript(&self, _session_id: &str) -> crate::error::Result<Vec<Message>> {
            Ok(vec![])
        }

        async fn save_transcript(
            &self,
            _session_id: &str,
            _messages: &[Message],
        ) -> crate::error::Result<()> {
            Err(crate::error::Error::Storage {
                message: "read-only filesystem".into(),
            })
        }

        async fn load_memory(&self, _key: &str) -> crate::error::Result<Option<String>> {
            Ok(None)
        }

        async fn save_memory(&self, _key: &str, _value: &str) -> crate::error::Result<()> {
            Err(crate::error::Error::Storage {
                message: "read-only filesystem".into(),
            })
        }
        async fn delete_transcript(&self, _session_id: &str) -> crate::error::Result<()> {
            Ok(())
        }
        async fn delete_memory(&self, _key: &str) -> crate::error::Result<()> {
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
            owner: None,
            tenant: None,
            runtime: Arc::new(tokio::sync::Mutex::new(runtime)),
            plan_approval_gate: Arc::new(crate::tools::plan_mode::PlanApprovalGate::new()),
            interrupt_token: Arc::new(tokio::sync::Mutex::new(None)),
            non_system_message_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            last_active_ms: Arc::new(AtomicU64::new(now_session_ms())),
            usage: Arc::new(SessionUsage::new("test-model")),
            event_seq: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Issue #121 (repaired after #114): the mirror prices the closing
    /// session from its accumulated `SessionUsage`, so a session with tokens
    /// still lands `meta.cost` — the per-counter atomics #114 replaced.
    #[test]
    fn mirror_closing_session_maps_session_usage_onto_meta_cost() {
        use crate::llm::TokenUsage;

        let root = tempfile::tempdir().unwrap();
        let session = test_session("sess-mirror-cost", 0);
        session.usage.record(
            TokenUsage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                ..Default::default()
            },
            0,
        );
        let transcript = vec![Message {
            role: Role::User,
            content: "hello".into(),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: None,
            is_compaction_summary: false,
        }];
        mirror_closing_session(
            Some(root.path()),
            std::path::Path::new("/tmp/ws"),
            "test-model",
            "openai",
            &session,
            &transcript,
            crate::session::SessionStatus::Completed,
        );

        let dir = root
            .path()
            .join(crate::session::workspace_slug(std::path::Path::new(
                "/tmp/ws",
            )))
            .join("sess-mirror-cost");
        let meta = crate::session::SessionReader::load_meta(&dir).expect("mirrored meta loads");
        let cost = meta
            .cost
            .expect("accumulated usage must reach the mirror's .meta.json cost");
        assert_eq!(cost.total_input_tokens, 10);
        assert_eq!(cost.total_output_tokens, 5);
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
        session_mirror_root: Option<PathBuf>,
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
            agui_active_runs: Arc::new(std::sync::Mutex::new(HashMap::new())),
            session_mirror_root,
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
            agui_active_runs: Arc::new(std::sync::Mutex::new(HashMap::new())),
            session_mirror_root: None,
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

    /// Issue #134 review: the server hands ONE process-wide registry to every
    /// session, and AG-UI installs a *request-scoped* permission hook on the
    /// registry it gets (`interrupt_before` / client tools). With a shared
    /// hook slot that hook outlives the run — denying its names in the base
    /// and in every later session. The slot must be per session, while views
    /// of one session still share it.
    #[tokio::test]
    async fn session_tool_registry_isolates_request_scoped_permission_hooks() {
        let state = test_state(test_host(0), RecordingStorage::new(), None).await;
        let base = state.tool_registry.clone();

        let mut first = state.session_tool_registry().await.expect("registry");
        first.set_permission_hook(Arc::new(crate::tools::registry::PermissionHookDisabled));
        assert!(first.permission_hook().is_some());

        assert!(
            base.permission_hook().is_none(),
            "a request-scoped hook must not leak into the process-wide base"
        );
        let second = state.session_tool_registry().await.expect("registry");
        assert!(
            second.permission_hook().is_none(),
            "a request-scoped hook must not leak into the next session"
        );

        // Views of ONE session (run_code's invoker registry) still share the
        // slot, in both directions.
        let mut view = first.clone();
        assert!(
            view.permission_hook().is_some(),
            "a view of the session must see the hook installed after it was taken"
        );
        view.clear_permission_hook();
        assert!(
            first.permission_hook().is_none(),
            "the session and its views must share the slot"
        );
    }

    #[tokio::test]
    async fn evict_persists_each_sessions_transcript_outside_the_lock() {
        let host = test_host(0);
        let sessions = host.sessions();
        let storage = RecordingStorage::with_probe(sessions.clone());
        // Mirroring is off here (issue #121): this test asserts persistence.
        let state = test_state(host, storage.clone(), None).await;

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
        // Issue #123: each eviction is counted; none of these saves failed.
        assert_eq!(
            state.metrics.sessions_evicted.load(Ordering::Relaxed),
            2,
            "every evicted session must bump sessions_evicted"
        );
        assert_eq!(
            state.metrics.persist_failures.load(Ordering::Relaxed),
            0,
            "successful persists must not count as failures"
        );
    }

    /// Issue #123: a reaper save that fails (read-only/full disk) is counted,
    /// not merely logged.
    #[tokio::test]
    async fn evict_counts_persist_failure_when_storage_write_fails() {
        let host = test_host(0);
        let storage: Arc<dyn StorageBackend> = Arc::new(FailingStorage);
        let state = test_state(host, storage, None).await;

        state
            .host
            .insert("s-loss".into(), test_session("s-loss", 1))
            .await;

        let evicted = evict_idle_sessions(&state).await;
        assert_eq!(evicted, vec!["s-loss"], "the idle session still evicts");
        assert_eq!(
            state.metrics.persist_failures.load(Ordering::Relaxed),
            1,
            "a failed teardown save must be counted"
        );
        assert_eq!(
            state.metrics.sessions_evicted.load(Ordering::Relaxed),
            1,
            "the eviction itself still counts"
        );
    }

    /// Issue #123: a session still mid-turn at graceful shutdown loses its
    /// transcript — that is data loss, so it must show up in the counter.
    #[tokio::test]
    async fn flush_counts_busy_session_as_data_loss() {
        let host = test_host(0);
        let storage: Arc<dyn StorageBackend> = RecordingStorage::new();
        let state = test_state(host, storage, None).await;

        state
            .host
            .insert("busy".into(), test_session("busy", 1))
            .await;
        // Clone the runtime handle so the read guard drops before the flush
        // takes its own write lock, then hold the runtime mutex to model an
        // in-flight turn.
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

        let persisted = flush_all_sessions(&state).await;
        assert_eq!(persisted, 0, "a busy session cannot be persisted");
        assert_eq!(
            state.metrics.persist_failures.load(Ordering::Relaxed),
            1,
            "a busy-on-shutdown session must be counted as data loss"
        );
    }

    #[tokio::test]
    async fn evict_skips_busy_session_in_place_without_persistence() {
        let host = test_host(0);
        let storage = RecordingStorage::new();
        let state = test_state(host, storage.clone(), None).await;

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
        // Issue #121: the shutdown flush also mirrors into the native session
        // layout. The root is injected — never read from the environment — so
        // the mirror can only land under this tempdir.
        let mirror_root = tempfile::tempdir().expect("mirror root");
        let host = test_host(0);
        let sessions = host.sessions();
        let storage = RecordingStorage::with_probe(sessions.clone());
        let state = test_state(
            host,
            storage.clone(),
            Some(mirror_root.path().to_path_buf()),
        )
        .await;

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
        assert_eq!(
            state.metrics.persist_failures.load(Ordering::Relaxed),
            0,
            "a clean shutdown must not report data loss"
        );

        // Issue #121: the flush mirrored `f-1` under the injected root, in the
        // shape `recursive sessions list` / resume reads.
        let mirrored = mirror_root
            .path()
            .join(crate::session::workspace_slug(&state.config.workspace))
            .join("f-1");
        let meta = crate::session::SessionReader::load_meta(&mirrored).expect("mirrored meta");
        assert_eq!(meta.session_id, "f-1");
        assert_eq!(meta.message_count, 2);
        assert_eq!(
            crate::session::SessionReader::load_full_history(&mirrored)
                .expect("mirrored transcript")
                .len(),
            2
        );
    }

    // ── Issue #102: retention window config + sweep wiring ────────────────

    #[test]
    fn retention_window_is_opt_in_and_days_based() {
        // Env is process-global: hold the lock so a concurrent test cannot
        // observe (or leak) a value mid-assertion.
        let _guard = crate::test_util::env_lock();
        std::env::remove_var("RECURSIVE_SESSION_RETENTION_DAYS");
        assert_eq!(session_retention_from_env(), None, "unset must be disabled");

        std::env::set_var("RECURSIVE_SESSION_RETENTION_DAYS", "0");
        assert_eq!(session_retention_from_env(), None, "0 must be disabled");

        std::env::set_var("RECURSIVE_SESSION_RETENTION_DAYS", "7");
        assert_eq!(
            session_retention_from_env(),
            Some(std::time::Duration::from_secs(7 * 86_400)),
            "days must be converted to a window"
        );

        std::env::set_var("RECURSIVE_SESSION_RETENTION_DAYS", " 30 ");
        assert_eq!(
            session_retention_from_env(),
            Some(std::time::Duration::from_secs(30 * 86_400)),
            "surrounding whitespace must be tolerated"
        );

        std::env::set_var("RECURSIVE_SESSION_RETENTION_DAYS", "forever");
        assert_eq!(
            session_retention_from_env(),
            None,
            "an unparseable value must fall back to disabled, never to 0 days"
        );

        std::env::remove_var("RECURSIVE_SESSION_RETENTION_DAYS");
    }

    #[tokio::test]
    async fn retention_sweep_passes_the_window_through_and_reports_removals() {
        let host = test_host(0);
        let storage = RecordingStorage::new();
        storage
            .purge_result
            .store(3, std::sync::atomic::Ordering::Relaxed);
        // A live session must be handed to the backend as "keep": its on-disk
        // snapshot is the only copy a crash could recover.
        host.sessions()
            .write()
            .await
            .insert("live".to_string(), test_session("live", 1));
        let state = test_state(host, storage.clone(), None).await;

        let window = std::time::Duration::from_secs(14 * 86_400);
        assert_eq!(
            purge_expired_transcripts(&state, window).await,
            3,
            "the backend's removed count must be reported to the caller"
        );
        assert_eq!(
            storage.purges(),
            vec![window],
            "the operator's window must reach the storage backend unchanged"
        );
        assert_eq!(
            storage.purge_keep(),
            vec![vec!["live".to_string()]],
            "live session ids must be excluded from the sweep"
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

    /// Issue #69: `RECURSIVE_ALLOW_TOOLS` must reach `config.allow_tools`
    /// even when only the env var is set. The CLI path is covered by clap's
    /// `env = "RECURSIVE_ALLOW_TOOLS"` injection; this pins the
    /// `Config::from_env` read that serves non-clap embedders (TUI preset
    /// config, HTTP session rebuilds).
    #[test]
    fn config_from_env_reads_allow_tools() {
        let src = include_str!("../config.rs");
        let from_env = src
            .split("pub fn from_env() -> Result<Self> {")
            .nth(1)
            .and_then(|rest| rest.split("pub fn ").next())
            .expect("Config::from_env must exist");
        assert!(
            from_env.contains("RECURSIVE_ALLOW_TOOLS"),
            "Config::from_env must read RECURSIVE_ALLOW_TOOLS so the operator \
             allow-list applies to non-clap embedders (issue #69)"
        );
    }

    #[test]
    fn session_rebind_reapplies_allow_tools_in_container_tier() {
        // Issue #69: the container tier rebuilds the registry per session.
        // Main's choke point for this is `session_tool_registry` (applies
        // coordinator pruning + the operator allow-list after every rebind);
        // `rebind_per_session_registry` itself must stay a pure rebuild.
        let src = include_str!("mod.rs").replace("\r\n", "\n");
        let rebind_block = src
            .split("async fn rebind_per_session_registry")
            .nth(1)
            .and_then(|rest| rest.split("impl AppState").next())
            .expect("rebind_per_session_registry must exist");
        assert!(
            !rebind_block.contains("retain_tools"),
            "rebind_per_session_registry is a rebuild helper — the allow-list \
             must stay in session_tool_registry, not duplicated here"
        );
        let session_block = src
            .split("pub async fn session_tool_registry")
            .nth(1)
            .and_then(|rest| rest.split("\n    }").next())
            .expect("session_tool_registry must exist");
        assert!(
            session_block.contains("retain_tools(&self.config.allow_tools)"),
            "session_tool_registry must reapply allow_tools narrowing to the \
             per-session (container-rebuilt) registry"
        );
    }

    /// Issue #104: the container tier rebuilds the registry from scratch, so
    /// it must re-attach the process-wide MCP tools — otherwise every
    /// `mcp__*` tool silently vanishes for container sessions.
    #[test]
    fn session_rebind_reattaches_mcp_tools() {
        let src = include_str!("mod.rs").replace("\r\n", "\n");
        let rebind_block = src
            .split("async fn rebind_per_session_registry")
            .nth(1)
            .and_then(|rest| rest.split("impl AppState").next())
            .expect("rebind_per_session_registry must exist");
        assert!(
            rebind_block.contains("base.mcp_tools()"),
            "container-tier registry rebuild must re-attach MCP tools via \
             base.mcp_tools() (issue #104)"
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
