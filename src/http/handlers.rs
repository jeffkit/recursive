//! HTTP handler functions for the agent API.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::sse::{Event, Sse},
    Json,
};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::broadcast;
use tokio_stream::{wrappers::BroadcastStream, StreamExt};

use crate::agent::PlanningMode;
use crate::event::{AgentEvent, ChannelSink, NullSink};
use crate::permissions::{LayeredPermissionsConfig, PermissionMode};
use crate::runtime::AgentRuntimeBuilder;

use super::{
    build_openapi_spec, AppState, CreateSessionRequest, CreateSessionResponse, ErrorResponse,
    ListSessionsQuery, RunRequest, RunResponse, SessionDetailResponse, SessionInfo,
    SessionMessageRequest, SessionMessageResponse, SessionState, SetGoalRequest, SlashCommandInfo,
    SseContentBlock, SseEvent, ToolInfo, UsageInfo,
};

pub(super) async fn health() -> &'static str {
    "ok"
}

pub(super) async fn openapi_spec() -> Json<serde_json::Value> {
    Json(build_openapi_spec())
}

pub(super) async fn list_tools(State(state): State<Arc<AppState>>) -> Json<Vec<ToolInfo>> {
    Json(state.tools.clone())
}

pub(super) async fn run_agent(
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
    let system_prompt = match body.system_prompt {
        Some(s) => s,
        None => {
            let mut p = state.config.system_prompt.clone();
            if let Some(extra) = &body.append_system_prompt {
                p.push('\n');
                p.push_str(extra);
            }
            p
        }
    };
    let planning_mode = parse_planning_mode(body.planning_mode.as_deref());
    let mut tool_registry = state.tool_registry.clone();
    if let Some(mode_str) = body.permission_mode.as_deref() {
        let perm_mode = parse_permission_mode(mode_str);
        tool_registry = tool_registry.with_permissions(LayeredPermissionsConfig {
            mode: perm_mode,
            layers: Vec::new(),
        });
    }

    let mut runtime = AgentRuntimeBuilder::new()
        .llm(state.provider.clone())
        .tools(tool_registry)
        .system_prompt(system_prompt)
        .max_steps(max_steps)
        .planning_mode(planning_mode)
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

    let finish_reason = outcome.finish_reason.to_string();

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

// ── Request parsing helpers ────────────────────────────────────────────────

/// Parse `planning_mode` string from an API request body.
///
/// Accepted values (case-insensitive): `"immediate"` (default), `"plan_first"`.
/// Unknown values fall back to `Immediate`.
fn parse_planning_mode(s: Option<&str>) -> PlanningMode {
    match s {
        Some(v) if v.eq_ignore_ascii_case("plan_first") || v.eq_ignore_ascii_case("planfirst") => {
            PlanningMode::PlanFirst
        }
        _ => PlanningMode::Immediate,
    }
}

/// Parse `permission_mode` string from an API request body.
///
/// Accepted values (case-insensitive): `"default"`, `"auto"`, `"strict"`,
/// `"bypass"` / `"bypass_permissions"`. Unknown values fall back to `Default`.
fn parse_permission_mode(s: &str) -> PermissionMode {
    match s.to_ascii_lowercase().as_str() {
        "auto" => PermissionMode::Auto,
        "strict" => PermissionMode::Strict,
        "bypass" | "bypass_permissions" => PermissionMode::BypassPermissions,
        _ => PermissionMode::Default,
    }
}

// ── Session endpoints ──────────────────────────────────────────────────────

/// Generate a session ID using UUID v7 (time-ordered, globally unique).
fn generate_session_id() -> String {
    uuid::Uuid::now_v7().to_string()
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
pub(super) async fn create_session(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateSessionRequest>,
) -> Result<(StatusCode, Json<CreateSessionResponse>), (StatusCode, Json<ErrorResponse>)> {
    let id = generate_session_id();
    let created_at = format_timestamp(SystemTime::now());
    let system_prompt = match body.system_prompt {
        Some(s) => s,
        None => {
            let mut p = state.config.system_prompt.clone();
            if let Some(extra) = &body.append_system_prompt {
                p.push('\n');
                p.push_str(extra);
            }
            p
        }
    };
    let max_steps = body
        .max_steps
        .map(|n| n as usize)
        .unwrap_or(state.config.max_steps);
    let planning_mode = parse_planning_mode(body.planning_mode.as_deref());
    let mut tool_registry = state.tool_registry.clone();
    if let Some(mode_str) = body.permission_mode.as_deref() {
        let perm_mode = parse_permission_mode(mode_str);
        tool_registry = tool_registry.with_permissions(LayeredPermissionsConfig {
            mode: perm_mode,
            layers: Vec::new(),
        });
    }

    let mut runtime = AgentRuntimeBuilder::new()
        .llm(state.provider.clone())
        .tools(tool_registry)
        .system_prompt(system_prompt)
        .max_steps(max_steps)
        .planning_mode(planning_mode)
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

    // Register the session ID so all turns emit tracing spans with session_id
    // and transcript is auto-saved to the storage backend after each turn.
    runtime.set_session_id(&id);

    // Extract the gate before moving runtime into the Mutex so HTTP handlers
    // can approve/reject without acquiring the per-session runtime lock.
    let plan_approval_gate = runtime.plan_approval_gate();

    let session = SessionState {
        id: id.clone(),
        created_at: created_at.clone(),
        title: body.session_name,
        runtime: Arc::new(tokio::sync::Mutex::new(runtime)),
        plan_approval_gate,
        interrupt_token: Arc::new(tokio::sync::Mutex::new(None)),
        non_system_message_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };

    state.sessions.write().await.insert(id.clone(), session);

    Ok((
        StatusCode::CREATED,
        Json(CreateSessionResponse { id, created_at }),
    ))
}

/// GET /sessions — list all sessions, with optional `limit` and `offset` pagination.
///
/// Example: `GET /sessions?limit=10&offset=20`
pub(super) async fn list_sessions(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<ListSessionsQuery>,
) -> Json<Vec<SessionInfo>> {
    let sessions = state.sessions.read().await;
    let mut infos = Vec::with_capacity(sessions.len());
    for s in sessions.values() {
        // Read the pre-computed count without acquiring the runtime lock.
        // The count is updated atomically whenever a non-system message is
        // appended, so it remains accurate while a turn is in progress.
        let message_count = s
            .non_system_message_count
            .load(std::sync::atomic::Ordering::Relaxed);
        infos.push(SessionInfo {
            id: s.id.clone(),
            created_at: s.created_at.clone(),
            message_count,
            title: s.title.clone(),
        });
    }
    // Apply offset + limit pagination.
    let offset = params.offset.unwrap_or(0);
    let page: Vec<SessionInfo> = infos
        .into_iter()
        .skip(offset)
        .take(params.limit.unwrap_or(usize::MAX))
        .collect();
    Json(page)
}

/// GET /sessions/:id — get session detail with messages.
///
/// Reads plan-approval status directly from the session gate (no runtime lock
/// needed) so this endpoint stays responsive even while an agent turn is
/// blocked awaiting plan approval.  Messages and todos fall back to empty
/// vectors when the runtime is busy rather than deadlocking.
pub(super) async fn get_session(
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

    // Extract first/last user prompt for display without a separate lock.
    let (first_prompt, last_prompt) = {
        let user_msgs: Vec<String> = messages
            .iter()
            .filter_map(|m| {
                if m.get("role")?.as_str()? == "user" {
                    m.get("content")?.as_str().map(|s| s.to_string())
                } else {
                    None
                }
            })
            .collect();
        let first = user_msgs.first().cloned();
        let last = user_msgs.last().cloned();
        (first, last)
    };

    Ok(Json(SessionDetailResponse {
        id: session.id.clone(),
        created_at: session.created_at.clone(),
        title: session.title.clone(),
        messages,
        todos,
        status,
        pending_plan,
        goal,
        first_prompt,
        last_prompt,
    }))
}

/// DELETE /sessions/:id — remove a session.
pub(super) async fn delete_session(
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

// ── Session patch endpoint (rename) ──────────────────────────────────────

/// Request body for `PATCH /sessions/:id` — update mutable session fields.
#[derive(serde::Deserialize, Debug)]
pub(super) struct PatchSessionRequest {
    /// Optional new title for the session.
    title: Option<String>,
}

/// PATCH /sessions/:id — update mutable session metadata.
///
/// Currently supports setting/clearing the `title` field.
///
/// Example:
/// ```text
/// PATCH /sessions/abc123
/// {"title": "Fix login bug"}
/// ```
pub(super) async fn patch_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<PatchSessionRequest>,
) -> Result<Json<SessionInfo>, StatusCode> {
    let mut sessions = state.sessions.write().await;
    let session = sessions.get_mut(&id).ok_or(StatusCode::NOT_FOUND)?;

    if let Some(title) = body.title {
        session.title = if title.is_empty() { None } else { Some(title) };
    }

    // Build response without taking the runtime lock (message_count is approximate here).
    Ok(Json(SessionInfo {
        id: session.id.clone(),
        created_at: session.created_at.clone(),
        message_count: 0, // omitted in patch response; caller can re-fetch if needed
        title: session.title.clone(),
    }))
}

// ── Fork session ─────────────────────────────────────────────────────────

/// Response for `POST /sessions/:id/fork`.
#[derive(serde::Serialize)]
pub(super) struct ForkSessionResponse {
    /// ID of the newly created forked session.
    id: String,
    /// Timestamp when the fork was created.
    created_at: String,
    /// Number of messages copied from the source session.
    message_count: usize,
}

/// POST /sessions/:id/fork — fork a session, copying its transcript.
///
/// Creates a new session with the same transcript as the source session.
/// The forked session is independent: subsequent messages do not affect the
/// original.
///
/// Returns the new session's ID and metadata.
pub(super) async fn fork_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<ForkSessionResponse>), StatusCode> {
    // Snapshot the source transcript while holding the write lock.
    let transcript_snapshot = {
        let sessions = state.sessions.read().await;
        let src = sessions.get(&id).ok_or(StatusCode::NOT_FOUND)?;
        let rt = src.runtime.try_lock().map_err(|_| StatusCode::CONFLICT)?;
        rt.transcript().to_vec()
    };

    let message_count = transcript_snapshot.len();

    // Build a new session with the copied transcript.
    let new_id = generate_session_id();
    let created_at = format_timestamp(SystemTime::now());
    let system_prompt = state.config.system_prompt.clone();

    let mut runtime = AgentRuntimeBuilder::new()
        .llm(state.provider.clone())
        .tools(state.tool_registry.clone())
        .system_prompt(system_prompt)
        .max_steps(state.config.max_steps)
        .build()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let non_system_count = transcript_snapshot
        .iter()
        .filter(|m| m.role != crate::message::Role::System)
        .count();
    runtime.set_transcript(transcript_snapshot);

    let plan_approval_gate = runtime.plan_approval_gate();
    let session = SessionState {
        id: new_id.clone(),
        created_at: created_at.clone(),
        title: None,
        runtime: Arc::new(tokio::sync::Mutex::new(runtime)),
        plan_approval_gate,
        interrupt_token: Arc::new(tokio::sync::Mutex::new(None)),
        non_system_message_count: Arc::new(std::sync::atomic::AtomicUsize::new(non_system_count)),
    };

    state.sessions.write().await.insert(new_id.clone(), session);

    Ok((
        StatusCode::CREATED,
        Json(ForkSessionResponse {
            id: new_id,
            created_at,
            message_count,
        }),
    ))
}

// ── Plan-approval endpoints ───────────────────────────────────────────────

#[derive(serde::Deserialize)]
pub(super) struct PlanConfirmRequest {
    /// Optional replacement plan text to use instead of the agent-proposed one.
    edits: Option<String>,
}

#[derive(serde::Deserialize)]
pub(super) struct PlanRejectRequest {
    /// Reason shown to the agent so it can revise the plan.
    reason: Option<String>,
}

/// POST /sessions/:id/plan/confirm — approve the pending plan.
pub(super) async fn session_plan_confirm(
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
pub(super) async fn session_plan_reject(
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
pub(super) async fn session_set_goal(
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
pub(super) async fn session_clear_goal(
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
async fn runtime_goal_state_clear(runtime: &Arc<tokio::sync::Mutex<crate::runtime::AgentRuntime>>) {
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
pub(super) async fn session_interrupt(
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
pub(super) async fn list_slash_commands(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<SlashCommandInfo>> {
    Json((*state.slash_commands).clone())
}

/// POST /sessions/:id/messages — send a message in a session.
pub(super) async fn send_session_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<SessionMessageRequest>,
) -> Result<Json<SessionMessageResponse>, (StatusCode, Json<ErrorResponse>)> {
    // Get the session's runtime, interrupt token, and message counter (Arc clones are cheap).
    let (runtime_arc, interrupt_token_arc, msg_count_arc) = {
        let sessions = state.sessions.read().await;
        let session = sessions.get(&id).ok_or((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                status: "error".into(),
                error: "session not found".into(),
            }),
        ))?;
        (
            session.runtime.clone(),
            session.interrupt_token.clone(),
            session.non_system_message_count.clone(),
        )
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
        let mut tool_start_times: HashMap<String, std::time::Instant> = HashMap::new();
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

    // Run the agent turn via enqueue so the runtime's FIFO queue is used.
    let run_result = runtime.enqueue(&body.content).await.map(|opt| {
        opt.unwrap_or_else(|| crate::runtime::RuntimeOutcome {
            final_text: None,
            finish_reason: crate::agent::FinishReason::NoMoreToolCalls,
            total_usage: crate::TokenUsage::default(),
            steps: 0,
            llm_latency_ms: 0,
            checkpoint_id: None,
        })
    });

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

    // Update the lock-free message counter so list_sessions doesn't need to
    // acquire the runtime mutex.
    let new_count = runtime
        .transcript()
        .iter()
        .filter(|m| m.role != crate::message::Role::System)
        .count();
    msg_count_arc.store(new_count, std::sync::atomic::Ordering::Relaxed);

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
pub(super) async fn session_events(
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
        AgentEvent::ToolResult { name, is_error, .. } => {
            let success = !is_error;
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
pub(super) async fn agui_run(
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

/// GET /metrics — Prometheus exposition format.
pub(super) async fn metrics_handler(State(state): State<Arc<AppState>>) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::AgentEvent;
    use crate::http::SseEvent;

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
        use std::collections::HashMap;
        use std::time::Instant;

        let mut tool_start_times: HashMap<String, Instant> = HashMap::new();
        let mut emitted: Vec<SseEvent> = Vec::new();

        // Simulate ToolCall arrival
        let call_event = AgentEvent::ToolCall {
            name: "Bash".to_string(),
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
            name: "Bash".to_string(),
            output: "ok".to_string(),
            step: 0,
            is_error: false,
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
        assert_eq!(tool_name, "Bash");
        let _ = elapsed_ms; // ≥ 0 is trivially true for u64
    }

    /// Verify that tool_start_times does NOT grow if a ToolResult arrives
    /// without a matching ToolCall (e.g. replayed events).
    #[test]
    fn tool_progress_elapsed_is_zero_for_unmatched_result() {
        use std::collections::HashMap;
        use std::time::Instant;

        let mut tool_start_times: HashMap<String, Instant> = HashMap::new();

        let result_event = AgentEvent::ToolResult {
            id: "tc-orphan".to_string(),
            name: "Read".to_string(),
            output: "data".to_string(),
            step: 0,
            is_error: false,
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
