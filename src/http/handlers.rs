//! HTTP handler functions for the agent API.

use async_trait::async_trait;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::sse::{Event, Sse},
    Json,
};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::broadcast;
use tokio_stream::{wrappers::BroadcastStream, wrappers::IntervalStream, StreamExt};

use crate::event::{AgentEvent, ChannelSink, NullSink};
use crate::message::Role;
use crate::permissions::{LayeredPermissionsConfig, PermissionMode};
use crate::runtime::AgentRuntimeBuilder;

use super::{
    build_openapi_spec, ApiError, AppState, CreateSessionRequest, CreateSessionResponse,
    ErrorResponse, ListSessionsQuery, RunRequest, RunResponse, SessionDetailResponse, SessionInfo,
    SessionMessageRequest, SessionMessageResponse, SessionState, SetGoalRequest, SlashCommandInfo,
    SseContentBlock, SseEvent, ToolInfo, UsageInfo,
};

pub(super) async fn health() -> &'static str {
    "ok"
}

/// Update metrics after a successful agent run.
fn record_run_success(metrics: &super::Metrics, steps: usize, usage: &crate::llm::TokenUsage) {
    metrics.agent_runs_total.fetch_add(1, Ordering::Relaxed);
    metrics.agent_runs_success.fetch_add(1, Ordering::Relaxed);
    metrics
        .agent_steps_total
        .fetch_add(steps as u64, Ordering::Relaxed);
    metrics
        .tokens_prompt_total
        .fetch_add(usage.prompt_tokens as u64, Ordering::Relaxed);
    metrics
        .tokens_completion_total
        .fetch_add(usage.completion_tokens as u64, Ordering::Relaxed);
}

/// Update metrics after a failed agent run.
fn record_run_failed(metrics: &super::Metrics) {
    metrics.agent_runs_total.fetch_add(1, Ordering::Relaxed);
    metrics.agent_runs_failed.fetch_add(1, Ordering::Relaxed);
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
) -> Result<Json<RunResponse>, ApiError> {
    // Validate: goal must not be empty
    if body.goal.trim().is_empty() {
        return Err(ApiError::bad_request("missing or empty 'goal' field"));
    }

    // Acquire a semaphore permit to limit concurrent runs.
    let _permit = state
        .run_semaphore
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "too many concurrent runs, try again later",
            )
        })?;
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
    // Common system-prompt assembly: project context (AGENTS.md + CLAUDE.md)
    // + base + skill index + coordinator/sub_agent note (when enabled).
    let system_prompt = crate::assemble_system_prompt(
        &system_prompt,
        &state.config.workspace,
        &state.skills,
        state.config.subagent_enabled,
    );
    let mut tool_registry = state.tool_registry.clone();
    if let Some(mode_str) = body.permission_mode.as_deref() {
        let perm_mode = parse_permission_mode(mode_str, state.config.allow_bypass_permissions);
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
        .build()
        .map_err(|e| ApiError::internal(format!("failed to build runtime: {e}")))?;

    let outcome = runtime.run(&body.goal).await.map_err(|e| {
        record_run_failed(&state.metrics);
        ApiError::internal(format!("agent run failed: {e}"))
    })?;

    record_run_success(&state.metrics, outcome.steps, &outcome.total_usage);

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

/// Parse `permission_mode` string from an API request body.
///
/// Accepted values (case-insensitive): `"default"`, `"auto"`, `"strict"`,
/// `"bypass"` / `"bypass_permissions"`. Unknown values fall back to `Default`.
fn parse_permission_mode(s: &str, allow_bypass: bool) -> PermissionMode {
    match s.to_ascii_lowercase().as_str() {
        "auto" => PermissionMode::Auto,
        "strict" => PermissionMode::Strict,
        "bypass" | "bypass_permissions" if allow_bypass => PermissionMode::BypassPermissions,
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

    // Days since 1970-01-01 — delegate to the O(1) civil-calendar impl in session.rs
    let (year, month, day) = crate::session::epoch_day_to_ymd(days as i64);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hours, minutes, seconds
    )
}

/// POST /sessions — create a new session.
pub(super) async fn create_session(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateSessionRequest>,
) -> Result<(StatusCode, Json<CreateSessionResponse>), ApiError> {
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
    // Common system-prompt assembly: project context (AGENTS.md + CLAUDE.md)
    // + base + skill index + coordinator/sub_agent note (when enabled).
    let system_prompt = crate::assemble_system_prompt(
        &system_prompt,
        &state.config.workspace,
        &state.skills,
        state.config.subagent_enabled,
    );
    let max_steps = body
        .max_steps
        .map(|n| n as usize)
        .unwrap_or(state.config.max_steps);
    let mut tool_registry = state.tool_registry.clone();
    if let Some(mode_str) = body.permission_mode.as_deref() {
        let perm_mode = parse_permission_mode(mode_str, state.config.allow_bypass_permissions);
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
        .build()
        .map_err(|e| ApiError::internal(format!("failed to build session runtime: {e}")))?;

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
        last_active_ms: Arc::new(AtomicU64::new(super::now_session_ms())),
        prompt_tokens: Arc::new(AtomicU64::new(0)),
        completion_tokens: Arc::new(AtomicU64::new(0)),
    };

    state.sessions.write().await.insert(id.clone(), session);
    state
        .metrics
        .sessions_active
        .fetch_add(1, Ordering::Relaxed);
    tracing::info!(session_id = %id, "session created");
    Ok((
        StatusCode::CREATED,
        Json(CreateSessionResponse { id, created_at }),
    ))
}

/// Response envelope for `GET /sessions`.
///
/// Wraps the paginated list of [`SessionInfo`] with a `total` count
/// representing the **un-paginated** number of sessions known to the
/// server. Clients use `total` to render "page X of Y" / scrollbars
/// without having to fetch every page just to count sessions.
#[derive(serde::Serialize)]
pub(super) struct SessionList {
    pub total: usize,
    pub sessions: Vec<SessionInfo>,
}

/// GET /sessions — list all sessions, with optional `limit` and `offset` pagination.
///
/// Example: `GET /sessions?limit=10&offset=20`
///
/// Returns a [`SessionList`] envelope (`{ "total": N, "sessions": [...] }`)
/// so paginated UIs can render total counts without fetching every page.
pub(super) async fn list_sessions(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<ListSessionsQuery>,
) -> Json<SessionList> {
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
    // Sort by creation time (ISO 8601 lexicographic = chronological) so clients
    // receive sessions in a predictable, meaningful order. Use `id` as a secondary
    // key to break ties between sessions created in the same second.
    infos.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    // `total` is the count BEFORE pagination so clients can compute total pages.
    let total = infos.len();
    // Apply offset + limit pagination.
    let offset = params.offset.unwrap_or(0);
    let page: Vec<SessionInfo> = infos
        .into_iter()
        .skip(offset)
        .take(params.limit.unwrap_or(usize::MAX))
        .collect();
    Json(SessionList {
        total,
        sessions: page,
    })
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
) -> Result<Json<SessionDetailResponse>, ApiError> {
    let sessions = state.sessions.read().await;
    let session = sessions
        .get(&id)
        .ok_or_else(|| ApiError::not_found("session not found"))?;

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

    // Read token usage directly from atomic counters — no lock needed.
    let prompt_tokens = session.prompt_tokens.load(Ordering::Relaxed);
    let completion_tokens = session.completion_tokens.load(Ordering::Relaxed);

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
        prompt_tokens,
        completion_tokens,
    }))
}

/// DELETE /sessions/:id — remove a session.
pub(super) async fn delete_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    // Look up the runtime under a read lock so we can take the per-session
    // runtime Mutex and call `close()` without holding the global write
    // lock across an await point.
    let session_runtime = {
        let sessions = state.sessions.read().await;
        sessions.get(&id).map(|s| s.runtime.clone())
    };
    if let Some(runtime) = session_runtime {
        // Fire SessionEnd (no outcome — the client is deleting the session
        // without a terminating turn) and flip `session_closed` before the
        // runtime is dropped. Idempotent on repeated calls.
        let mut rt = runtime.lock().await;
        rt.close(None).await;
        drop(rt);
        state.sessions.write().await.remove(&id);
        state
            .metrics
            .sessions_active
            .fetch_sub(1, Ordering::Relaxed);
        // Clean up SSE event channel for this session.
        state.event_channels.write().await.remove(&id);
        tracing::info!(session_id = %id, "session deleted");
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("session not found"))
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
) -> Result<Json<SessionInfo>, ApiError> {
    let mut sessions = state.sessions.write().await;
    let session = sessions
        .get_mut(&id)
        .ok_or_else(|| ApiError::not_found("session not found"))?;

    if let Some(title) = body.title {
        session.title = if title.is_empty() { None } else { Some(title) };
    }

    // Read the pre-computed non-system message count directly from the
    // atomic. It is updated whenever a non-system message is appended, so
    // we don't need to acquire the runtime lock here.
    Ok(Json(SessionInfo {
        id: session.id.clone(),
        created_at: session.created_at.clone(),
        message_count: session.non_system_message_count.load(Ordering::Relaxed),
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
    /// Number of non-system messages copied from the source session
    /// (matches the semantics of `SessionInfo.message_count` from
    /// `GET /sessions`).
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
) -> Result<(StatusCode, Json<ForkSessionResponse>), ApiError> {
    // Snapshot the source transcript while holding the write lock.
    let transcript_snapshot = {
        let sessions = state.sessions.read().await;
        let src = sessions
            .get(&id)
            .ok_or_else(|| ApiError::not_found("session not found"))?;
        let rt = src
            .runtime
            .try_lock()
            .map_err(|_| ApiError::conflict("session is busy"))?;
        rt.transcript().to_vec()
    };

    // Build a new session with the copied transcript.
    let new_id = generate_session_id();
    let created_at = format_timestamp(SystemTime::now());
    let system_prompt = state.config.system_prompt.clone();
    // Common system-prompt assembly so the forked session matches every
    // other channel (project context + skill index + sub-agent note).
    let system_prompt = crate::assemble_system_prompt(
        &system_prompt,
        &state.config.workspace,
        &state.skills,
        state.config.subagent_enabled,
    );

    let mut runtime = AgentRuntimeBuilder::new()
        .llm(state.provider.clone())
        .tools(state.tool_registry.clone())
        .system_prompt(system_prompt)
        .max_steps(state.config.max_steps)
        .build()
        .map_err(|_| ApiError::internal("failed to build forked session runtime"))?;

    // Count non-system messages BEFORE set_transcript (which moves the
    // snapshot). The new session's `non_system_message_count` atomic and
    // the fork response's `message_count` both use this number so they
    // agree with `SessionInfo.message_count` from `GET /sessions`.
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
        last_active_ms: Arc::new(AtomicU64::new(super::now_session_ms())),
        prompt_tokens: Arc::new(AtomicU64::new(0)),
        completion_tokens: Arc::new(AtomicU64::new(0)),
    };

    state.sessions.write().await.insert(new_id.clone(), session);
    state
        .metrics
        .sessions_active
        .fetch_add(1, Ordering::Relaxed);

    Ok((
        StatusCode::CREATED,
        Json(ForkSessionResponse {
            id: new_id,
            created_at,
            message_count: non_system_count,
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
) -> Result<Json<serde_json::Value>, ApiError> {
    let sessions = state.sessions.read().await;
    let session = sessions
        .get(&session_id)
        .ok_or_else(|| ApiError::not_found("session not found"))?;
    let pending = session
        .plan_approval_gate
        .pending_plan
        .read()
        .ok()
        .and_then(|g| g.clone());
    if pending.is_none() {
        return Err(ApiError::conflict("session is not awaiting plan approval"));
    }
    // Optionally replace the plan text before approving.
    if let Some(edited) = body.edits {
        if let Ok(mut w) = session.plan_approval_gate.pending_plan.write() {
            *w = Some(edited);
        }
    }
    session.plan_approval_gate.approve();
    Ok(Json(serde_json::json!({
        "status": "approved",
        "session_id": session_id
    })))
}

/// POST /sessions/:id/plan/reject — reject the pending plan.
pub(super) async fn session_plan_reject(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    Json(body): Json<PlanRejectRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let sessions = state.sessions.read().await;
    let session = sessions
        .get(&session_id)
        .ok_or_else(|| ApiError::not_found("session not found"))?;
    let pending = session
        .plan_approval_gate
        .pending_plan
        .read()
        .ok()
        .and_then(|g| g.clone());
    if pending.is_none() {
        return Err(ApiError::conflict("session is not awaiting plan approval"));
    }
    let reason = body.reason.unwrap_or_default();
    session.plan_approval_gate.reject(&reason);
    Ok(Json(serde_json::json!({
        "status": "rejected",
        "session_id": session_id
    })))
}

// ── Goal-168: goal endpoints ──────────────────────────────────────────────

/// POST /sessions/:id/goal — start a condition-based autonomous loop.
pub(super) async fn session_set_goal(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    Json(body): Json<SetGoalRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let runtime_arc = {
        let sessions = state.sessions.read().await;
        let session = sessions
            .get(&session_id)
            .ok_or_else(|| ApiError::not_found("session not found"))?;
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
            return Err(ApiError::conflict("session runtime is busy"));
        }
    }

    Ok(Json(serde_json::json!({
        "status": "pursuing",
        "session_id": session_id
    })))
}

/// DELETE /sessions/:id/goal — clear the active goal.
pub(super) async fn session_clear_goal(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let runtime_arc = {
        let sessions = state.sessions.read().await;
        let session = sessions
            .get(&session_id)
            .ok_or_else(|| ApiError::not_found("session not found"))?;
        session.runtime.clone()
    };

    let lock_result = runtime_arc.try_lock();

    match lock_result {
        Ok(runtime) => {
            runtime.clear_goal().await;
            drop(runtime);
            Ok(Json(serde_json::json!({
                "status": "cleared",
                "session_id": session_id
            })))
        }
        Err(_) => {
            // Runtime is busy with an in-flight turn; retry briefly.
            if runtime_goal_state_clear(&runtime_arc).await {
                return Ok(Json(serde_json::json!({
                    "status": "cleared",
                    "session_id": session_id
                })));
            }
            // Fold the original `hint` text into the error message so the
            // standardized `{"error": "..."}` envelope preserves it (Goal-313).
            // Attach `Retry-After: 5` via ApiError::with_retry_after so
            // clients that respect the hint can back off correctly.
            Err(ApiError::conflict(
                "session runtime is busy; goal not cleared — retry after the current turn completes",
            )
            .with_retry_after(5))
        }
    }
}

/// Force-clear goal state when the runtime Mutex is held.
///
/// Retries up to 10 times × 100ms (1s total). Returns `true` if the
/// goal was cleared, `false` if the runtime is still busy.
async fn runtime_goal_state_clear(
    runtime: &Arc<tokio::sync::Mutex<crate::runtime::AgentRuntime>>,
) -> bool {
    for _ in 0..10u8 {
        if let Ok(rt) = runtime.try_lock() {
            rt.clear_goal().await;
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    false
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
) -> Result<Json<serde_json::Value>, ApiError> {
    let token_arc = {
        let sessions = state.sessions.read().await;
        let session = sessions
            .get(&session_id)
            .ok_or_else(|| ApiError::not_found("session not found"))?;
        session.interrupt_token.clone()
    };

    // Cancel the current token if one is installed.
    let token_opt = token_arc.lock().await.clone();
    if let Some(token) = token_opt {
        token.cancel();
    }

    Ok(Json(serde_json::json!({
        "status": "interrupted",
        "session_id": session_id
    })))
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
) -> Result<Json<SessionMessageResponse>, ApiError> {
    if body.content.trim().is_empty() {
        return Err(ApiError::bad_request("missing or empty 'content' field"));
    }
    tracing::debug!(session_id = %id, content_len = body.content.len(), "session message received");
    // Get the session's runtime, interrupt token, message counter, last_active,
    // and token usage counters.
    let (runtime_arc, interrupt_token_arc, msg_count_arc, prompt_tokens_arc, completion_tokens_arc) = {
        let sessions = state.sessions.read().await;
        let session = sessions
            .get(&id)
            .ok_or_else(|| ApiError::not_found("session not found"))?;
        // Update last_active_ms timestamp for this session.
        session
            .last_active_ms
            .store(super::now_session_ms(), Ordering::Relaxed);
        (
            session.runtime.clone(),
            session.interrupt_token.clone(),
            session.non_system_message_count.clone(),
            session.prompt_tokens.clone(),
            session.completion_tokens.clone(),
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

    // Acquire a semaphore permit to limit concurrent runs.
    let _permit = state
        .run_semaphore
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "too many concurrent runs, try again later",
            )
        })?;
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
    // Goal 274: also maintain the non_system_message_count atomic so the
    // count stays correct even when the turn errors out mid-run.
    let initial_count = msg_count_arc.load(std::sync::atomic::Ordering::Relaxed);
    let count_arc = msg_count_arc.clone();
    let forward_handle = tokio::spawn(async move {
        let mut tool_start_times: HashMap<String, std::time::Instant> = HashMap::new();
        let mut count: usize = initial_count;
        while let Some(ref agent_event) = event_rx.recv().await {
            // Increment the count for every non-System message appended.
            match agent_event {
                AgentEvent::MessageAppended { message, .. }
                | AgentEvent::MessageAppendedWithAudit { message, .. }
                    if message.role != Role::System =>
                {
                    count += 1;
                    count_arc.store(count, std::sync::atomic::Ordering::Relaxed);
                }
                _ => {}
            }
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

    let outcome = run_result.map_err(|e| ApiError::internal(format!("agent run failed: {e}")))?;

    // Update per-session token counters and global metrics.
    prompt_tokens_arc.fetch_add(outcome.total_usage.prompt_tokens as u64, Ordering::Relaxed);
    completion_tokens_arc.fetch_add(
        outcome.total_usage.completion_tokens as u64,
        Ordering::Relaxed,
    );
    record_run_success(&state.metrics, outcome.steps, &outcome.total_usage);

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
) -> Result<Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    // Verify session exists
    {
        let sessions = state.sessions.read().await;
        if !sessions.contains_key(&id) {
            return Err(ApiError::not_found("session not found"));
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

    // Map real agent events to SSE data events, dropping lagged-receiver errors.
    let agent_stream = BroadcastStream::new(rx).filter_map(|result| match result {
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
            Some(Ok::<Event, Infallible>(
                Event::default().event(event_type).data(data),
            ))
        }
        Err(_) => None,
    });

    // Heartbeat: emit an SSE comment every 30 seconds so proxy/load-balancer
    // layers can detect the connection is still alive.
    let heartbeat_stream = IntervalStream::new(tokio::time::interval(Duration::from_secs(30)))
        .map(|_| Ok::<Event, Infallible>(Event::default().comment("heartbeat")));

    // Merge agent events and heartbeats into a single stream, capped at 1 hour.
    let combined = agent_stream
        .merge(heartbeat_stream)
        .timeout(Duration::from_secs(3600))
        .filter_map(|r| r.ok());

    Ok(Sse::new(combined))
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
            // Hook lifecycle events: forward as Custom so AG-UI clients can
            // render hook progress and system messages in real time.
            AgentEvent::HookStarted {
                hook_event,
                hook_name,
                status_message,
                ..
            } => {
                out.push(ag::Event::Custom(ag::Custom {
                    name: "agui-tui/hook_started".into(),
                    value: serde_json::json!({
                        "hookEvent": hook_event,
                        "hookName": hook_name,
                        "statusMessage": status_message,
                    }),
                    base: ag::BaseEvent::default(),
                }));
            }
            AgentEvent::HookProgress {
                hook_event,
                hook_name,
                last_line,
                ..
            } => {
                out.push(ag::Event::Custom(ag::Custom {
                    name: "agui-tui/hook_progress".into(),
                    value: serde_json::json!({
                        "hookEvent": hook_event,
                        "hookName": hook_name,
                        "lastLine": last_line,
                    }),
                    base: ag::BaseEvent::default(),
                }));
            }
            AgentEvent::HookFinished {
                hook_event,
                hook_name,
                outcome,
                duration_ms,
                ..
            } => {
                out.push(ag::Event::Custom(ag::Custom {
                    name: "agui-tui/hook_finished".into(),
                    value: serde_json::json!({
                        "hookEvent": hook_event,
                        "hookName": hook_name,
                        "outcome": outcome,
                        "durationMs": duration_ms,
                    }),
                    base: ag::BaseEvent::default(),
                }));
            }
            AgentEvent::HookSystemMessage { text, .. } => {
                out.push(ag::Event::Custom(ag::Custom {
                    name: "agui-tui/hook_system_message".into(),
                    value: serde_json::json!({ "text": text }),
                    base: ag::BaseEvent::default(),
                }));
            }
            // Task checklist updates: forward so clients can render live todo state.
            AgentEvent::TodoUpdated { todos, .. } => {
                out.push(ag::Event::Custom(ag::Custom {
                    name: "agui-tui/todo_updated".into(),
                    value: serde_json::json!({ "todos": todos }),
                    base: ag::BaseEvent::default(),
                }));
            }
            // checkpoint_post is emitted directly by the driver task (not via
            // AguiConverter) after RunFinished so it lands last.
            // heartbeat is emitted as an SSE comment at the HTTP layer.
            // permission_request and file_artifact require new AgentEvent variants
            // (tracked as g141/g140) before they can be mapped here.
            // Other variants (Latency, Usage, Compacted, PlanProposed,
            // PlanConfirmed, PlanRejected, etc.) have no AG-UI standard
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

    // ── Resume handling ──────────────────────────────────────────────────
    // If `input.resume` is present and non-empty, process the interrupt
    // resolutions before building the runtime.
    let resume_items: Vec<ag::Resume> = input.resume.unwrap_or_default();
    let mut seed_transcript: Option<Vec<crate::message::Message>> = None;

    if !resume_items.is_empty() {
        let session_dir =
            agui_session_dir(&state.config.workspace, &input.thread_id).ok_or_else(|| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse {
                        status: "error".into(),
                        error: "cannot resolve session directory for resume".into(),
                    }),
                )
            })?;

        if !session_dir.join("transcript.jsonl").is_file() {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    status: "error".into(),
                    error: format!(
                        "no prior run found for thread '{}'; cannot resume",
                        input.thread_id
                    ),
                }),
            ));
        }

        // Load open interrupts from session metadata.
        let open_interrupts = load_open_interrupts(&session_dir);
        if open_interrupts.is_empty() {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    status: "error".into(),
                    error: format!(
                        "thread '{}' has no open interrupts; nothing to resume",
                        input.thread_id
                    ),
                }),
            ));
        }

        // Spec rule 3: a single resume must address EVERY open interrupt.
        let resume_ids: std::collections::HashSet<String> = resume_items
            .iter()
            .map(|r| r.interrupt_id.clone())
            .collect();
        for open_int in &open_interrupts {
            if !resume_ids.contains(&open_int.interrupt_id) {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        status: "error".into(),
                        error: format!(
                            "resume must cover all open interrupts; missing '{}'",
                            open_int.interrupt_id
                        ),
                    }),
                ));
            }
        }

        // Load the transcript from the session.
        let loaded_messages =
            crate::session::SessionReader::load_messages(&session_dir).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse {
                        status: "error".into(),
                        error: format!("failed to load session transcript: {e}"),
                    }),
                )
            })?;

        // Build an index of resume items by interrupt_id.
        let resume_by_id: std::collections::HashMap<&str, &ag::Resume> = resume_items
            .iter()
            .map(|r| (r.interrupt_id.as_str(), r))
            .collect();

        // For each resolved interrupt, find the matching tool result in the
        // transcript and replace/inject the content. The tool was denied by
        // the TestInterruptHook, so we look for the tool result whose
        // `tool_call_id` matches the interrupt's bound tool_call_id.
        let mut modified = loaded_messages;
        for open_int in &open_interrupts {
            let Some(resume) = resume_by_id.get(open_int.interrupt_id.as_str()) else {
                continue;
            };
            let tool_call_id = &open_int.tool_call_id;

            if resume.status == ag::ResumeStatus::Cancelled {
                // For cancelled interrupts, inject a sentinel tool result.
                let sentinel = crate::message::Message::tool_result(
                    tool_call_id,
                    "[interrupt cancelled by user]",
                );
                modified.push(sentinel);
            } else if let Some(ref payload) = resume.payload {
                // Resolved: replace the denied tool result content with the
                // resume payload, or inject a new tool result if none exists.
                let payload_str = serde_json::to_string(payload).unwrap_or_default();
                let replaced = modified.iter_mut().any(|msg| {
                    if msg.tool_call_id.as_deref() == Some(tool_call_id.as_str()) {
                        msg.content = payload_str.clone();
                        true
                    } else {
                        false
                    }
                });
                if !replaced {
                    // No existing tool result found — inject one.
                    modified.push(crate::message::Message::tool_result(
                        tool_call_id,
                        &payload_str,
                    ));
                }
            }
        }

        // Clear the open interrupts now that they've been consumed.
        clear_open_interrupts(&session_dir);

        seed_transcript = Some(modified);
    }

    // ── Interrupt-before check (spec rule 4) ────────────────────────────
    // If the thread has open interrupts and no resume is provided, reject.
    if resume_items.is_empty() {
        if let Some(session_dir) = agui_session_dir(&state.config.workspace, &input.thread_id) {
            let open = load_open_interrupts(&session_dir);
            if !open.is_empty() {
                return Err((
                    StatusCode::CONFLICT,
                    Json(ErrorResponse {
                        status: "error".into(),
                        error: format!(
                            "thread '{}' has {} open interrupt(s); \
                             must provide resume to continue",
                            input.thread_id,
                            open.len()
                        ),
                    }),
                ));
            }
        }
    }

    // Acquire a semaphore permit to limit concurrent runs.
    // Goal-H J2: use `try_acquire_owned` so a saturated semaphore
    // returns immediately with a 503 (rather than awaiting
    // indefinitely via `acquire_owned().await`, which would hang
    // every /agui request when the pool is full). The previous
    // behaviour was documented in the g268 lead-completion
    // journal entry but the fix was deferred. Closing it here.
    let _permit = state
        .run_semaphore
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse {
                    status: "error".into(),
                    error: "too many concurrent runs, try again later".into(),
                }),
            )
        })?;

    // Common system-prompt assembly: project context + skill index +
    // coordinator/sub_agent note (when enabled).
    let system_prompt = crate::assemble_system_prompt(
        &state.config.system_prompt,
        &state.config.workspace,
        &state.skills,
        state.config.subagent_enabled,
    );

    // If interrupt_before is set, install a test-only permission hook on a
    // clone of the registry so matching tools are denied. The driver task
    // later checks if any tool was denied and emits an interrupt RunFinished.
    let interrupt_hook: Option<Arc<TestInterruptHook>> =
        input.interrupt_before.as_ref().map(|names| {
            Arc::new(TestInterruptHook {
                interrupt_before: names.clone(),
                interrupted_tool_call_id: std::sync::Mutex::new(None),
                interrupted_tool_name: std::sync::Mutex::new(None),
                interrupted_arguments: std::sync::Mutex::new(None),
            })
        });

    let mut tool_registry = state.tool_registry.clone();
    if let Some(ref hook) = interrupt_hook {
        tool_registry.set_permission_hook(hook.clone());
    }

    let mut runtime_builder = AgentRuntimeBuilder::new()
        .llm(state.provider.clone())
        .tools(tool_registry)
        .system_prompt(system_prompt)
        .max_steps(state.config.max_steps);

    // Seed the transcript if we're resuming.
    if let Some(seed) = seed_transcript {
        runtime_builder = runtime_builder.seed_transcript(seed);
    }

    let mut runtime = runtime_builder.build().map_err(|e| {
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
    // Capture the thread/run ids and interrupt hook for the driver task.
    let drv_thread = thread_id.clone();
    let drv_run = run_id.clone();
    let drv_interrupt_hook = interrupt_hook.clone();
    let drv_workspace = state.config.workspace.clone();

    let driver_handle = tokio::spawn(async move {
        // Scan transcript after run for test interrupt markers.
        let was_interrupted = drv_interrupt_hook
            .as_ref()
            .and_then(|hook| {
                hook.interrupted_tool_name
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
            })
            .is_some();

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
            Ok(o) => record_run_success(&metrics, o.steps, &o.total_usage),
            Err(_) => record_run_failed(&metrics),
        }

        // If the test interrupt hook was triggered, find the denied tool
        // call in the transcript to extract the tool_call_id.
        let interrupt_details: Option<(String, String)> = if was_interrupted {
            let transcript = runtime.transcript();
            let denied_tool_name = drv_interrupt_hook.as_ref().and_then(|h| {
                h.interrupted_tool_name
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
            });
            transcript
                .iter()
                .rev()
                .find(|msg| {
                    msg.role == crate::message::Role::Tool
                        && msg.content.contains("test interrupt trigger")
                })
                .and_then(|msg| {
                    msg.tool_call_id
                        .clone()
                        .map(|tc_id| (tc_id, denied_tool_name.unwrap_or_else(|| "unknown".into())))
                })
        } else {
            None
        };

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

        // Emit RunFinished — with Interrupt outcome if a test trigger fired.
        if let Some((tc_id, tc_name)) = interrupt_details {
            // Build the interrupt and persist it.
            let interrupt_id = uuid::Uuid::new_v4().to_string();
            let open_interrupt = OpenInterrupt {
                interrupt_id: interrupt_id.clone(),
                tool_call_id: tc_id.clone(),
                reason: "tool_call".into(),
                message: Some(format!(
                    "Test interrupt: tool '{tc_name}' needs user input to proceed"
                )),
                created_at: crate::session::chrono_lite_now(),
            };

            // Persist before emitting (crash safety).
            if let Some(session_dir) = agui_session_dir(&drv_workspace, &drv_thread) {
                let _ = std::fs::create_dir_all(&session_dir);
                save_open_interrupts(&session_dir, std::slice::from_ref(&open_interrupt));

                // Emit StateSnapshot and MessagesSnapshot before RunFinished
                // per spec requirement (snapshots must precede the interrupting
                // RunFinished event).
                if let Ok(state_val) = serde_json::to_value(runtime.transcript()) {
                    let _ = sse_tx.send(ag::Event::StateSnapshot(ag::StateSnapshot {
                        snapshot: state_val,
                        base: ag::BaseEvent::default(),
                    }));
                }
                let messages_json: Vec<serde_json::Value> = runtime
                    .transcript()
                    .iter()
                    .filter_map(|m| serde_json::to_value(m).ok())
                    .collect();
                let _ = sse_tx.send(ag::Event::MessagesSnapshot(ag::MessagesSnapshot {
                    messages: messages_json,
                    base: ag::BaseEvent::default(),
                }));
            }

            let _ = sse_tx.send(ag::Event::RunFinished(ag::RunFinished {
                thread_id: drv_thread,
                run_id: drv_run,
                outcome: Some(ag::RunFinishedOutcome::Interrupt {
                    interrupts: vec![ag::Interrupt {
                        id: interrupt_id,
                        reason: "tool_call".into(),
                        message: Some(format!(
                            "Test interrupt: tool '{tc_name}' needs user input to proceed"
                        )),
                        tool_call_id: Some(tc_id),
                        response_schema: Some(serde_json::json!({
                            "type": "object",
                            "properties": {
                                "approved": {"type": "boolean"}
                            }
                        })),
                        expires_at: None,
                        metadata: Some(serde_json::json!({
                            "testTrigger": true,
                            "toolName": tc_name,
                        })),
                    }],
                }),
                result: None,
                base: ag::BaseEvent::default(),
            }));
        } else {
            let _ = sse_tx.send(ag::Event::RunFinished(ag::RunFinished {
                thread_id: drv_thread,
                run_id: drv_run,
                outcome: Some(ag::RunFinishedOutcome::Success),
                result: None,
                base: ag::BaseEvent::default(),
            }));
        }
    });

    // Monitor the driver task so panics are surfaced in logs rather than
    // silently swallowed by the dropped JoinHandle.
    tokio::spawn(async move {
        if let Err(e) = driver_handle.await {
            tracing::error!("agui: driver task panicked: {e}");
        }
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

// ── AG-UI Interrupt/Resume helpers ──────────────────────────────────────

/// Path to the JSONL session directory for an AG-UI thread.
fn agui_session_dir(workspace: &std::path::Path, thread_id: &str) -> Option<std::path::PathBuf> {
    let session_id = sanitize_thread_id_for_session(thread_id);
    crate::user_sessions_dir(workspace)
        .ok()
        .map(|d| d.join(format!("agui-{session_id}")))
}

/// One open interrupt persisted in the session metadata.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct OpenInterrupt {
    pub interrupt_id: String,
    pub tool_call_id: String,
    pub reason: String,
    pub message: Option<String>,
    pub created_at: String,
}

/// Load open interrupts from session metadata.
fn load_open_interrupts(session_dir: &std::path::Path) -> Vec<OpenInterrupt> {
    let path = session_dir.join(".interrupts.json");
    let Ok(bytes) = std::fs::read(&path) else {
        return Vec::new();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

/// Save open interrupts to session metadata. Written atomically before
/// emitting RunFinished with Interrupt outcome.
fn save_open_interrupts(session_dir: &std::path::Path, interrupts: &[OpenInterrupt]) {
    if let Ok(json) = serde_json::to_string_pretty(interrupts) {
        let path = session_dir.join(".interrupts.json");
        let _ = std::fs::create_dir_all(session_dir);
        crate::atomic::atomic_write(&path, json.as_bytes()).ok();
    }
}

/// Clear open interrupts (called after a successful resume that consumed
/// all pending interrupts).
fn clear_open_interrupts(session_dir: &std::path::Path) {
    let path = session_dir.join(".interrupts.json");
    let _ = std::fs::remove_file(&path);
}

/// Test-only permission hook: denies tools whose names appear in
/// `interrupt_before`, records the first such denial as an 'interrupt'.
/// This is the test-only trigger — the real permission_pipeline.Ask
/// integration is g325.
struct TestInterruptHook {
    interrupt_before: Vec<String>,
    /// Set to the first tool_call_id that was denied (if any).
    interrupted_tool_call_id: std::sync::Mutex<Option<String>>,
    /// Set to the name of the first tool that was denied.
    interrupted_tool_name: std::sync::Mutex<Option<String>>,
    /// Set to the arguments of the first tool that was denied.
    interrupted_arguments: std::sync::Mutex<Option<serde_json::Value>>,
}

#[async_trait::async_trait]
#[async_trait]
impl crate::tools::PermissionHook for TestInterruptHook {
    async fn check(
        &self,
        name: &str,
        args: &serde_json::Value,
    ) -> crate::agent::PermissionDecision {
        if self
            .interrupted_tool_call_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
        {
            // Already interrupted — don't deny further tools.
            return crate::agent::PermissionDecision::Allow;
        }
        if self.interrupt_before.iter().any(|n| n == name) {
            // Record the interrupt — the tool_call_id is synthetic because
            // the actual tool_call hasn't been assigned an id yet at this
            // point. The drive task will capture the real id from the stream.
            *self
                .interrupted_tool_name
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(name.to_string());
            *self
                .interrupted_arguments
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(args.clone());
            return crate::agent::PermissionDecision::Deny(
                "test interrupt trigger — tool blocked by interrupt_before".into(),
            );
        }
        crate::agent::PermissionDecision::Allow
    }
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
    let sessions_active = metrics.sessions_active.load(Ordering::Relaxed);
    let rate_limits_rejected = metrics.rate_limits_rejected.load(Ordering::Relaxed);

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
         recursive_agent_steps_total {agent_steps_total}\n\
         # HELP recursive_sessions_active Currently active sessions\n\
         # TYPE recursive_sessions_active gauge\n\
         recursive_sessions_active {sessions_active}\n\
         # HELP recursive_rate_limits_rejected_total Total requests rejected by rate limiting\n\
         # TYPE recursive_rate_limits_rejected_total counter\n\
         recursive_rate_limits_rejected_total {rate_limits_rejected}\n"
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

    /// Goal-268 + Goal-H J2: /agui must respect run_semaphore. The
    /// handler uses `try_acquire_owned` (J2) so a saturated (0-
    /// permit) semaphore returns 503 SERVICE_UNAVAILABLE
    /// **immediately** rather than blocking forever on
    /// `acquire_owned().await`. A 0-permit `Semaphore` is the
    /// natural test fixture — no `close()` workaround needed
    /// (the previous form tested a *closed* semaphore, which is
    /// a different code path inside `try_acquire_owned`).
    #[tokio::test]
    async fn agui_run_respects_run_semaphore() {
        use crate::llm::MockProvider;
        use crate::tools::ToolRegistry;
        use std::sync::Arc;
        use tokio::sync::Semaphore;

        std::env::set_var("RECURSIVE_API_KEY", "test-key");
        std::env::set_var("RECURSIVE_MODEL", "test-model");
        let config = crate::config::Config::from_env().unwrap();

        // 0-permit semaphore: every `try_acquire_owned` call
        // returns `TryAcquireError::NoPermits` immediately.
        let sem = Arc::new(Semaphore::new(0));

        let state = Arc::new(crate::http::AppState {
            tools: vec![],
            tool_registry: ToolRegistry::default(),
            config,
            provider: Arc::new(MockProvider::new(vec![])),
            sessions: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            event_channels: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            metrics: Arc::new(crate::http::Metrics::default()),
            slash_commands: Arc::new(vec![]),
            session_ttl_secs: 3600,
            run_semaphore: sem,
            rate_limiter: crate::http::RateLimiter::new(10, 1.0),
            skills: vec![],
        });

        let body = serde_json::json!({
            "threadId": "t1",
            "runId": "r1",
            "messages": [{"id": "m1", "role": "user", "content": "hi"}],
        });
        let (status, _err) = agui_run(State(state), Json(body))
            .await
            .expect_err("expected SERVICE_UNAVAILABLE");
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    // ── Goal-280: clear_goal returns 409 when runtime busy ────────────

    /// Simulate a busy runtime (mutex held by an in-flight turn) and
    /// verify `session_clear_goal` returns 409 with Retry-After: 5.
    /// Then release the lock and verify the next call returns 200.
    ///
    /// Goal-313: the handler now returns `Result<Json<...>, ApiError>`
    /// instead of `Response` directly. We convert via
    /// `axum::response::IntoResponse` so the same assertions (status,
    /// headers, body) still work.
    #[tokio::test]
    async fn clear_goal_returns_409_when_runtime_busy() {
        use crate::llm::MockProvider;
        use crate::tools::ToolRegistry;
        use axum::response::IntoResponse;
        use std::sync::Arc;
        use tokio::sync::Semaphore;

        std::env::set_var("RECURSIVE_API_KEY", "test-key");
        std::env::set_var("RECURSIVE_MODEL", "test-model");
        let config = crate::config::Config::from_env().unwrap();
        let provider = Arc::new(MockProvider::new(vec![]));

        let session_id = "test-busy-session".to_string();
        let runtime = AgentRuntimeBuilder::new()
            .llm(provider.clone())
            .tools(ToolRegistry::default())
            .build()
            .expect("runtime build");
        let runtime_arc = Arc::new(tokio::sync::Mutex::new(runtime));
        let session = SessionState {
            id: session_id.clone(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            title: None,
            runtime: runtime_arc.clone(),
            plan_approval_gate: Default::default(),
            interrupt_token: Arc::new(tokio::sync::Mutex::new(None)),
            non_system_message_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            last_active_ms: Arc::new(AtomicU64::new(0)),
            prompt_tokens: Arc::new(AtomicU64::new(0)),
            completion_tokens: Arc::new(AtomicU64::new(0)),
        };

        let sessions: HashMap<String, SessionState> = [(session_id.clone(), session)].into();
        let state = Arc::new(AppState {
            tools: vec![],
            tool_registry: ToolRegistry::default(),
            config,
            provider,
            sessions: Arc::new(tokio::sync::RwLock::new(sessions)),
            event_channels: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            metrics: Arc::new(crate::http::Metrics::default()),
            slash_commands: Arc::new(vec![]),
            session_ttl_secs: 3600,
            run_semaphore: Arc::new(Semaphore::new(8)),
            rate_limiter: crate::http::RateLimiter::new(10, 1.0),
            skills: vec![],
        });

        // Acquire the runtime mutex to simulate a busy runtime.
        let guard = runtime_arc.lock().await;

        // Call the handler while the mutex is held → should get 409
        // (Result::Err(ApiError::conflict(...).with_retry_after(5))).
        let resp = session_clear_goal(State(state.clone()), Path(session_id.clone()))
            .await
            .into_response();
        let status = resp.status();
        assert_eq!(status, StatusCode::CONFLICT, "expected 409 Conflict");
        let retry_after = resp
            .headers()
            .get(axum::http::header::RETRY_AFTER)
            .expect("Retry-After header missing")
            .to_str()
            .unwrap();
        assert_eq!(retry_after, "5", "expected Retry-After: 5");

        // Drop the guard and retry → should get 200.
        drop(guard);
        let resp = session_clear_goal(State(state), Path(session_id))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::OK, "expected 200 after unlock");
    }

    /// Goal-292: metrics_handler output includes sessions_active and
    /// rate_limits_rejected.
    #[tokio::test]
    async fn metrics_handler_includes_new_fields() {
        use crate::http::Metrics;
        use crate::tools::ToolRegistry;
        use std::sync::atomic::AtomicU64;
        std::env::set_var("RECURSIVE_API_KEY", "test-key");
        std::env::set_var("RECURSIVE_MODEL", "test-model");
        let config = crate::config::Config::from_env().unwrap();
        let metrics = Metrics {
            sessions_active: AtomicU64::new(3),
            rate_limits_rejected: AtomicU64::new(42),
            ..Metrics::default()
        };
        let state = Arc::new(AppState {
            metrics: Arc::new(metrics),
            tools: vec![],
            tool_registry: ToolRegistry::default(),
            config,
            provider: Arc::new(crate::llm::MockProvider::new(vec![])),
            sessions: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            event_channels: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            slash_commands: Arc::new(vec![]),
            session_ttl_secs: 3600,
            run_semaphore: Arc::new(tokio::sync::Semaphore::new(8)),
            rate_limiter: crate::http::RateLimiter::new(10, 1.0),
            skills: vec![],
        });
        let output = metrics_handler(State(state)).await;
        assert!(
            output.contains("recursive_sessions_active 3"),
            "output should contain sessions_active: {output}"
        );
        assert!(
            output.contains("recursive_rate_limits_rejected_total 42"),
            "output should contain rate_limits_rejected_total: {output}"
        );
    }

    /// Goal-292: sessions_active increments on create_session and
    /// decrements on delete_session.
    #[tokio::test]
    async fn sessions_active_tracks_session_lifecycle() {
        use crate::tools::ToolRegistry;
        use tower::ServiceExt;
        std::env::set_var("RECURSIVE_API_KEY", "test-key");
        std::env::set_var("RECURSIVE_MODEL", "test-model");
        std::env::set_var("RECURSIVE_HTTP_AUTH_INSECURE_OK", "1");
        let config = crate::config::Config::from_env().unwrap();
        let provider = Arc::new(crate::llm::MockProvider::new(vec![]));
        let metrics = Arc::new(crate::http::Metrics::default());

        let state = AppState {
            tools: vec![],
            tool_registry: ToolRegistry::default(),
            config,
            provider,
            sessions: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            event_channels: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            metrics: metrics.clone(),
            slash_commands: Arc::new(vec![]),
            session_ttl_secs: 3600,
            run_semaphore: Arc::new(tokio::sync::Semaphore::new(8)),
            rate_limiter: crate::http::RateLimiter::new(100, 1.0),
            skills: vec![],
        };

        let auth = crate::http::auth::AuthConfig::default();
        let limiter = state.rate_limiter.clone();
        let app = crate::http::build_router_with_auth_and_rate_limit(state, auth, limiter);

        // Initially sessions_active is 0.
        assert_eq!(
            metrics.sessions_active.load(Ordering::Relaxed),
            0,
            "sessions_active should start at 0"
        );

        // Create a session.
        let resp = app
            .clone()
            .oneshot(
                axum::extract::Request::builder()
                    .method("POST")
                    .uri("/sessions")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"session_name":"test"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::CREATED,
            "expected 201 Created from POST /sessions"
        );
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let created: CreateSessionResponse =
            serde_json::from_slice(&body).expect("valid CreateSessionResponse");
        let session_id = created.id;

        // sessions_active should now be 1.
        assert_eq!(
            metrics.sessions_active.load(Ordering::Relaxed),
            1,
            "sessions_active should increment to 1 after create"
        );

        // Delete the session.
        let resp = app
            .clone()
            .oneshot(
                axum::extract::Request::builder()
                    .method("DELETE")
                    .uri(format!("/sessions/{session_id}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NO_CONTENT,
            "expected 204 No Content from DELETE /sessions/:id"
        );

        // sessions_active should be back to 0.
        assert_eq!(
            metrics.sessions_active.load(Ordering::Relaxed),
            0,
            "sessions_active should decrement to 0 after delete"
        );
    }

    // ── G298: OpenAPI spec sync ───────────────────────────────────────

    /// Verify that `build_openapi_spec()` correctly describes the
    /// `SessionDetailResponse` schema with all the fields added in
    /// G293, G294, G295, G296, etc.
    #[test]
    fn openapi_session_detail_has_complete_schema() {
        let spec = super::build_openapi_spec();
        let props = &spec["components"]["schemas"]["SessionDetailResponse"]["properties"];

        // Fields from the original (G-pre) spec.
        assert!(props.get("id").is_some(), "id missing");
        assert!(props.get("created_at").is_some(), "created_at missing");
        assert!(props.get("messages").is_some(), "messages missing");

        // Fields added by G293-G296 that the goal explicitly requires.
        assert!(
            props.get("prompt_tokens").is_some(),
            "prompt_tokens missing"
        );
        assert!(
            props.get("completion_tokens").is_some(),
            "completion_tokens missing"
        );
        assert!(props.get("status").is_some(), "status missing");
        assert!(props.get("todos").is_some(), "todos missing");
        assert!(props.get("goal").is_some(), "goal missing");

        // Remaining fields: title, pending_plan, first_prompt, last_prompt.
        assert!(props.get("title").is_some(), "title missing");
        assert!(props.get("pending_plan").is_some(), "pending_plan missing");
        assert!(props.get("first_prompt").is_some(), "first_prompt missing");
        assert!(props.get("last_prompt").is_some(), "last_prompt missing");

        // Verify we have at least 10 properties total.
        let obj = props.as_object().expect("properties is an object");
        assert!(
            obj.len() >= 10,
            "SessionDetailResponse should have ≥10 properties, got {}",
            obj.len()
        );
    }

    /// Verify `SessionInfo` schema includes `message_count` and `title`.
    #[test]
    fn openapi_session_info_has_message_count_and_title() {
        let spec = super::build_openapi_spec();
        let props = &spec["components"]["schemas"]["SessionInfo"]["properties"];

        assert!(props.get("id").is_some(), "id missing");
        assert!(props.get("created_at").is_some(), "created_at missing");
        assert!(
            props.get("message_count").is_some(),
            "message_count missing"
        );
        assert!(props.get("title").is_some(), "title missing");
    }

    /// Verify the `/metrics` path exists and mentions the two G292
    /// metrics in its description.
    #[test]
    fn openapi_metrics_path_documents_new_metrics() {
        let spec = super::build_openapi_spec();
        let description = spec["paths"]["/metrics"]["get"]["description"]
            .as_str()
            .expect("metrics path description is a string");

        assert!(
            description.contains("recursive_sessions_active"),
            "metrics description should mention recursive_sessions_active: {description}"
        );
        assert!(
            description.contains("recursive_rate_limits_rejected_total"),
            "metrics description should mention recursive_rate_limits_rejected_total: {description}"
        );
    }

    // ── Goal-303: sort GET /sessions results by created_at ───────────

    /// Verify that sorting SessionInfo by `created_at` yields
    /// chronological order (oldest first).
    #[test]
    fn list_sessions_sort_is_chronological() {
        let mut infos = [
            SessionInfo {
                id: "c".into(),
                created_at: "2026-01-03T00:00:00Z".into(),
                message_count: 0,
                title: None,
            },
            SessionInfo {
                id: "a".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
                message_count: 0,
                title: None,
            },
            SessionInfo {
                id: "b".into(),
                created_at: "2026-01-02T00:00:00Z".into(),
                message_count: 0,
                title: None,
            },
        ];
        infos.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        assert_eq!(infos[0].id, "a");
        assert_eq!(infos[1].id, "b");
        assert_eq!(infos[2].id, "c");
    }

    /// Verify that sessions created in the same second are tie-broken
    /// by `id` so the sort remains fully deterministic.
    #[test]
    fn list_sessions_same_second_tiebreak_by_id() {
        let mut infos = [
            SessionInfo {
                id: "z".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
                message_count: 0,
                title: None,
            },
            SessionInfo {
                id: "a".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
                message_count: 0,
                title: None,
            },
        ];
        infos.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        assert_eq!(infos[0].id, "a");
        assert_eq!(infos[1].id, "z");
    }

    // ── Goal-312: skill_index injection into system prompt ──────────

    /// Verify that system prompt construction logic correctly appends
    /// the skill index when skills are present in AppState.
    #[test]
    fn system_prompt_includes_skill_index_when_skills_present() {
        let skills = vec![crate::skills::Skill {
            name: "rust-patch-discipline".to_string(),
            description: "V4A patch format rules".to_string(),
            path: std::path::PathBuf::from("/tmp/skills/rust-patch-discipline/SKILL.md"),
            mode: crate::skills::SkillMode::Manual,
            triggers: vec![],
            hint: String::new(),
            depends_on: vec![],
            refs: vec![],
            params: vec![],
            scripts: vec![],
            sections: vec![],
            globs: None,
        }];

        let base_prompt = "You are a helpful agent.";
        let idx = crate::skills::skill_index(&skills);
        assert!(!idx.is_empty(), "skill_index should not be empty");
        assert!(
            idx.contains("Available skills"),
            "skill_index should contain header"
        );
        assert!(
            idx.contains("rust-patch-discipline"),
            "skill_index should list skill names"
        );

        // Simulate what run_agent/create_session do.
        let mut sp = base_prompt.to_string();
        sp.push('\n');
        sp.push_str(&idx);

        assert!(
            sp.contains("Available skills"),
            "system prompt should contain skill index after injection"
        );
        assert!(
            sp.contains("rust-patch-discipline"),
            "system prompt should contain skill name"
        );
    }

    /// skill_index returns empty string when no skills are present,
    /// so injection is a no-op (no spurious newlines).
    #[test]
    fn system_prompt_unchanged_when_no_skills() {
        let skills: Vec<crate::skills::Skill> = vec![];
        let idx = crate::skills::skill_index(&skills);
        assert!(idx.is_empty(), "empty skills should produce empty index");

        let base_prompt = "You are a helpful agent.";
        let mut sp = base_prompt.to_string();
        if !idx.is_empty() {
            sp.push('\n');
            sp.push_str(&idx);
        }
        // The prompt should be unchanged.
        assert_eq!(sp, base_prompt);
    }

    // ── format_timestamp ────────────────────────────────────────────────────

    #[test]
    fn format_timestamp_unix_epoch_is_1970() {
        let ts = format_timestamp(SystemTime::UNIX_EPOCH);
        assert_eq!(
            &ts[..10],
            "1970-01-01",
            "epoch date must be 1970-01-01; got {ts}"
        );
        assert_eq!(
            &ts[11..19],
            "00:00:00",
            "epoch time must be 00:00:00; got {ts}"
        );
        assert!(ts.ends_with('Z'), "must end with Z; got {ts}");
    }

    #[test]
    fn format_timestamp_known_date() {
        // 2026-07-06T00:00:00Z = 20640 days * 86400 sec = 1_783_296_000 seconds
        let secs = 1_783_296_000u64;
        let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        let ts = format_timestamp(t);
        assert_eq!(&ts[..10], "2026-07-06", "expected 2026-07-06; got {ts}");
        assert_eq!(&ts[11..19], "00:00:00", "time part must be zero; got {ts}");
    }

    #[test]
    fn format_timestamp_time_parts_in_range() {
        // 2026-07-06T13:45:30Z
        let secs: u64 = 1_783_296_000 + 13 * 3600 + 45 * 60 + 30;
        let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        let ts = format_timestamp(t);
        assert_eq!(&ts[11..13], "13", "hours mismatch; got {ts}");
        assert_eq!(&ts[14..16], "45", "minutes mismatch; got {ts}");
        assert_eq!(&ts[17..19], "30", "seconds mismatch; got {ts}");
    }

    // ── sanitize_thread_id_for_session ──────────────────────────────────────

    #[test]
    fn sanitize_thread_id_valid_passthrough() {
        assert_eq!(sanitize_thread_id_for_session("abc-123"), "abc-123");
        assert_eq!(sanitize_thread_id_for_session("foo_bar.baz"), "foo_bar.baz");
    }

    #[test]
    fn sanitize_thread_id_replaces_special_chars() {
        let out = sanitize_thread_id_for_session("a/b:c");
        assert!(!out.contains('/'), "slash must be replaced");
        assert!(!out.contains(':'), "colon must be replaced");
    }

    #[test]
    fn sanitize_thread_id_leading_dot_replaced() {
        let out = sanitize_thread_id_for_session(".hidden");
        assert!(
            !out.starts_with('.'),
            "leading dot must be replaced; got {out}"
        );
    }

    #[test]
    fn sanitize_thread_id_double_dot_collapsed() {
        let out = sanitize_thread_id_for_session("a..b");
        assert!(
            !out.contains(".."),
            "double dot must be collapsed; got {out}"
        );
    }

    #[test]
    fn sanitize_thread_id_empty_becomes_default() {
        assert_eq!(sanitize_thread_id_for_session(""), "default");
    }
}
