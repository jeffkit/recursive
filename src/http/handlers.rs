//! HTTP handler functions for the agent API.

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
use crate::tools::ToolRegistry;

use super::{
    build_openapi_spec, AcquireError, AdmissionGate, ApiError, AppState, CreateSessionRequest,
    CreateSessionResponse, ErrorResponse, ListSessionsQuery, RunRequest, RunResponse,
    SessionDetailResponse, SessionInfo, SessionMessageRequest, SessionMessageResponse,
    SessionState, SetGoalRequest, SlashCommandInfo, SseContentBlock, SseEvent, ToolInfo, UsageInfo,
};

// Constant body — no branching worth scoring.
#[cfg_attr(test, mutants::skip)]
pub(super) async fn health() -> &'static str {
    "ok"
}

/// Map an admission failure to the standardized API error (Goal 398).
///
/// `Timeout` → `503 Service Unavailable` with a `Retry-After` hint; a closed
/// semaphore keeps the historical "too many concurrent runs" 503. The
/// `Retry-After` value is a **rough drain estimate**
/// (`ceil(runs_waiting / max_concurrent)`, see
/// [`AdmissionGate::estimate_retry_after_secs`]) — deliberately conservative
/// and always a plain integer so any HTTP client can parse it.
fn admission_error(err: AcquireError, gate: &AdmissionGate) -> ApiError {
    match err {
        AcquireError::Timeout { waited } => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "server at capacity: no run slot after waiting {}s, try again later",
                waited.as_secs()
            ),
        )
        .with_retry_after(gate.estimate_retry_after_secs()),
        AcquireError::Closed => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many concurrent runs, try again later",
        ),
    }
}

/// Issue #31 §2: inject the `<environment>` segment into an assembled
/// prompt ONLY when the session's transport reports non-local capabilities
/// (container tier). The local tier (empty `path_root` + default local
/// semantics) keeps the prompt byte-identical to the pre-#31 form.
pub(super) fn inject_environment_segment(
    mut full: String,
    mut segments: crate::system_prompt::PromptSegments,
    registry: &ToolRegistry,
) -> (String, crate::system_prompt::PromptSegments) {
    let caps = registry.transport().capabilities();
    // Local semantics = empty path_root; treat as "no environment segment".
    if caps.path_root.as_os_str().is_empty() {
        return (full, segments);
    }
    let seg = caps.render_environment_segment();
    full.push_str(&seg);
    segments.environment = seg;
    (full, segments)
}

/// Goal-393: the one place where HTTP session runtimes get built. Every
/// build point (`POST /run`, `POST /sessions`, session fork) goes
/// through here so the channels cannot drift apart — the compactor /
/// microcompactor / transcript-cap assembly comes from the same
/// frontend-neutral helper the CLI uses (`apply_context_management`).
///
/// `/agui` builds through [`build_session_runtime_parts`] (same context
/// management) plus the provider / wall-timeout / storage setters here —
/// its runtime assembly lives in `super::agui`, away from the axum types.
///
/// Callers add what is genuinely request-specific on top of the returned
/// builder (`seed_transcript` for `/agui` resume, then `build()`).
fn build_session_runtime(
    state: &AppState,
    tool_registry: ToolRegistry,
    system_prompt: String,
    prompt_segments: crate::system_prompt::PromptSegments,
    max_steps: usize,
) -> AgentRuntimeBuilder {
    crate::runtime::apply_context_management(
        build_session_runtime_parts(
            tool_registry,
            system_prompt,
            prompt_segments,
            max_steps,
            &state.config.model,
        )
        .llm(state.provider.clone())
        // Goal 399: safe wall-clock budget for HTTP sessions
        // (env-overridable via RECURSIVE_HTTP_WALL_TIMEOUT_SECS, resolved
        // into state.config at server startup). Exceeding it finishes
        // with WallClockExceeded.
        .wall_timeout_secs(state.config.wall_timeout_secs)
        // Goal 396: the host layer persists this session's transcript
        // through the same storage backend on teardown (DELETE / idle
        // eviction / graceful shutdown) — not per turn.
        .storage(state.storage.clone())
        // Issue #66 §3.2: token-level streaming for every HTTP entry
        // point (/sessions, /runs, /agui). RunCore only builds the
        // partial-token forwarder when `streaming` is set, so before
        // this an AG-UI answer arrived as ONE TextMessageContent frame
        // and `/sessions/:id/events` never emitted `partial_message`.
        // Consumers are ready: the AguiConverter frames PartialToken
        // deltas into TextMessageStart/Content/End, and both SDKs treat
        // `partial_message`/`stream_event` as fire-hose-only — their
        // final result still aggregates from the complete `message`
        // events.
        .streaming(true),
        &state.config,
    )
}

/// Provider-agnostic core of [`build_session_runtime`]: context management
/// only. The AG-UI layer (`super::agui::build_agui_runtime`) layers the
/// provider and wall-clock budget on top, keeping its runtime build free of
/// `AppState`.
pub(super) fn build_session_runtime_parts(
    tool_registry: ToolRegistry,
    system_prompt: String,
    prompt_segments: crate::system_prompt::PromptSegments,
    max_steps: usize,
    model: &str,
) -> AgentRuntimeBuilder {
    // `apply_context_management` only reads `config.model` (auto compaction
    // thresholds); a minimal stub carrying exactly that model keeps the
    // AG-UI build path independent of `AppState` while producing thresholds
    // identical to `/run` and `/sessions` — they all pass the SAME model
    // (`state.config.model`) in, so the channels cannot drift apart.
    let config = crate::config::Config {
        workspace: std::path::PathBuf::from("."),
        model: model.to_string(),
        ..crate::http::test_config_stub()
    };
    crate::runtime::apply_context_management(
        AgentRuntimeBuilder::new()
            .tools(tool_registry)
            .system_prompt(system_prompt)
            .prompt_segments(prompt_segments)
            .max_steps(max_steps),
        &config,
    )
}

/// Update metrics after a successful agent run.
pub(super) fn record_run_success(
    metrics: &super::Metrics,
    steps: usize,
    usage: &crate::llm::TokenUsage,
) {
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
pub(super) fn record_run_failed(metrics: &super::Metrics) {
    metrics.agent_runs_total.fetch_add(1, Ordering::Relaxed);
    metrics.agent_runs_failed.fetch_add(1, Ordering::Relaxed);
}

/// Map a typed runtime [`crate::error::Error`] to the correct HTTP status code.
///
/// Goal 361: previously every run failure collapsed to 500, so a client could
/// not distinguish "your input was bad" (400) from "you got rate limited"
/// (429, retry after N seconds) from "we crashed" (500). This helper is the
/// single place that translates a runtime error into the right [`ApiError`];
/// both run-entry handlers call it after recording metrics.
///
/// Deliberately NOT mapped per-variant: `Io`/`Json`/`Mcp`/`Llm`/`Timeout`/
/// `Tool` etc. are server-side failures — 500 is correct for them. Only
/// client-correctable or retryable conditions get a 4xx/503.
///
/// `Cancelled` maps to 503 (Service Unavailable) rather than 500: 499 is a
/// non-standard nginx code and axum/tower has no built-in constant for it,
/// and this goal explicitly prefers 503 over 500 (a client retrying a cancel
/// as 500 is the pre-existing bug; 503 at least signals "transient, ok to
/// retry"). We do NOT return 200-with-body for a cancel because no other
/// finish-reason in this API uses that convention — the run genuinely did
/// not complete.
fn map_run_error(e: &crate::error::Error) -> ApiError {
    use crate::error::Error;
    match e {
        Error::Cancelled => ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "agent run cancelled"),
        Error::PermissionDenied { .. } | Error::PermissionDeniedLimit { .. } => {
            ApiError::forbidden(e.to_string())
        }
        Error::RateLimited { retry_after_ms, .. } => {
            // `with_retry_after` takes whole seconds; floor so a sub-second
            // wait becomes `Retry-After: 0` (retry immediately) rather than
            // over-promising.
            ApiError::new(StatusCode::TOO_MANY_REQUESTS, e.to_string())
                .with_retry_after((*retry_after_ms / 1000) as u32)
        }
        Error::BadToolArgs { .. } => ApiError::bad_request(e.to_string()),
        // Everything else is genuinely internal: keep 500.
        _ => ApiError::internal(e.to_string()),
    }
}

// Thin wrapper around `build_openapi_spec` (schema covered elsewhere).
#[cfg_attr(test, mutants::skip)]
pub(super) async fn openapi_spec() -> Json<serde_json::Value> {
    Json(build_openapi_spec())
}

// Pure clone of AppState field — no branching worth scoring.
#[cfg_attr(test, mutants::skip)]
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

    // Acquire a run permit with a bounded wait (Goal 398): a saturated pool
    // now fails fast with 503 + Retry-After instead of hanging the request.
    let _permit = state
        .host
        .admission()
        .acquire_run()
        .await
        .map_err(|e| admission_error(e, &state.host.admission()))?;
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
    let assembled_system_prompt = crate::assemble_system_prompt(
        &system_prompt,
        &state.config.workspace,
        &state.skills,
        state.config.subagent_enabled,
    );
    let system_prompt = assembled_system_prompt.full;
    let prompt_segments = assembled_system_prompt.segments;
    let mut tool_registry = state
        .session_tool_registry()
        .await
        .map_err(|e| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, e))?;
    // Issue #31 §2: inject the `<environment>` segment only when the
    // session's transport is a real sandbox (non-local capabilities).
    let (system_prompt, prompt_segments) =
        inject_environment_segment(system_prompt, prompt_segments, &tool_registry);
    if let Some(mode_str) = body.permission_mode.as_deref() {
        let perm_mode = parse_permission_mode(mode_str, state.config.allow_bypass_permissions);
        tool_registry = tool_registry.with_permissions(LayeredPermissionsConfig {
            mode: perm_mode,
            layers: Vec::new(),
        });
    }

    let mut runtime = build_session_runtime(
        &state,
        tool_registry,
        system_prompt,
        prompt_segments,
        max_steps,
    )
    .build()
    .map_err(|e| ApiError::internal(format!("failed to build runtime: {e}")))?;

    // Issue #31 §B: this one-shot run owns its environment (container tier
    // creates one per run) — destroy it on BOTH exits so no container
    // outlives the request.
    let outcome = match runtime.run(&body.goal).await {
        Ok(o) => o,
        Err(e) => {
            runtime.destroy_environment().await;
            record_run_failed(&state.metrics);
            return Err(map_run_error(&e));
        }
    };
    runtime.destroy_environment().await;

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
// Non-deterministic UUID — not unit-observable for mutation scoring.
#[cfg_attr(test, mutants::skip)]
fn generate_session_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// Format a SystemTime as a basic ISO-8601 string (without chrono).
pub(super) fn format_timestamp(t: SystemTime) -> String {
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
    let assembled_system_prompt = crate::assemble_system_prompt(
        &system_prompt,
        &state.config.workspace,
        &state.skills,
        state.config.subagent_enabled,
    );
    let system_prompt = assembled_system_prompt.full;
    let prompt_segments = assembled_system_prompt.segments;
    let max_steps = body
        .max_steps
        .map(|n| n as usize)
        .unwrap_or(state.config.max_steps);
    let mut tool_registry = state
        .session_tool_registry()
        .await
        .map_err(|e| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, e))?;
    let (system_prompt, prompt_segments) =
        inject_environment_segment(system_prompt, prompt_segments, &tool_registry);
    if let Some(mode_str) = body.permission_mode.as_deref() {
        let perm_mode = parse_permission_mode(mode_str, state.config.allow_bypass_permissions);
        tool_registry = tool_registry.with_permissions(LayeredPermissionsConfig {
            mode: perm_mode,
            layers: Vec::new(),
        });
    }

    let mut runtime = build_session_runtime(
        &state,
        tool_registry,
        system_prompt,
        prompt_segments,
        max_steps,
    )
    .build()
    .map_err(|e| ApiError::internal(format!("failed to build session runtime: {e}")))?;

    // Register the session ID so all turns emit tracing spans with
    // session_id. The transcript is NOT saved per turn — the host layer
    // persists it once on teardown (DELETE / idle eviction / graceful
    // shutdown) through the storage backend that `build_session_runtime`
    // wires into the builder (Goal 396).
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

    state
        .host
        .sessions()
        .write()
        .await
        .insert(id.clone(), session);
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
    let sessions_lock = state.host.sessions();
    let sessions = sessions_lock.read().await;
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
///
/// Goal 397: a session persisted by a previous server process is restored
/// from the storage backend here (cold load) instead of 404-ing.
pub(super) async fn get_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<SessionDetailResponse>, ApiError> {
    let session = super::cold_load::get_or_load_session(&state, &id).await?;

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
        let sessions_lock = state.host.sessions();
        let sessions = sessions_lock.read().await;
        sessions.get(&id).map(|s| s.runtime.clone())
    };
    if let Some(runtime) = session_runtime {
        // Fire SessionEnd (no outcome — the client is deleting the session
        // without a terminating turn) and flip `session_closed` before the
        // runtime is dropped. Idempotent on repeated calls.
        let mut rt = runtime.lock().await;
        rt.close(None).await;
        // Issue #31 §B: explicit, idempotent environment teardown paired
        // with session deletion. Failure is logged only — destroy is
        // idempotent and retryable, it must not block the HTTP delete.
        rt.destroy_environment().await;
        // Goal 396: snapshot the transcript before releasing the runtime
        // Mutex, but persist it only after the session is out of the map —
        // the save is I/O and must not run under either lock. 会话结束即落盘是
        // 396 的既定语义（DELETE / 驱逐 / 优雅停机同路）。
        let transcript = rt.transcript().to_vec();
        drop(rt);
        state.host.sessions().write().await.remove(&id);
        state
            .metrics
            .sessions_active
            .fetch_sub(1, Ordering::Relaxed);
        // Clean up SSE event channel for this session.
        state.event_channels.write().await.remove(&id);
        if let Err(e) = state.storage.save_transcript(&id, &transcript).await {
            tracing::warn!(
                session_id = %id,
                error = %e,
                "failed to persist deleted session transcript"
            );
        }
        // Goal 396/397 集成语义：快照保留，但删掉的会话**不得被冷加载复活**
        // （否则 DELETE → GET 会 200，违反 v050 生命周期契约）。落一个 tombstone，
        // 冷加载见它即 404；驱逐/停机不写 tombstone，仍可从存储恢复。
        if let Err(e) = state
            .storage
            .save_memory(&super::cold_load::deleted_marker_key(&id), "1")
            .await
        {
            tracing::warn!(
                session_id = %id,
                error = %e,
                "failed to write session tombstone"
            );
        }
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
    let sessions_lock = state.host.sessions();
    let mut sessions = sessions_lock.write().await;
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
        let sessions_lock = state.host.sessions();
        let sessions = sessions_lock.read().await;
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
    let base_system_prompt = state.config.system_prompt.clone();
    // Common system-prompt assembly so the forked session matches every
    // other channel (project context + skill index + sub-agent note).
    let assembled_system_prompt = crate::assemble_system_prompt(
        &base_system_prompt,
        &state.config.workspace,
        &state.skills,
        state.config.subagent_enabled,
    );
    let system_prompt = assembled_system_prompt.full;
    let prompt_segments = assembled_system_prompt.segments;

    let tool_registry = state
        .session_tool_registry()
        .await
        .map_err(|e| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, e))?;
    let (system_prompt, prompt_segments) =
        inject_environment_segment(system_prompt, prompt_segments, &tool_registry);

    let mut runtime = build_session_runtime(
        &state,
        tool_registry,
        system_prompt,
        prompt_segments,
        state.config.max_steps,
    )
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

    state
        .host
        .sessions()
        .write()
        .await
        .insert(new_id.clone(), session);
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
    let sessions_lock = state.host.sessions();
    let sessions = sessions_lock.read().await;
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
    let sessions_lock = state.host.sessions();
    let sessions = sessions_lock.read().await;
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
        let sessions_lock = state.host.sessions();
        let sessions = sessions_lock.read().await;
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
        let sessions_lock = state.host.sessions();
        let sessions = sessions_lock.read().await;
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
///
/// Intentionally does NOT cold-load (Goal 397 allows read-only paths to skip
/// it): after a restart no turn is running, so there is nothing to cancel —
/// a restored session would only be built to discover a `None` token. An
/// unknown id stays 404.
pub(super) async fn session_interrupt(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let token_arc = {
        let sessions_lock = state.host.sessions();
        let sessions = sessions_lock.read().await;
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
// Pure clone of AppState field — no branching worth scoring.
#[cfg_attr(test, mutants::skip)]
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
    // Goal 397: get the session (cold-loading it from storage when it only
    // exists on disk), then grab its runtime, interrupt token, message
    // counter, last_active, and token usage counters. The returned handle
    // shares all mutable state with the table entry — no table lock held
    // while the turn runs.
    let session = super::cold_load::get_or_load_session(&state, &id).await?;
    // Update last_active_ms timestamp for this session.
    session
        .last_active_ms
        .store(super::now_session_ms(), Ordering::Relaxed);
    let (runtime_arc, interrupt_token_arc, msg_count_arc, prompt_tokens_arc, completion_tokens_arc) = (
        session.runtime.clone(),
        session.interrupt_token.clone(),
        session.non_system_message_count.clone(),
        session.prompt_tokens.clone(),
        session.completion_tokens.clone(),
    );

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

    // Acquire a run permit with a bounded wait (Goal 398): a saturated pool
    // now fails fast with 503 + Retry-After instead of hanging the request.
    let _permit = state
        .host
        .admission()
        .acquire_run()
        .await
        .map_err(|e| admission_error(e, &state.host.admission()))?;
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

    let outcome = run_result.map_err(|e| {
        record_run_failed(&state.metrics);
        map_run_error(&e)
    })?;

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
        let sessions_lock = state.host.sessions();
        let sessions = sessions_lock.read().await;
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

/// POST /agui — drive an agent run via the AG-UI protocol and stream
/// AG-UI events back as SSE.
///
/// Thin HTTP adapter (Issue #56): parse the JSON body, map transport-free
/// prepare/admission errors onto status codes, assemble the request-specific
/// inputs, then hand off to `super::agui` (`build_agui_runtime` +
/// `spawn_agui_run`) and frame the resulting event stream as SSE. All
/// session / persistence / protocol state machine logic lives in
/// `super::agui` and is unit-tested there without an HTTP server.
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

    // ── Transport-free prepare: resume/interrupt state machine ─────────
    let prepared = super::agui::prepare_run(super::agui::AguiRunInput {
        workspace: &state.config.workspace,
        input: &input,
    })
    .map_err(agui_prepare_error_response)?;

    // ── Per-thread run fence (issue #57 §④) ─────────────────────────────
    // At most one in-flight run per thread. Mobile retries / double
    // submits used to run two drivers concurrently against one transcript
    // (measured lost-update); refuse the second run instead of queueing
    // it — a queued duplicate would run the same prompt twice. The guard
    // is released when the driver task finishes (or on unwind).
    let run_guard = state
        .host
        .try_begin_run(crate::agui_session::thread_session_key(&input.thread_id))
        .ok_or_else(|| {
            (
                StatusCode::CONFLICT,
                Json(ErrorResponse {
                    status: "error".into(),
                    error: format!(
                        "a run is already active for thread '{}'; \
                         wait for it to finish before starting another",
                        input.thread_id
                    ),
                }),
            )
        })?;

    // Acquire a semaphore permit to limit concurrent runs.
    // Goal-H J2: /agui stays on the never-wait contract (`try_acquire_run`),
    // so a saturated semaphore returns immediately with a 503 (rather than
    // awaiting indefinitely, which would hang every /agui request when the
    // pool is full). Goal 398 routes it through the same admission gate as
    // the REST endpoints; only the waiting policy differs (none).
    // Issue #66: the permit now moves into the driver task — the run keeps
    // its admission slot (and `runs_in_flight` stays truthful) until the run
    // actually finishes, instead of releasing it when the handler returns
    // while the agent keeps running in the background.
    let permit = state.host.admission().try_acquire_run().map_err(|_| {
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
    let assembled_system_prompt = crate::assemble_system_prompt(
        &state.config.system_prompt,
        &state.config.workspace,
        &state.skills,
        state.config.subagent_enabled,
    );

    let tool_registry = state.session_tool_registry().await.map_err(|e| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                status: "error".into(),
                error: e,
            }),
        )
    })?;

    let (runtime, hooks) = super::agui::build_agui_runtime(
        &state.config.workspace,
        &input.thread_id,
        super::agui::AguiRuntimeDeps {
            llm: state.provider.clone(),
            tool_registry,
            system_prompt: assembled_system_prompt.full,
            prompt_segments: assembled_system_prompt.segments,
            max_steps: state.config.max_steps,
            seed_transcript: prepared.seed_transcript,
            interrupt_before: input.interrupt_before.as_deref().unwrap_or(&[]),
            client_tools: &input.tools,
            model: state.config.model.clone(),
        },
    )
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                status: "error".into(),
                error: format!("failed to build runtime: {e}"),
            }),
        )
    })?;

    let run_cancel = tokio_util::sync::CancellationToken::new();
    let sse_rx = super::agui::spawn_agui_run(
        runtime,
        prepared.goal,
        super::agui::AguiRunContext {
            thread_id: input.thread_id.clone(),
            run_id: input.run_id.clone(),
            client_tools: input.tools.clone(),
            hooks,
            workspace: state.config.workspace.clone(),
            metrics: state.metrics.clone(),
            model: state.config.model.clone(),
            provider: state.config.provider_type.clone(),
            preset: state.config.preset.clone(),
            cancel: run_cancel.clone(),
            permit,
            run_guard,
            active_runs: Arc::clone(&state.agui_active_runs),
        },
    );

    let stream =
        tokio_stream::wrappers::UnboundedReceiverStream::new(sse_rx).map(|ev| {
            let data = serde_json::to_string(&ev).unwrap_or_else(|_| "{}".into());
            Ok::<_, Infallible>(Event::default().data(data))
        });

    // Issue #66 §3.3 (approach A, AG-UI ecosystem convention): when the
    // client disconnects, hyper drops this response body — the wrapper's
    // `Drop` then cancels the run token. Normal completion drops it too,
    // but by then `runtime.run()` has returned and the token is inert.
    //
    // The 30s keep-alive bounds how long a disconnect can go unnoticed:
    // hyper only notices a dead socket on a write attempt, and a silent
    // tool-execution stretch would otherwise delay the cancel until the
    // next event. (axum emits the idle comment itself, so the stream still
    // ends the moment the driver drops `sse_tx` after RunFinished —
    // unlike a merged heartbeat interval, which would never end.)
    Ok(Sse::new(CancelOnDrop {
        inner: stream,
        token: Some(run_cancel),
    })
    .keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(30))
            .text("heartbeat"),
    ))
}

/// SSE body wrapper that cancels an in-flight AG-UI run when the response
/// stream is dropped (client disconnect). Issue #66 §3.3 approach A.
struct CancelOnDrop<S> {
    inner: S,
    token: Option<tokio_util::sync::CancellationToken>,
}

impl<S> futures_util::Stream for CancelOnDrop<S>
where
    S: futures_util::Stream + Unpin,
{
    type Item = S::Item;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.inner).poll_next(cx)
    }
}

impl<S> Drop for CancelOnDrop<S> {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            // Normal completion also drops the body — by then the driver has
            // finished `runtime.run()` and the token already fired, so only a
            // still-live token means the client actually went away.
            if !token.is_cancelled() {
                tracing::info!(
                    target: "recursive::http",
                    "agui: SSE stream dropped — cancelling in-flight run"
                );
                token.cancel();
            }
        }
    }
}

/// POST /agui/:thread_id/cancel — cancel the in-flight AG-UI run for a
/// thread. Issue #66 §3.3 approach B (explicit endpoint fallback): stream
/// drop (approach A) cannot fire when a network partition keeps the TCP
/// connection half-open, so clients get a direct stop button.
///
/// Idempotent like `session_interrupt`: an unknown thread or an
/// already-finished run answers `200 OK` with `"cancelled": false` —
/// stopping something that already stopped is not an error.
pub(super) async fn agui_cancel(
    State(state): State<Arc<AppState>>,
    Path(thread_id): Path<String>,
) -> Json<serde_json::Value> {
    let token = state
        .agui_active_runs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&thread_id)
        .cloned();
    let cancelled = token.is_some();
    if let Some(token) = token {
        tracing::info!(
            target: "recursive::http",
            thread_id = %thread_id,
            "agui: explicit cancel requested"
        );
        token.cancel();
    }
    Json(serde_json::json!({
        "status": "interrupted",
        "thread_id": thread_id,
        "cancelled": cancelled,
    }))
}

/// Map a transport-free [`super::agui::PrepareAguiError`] onto its HTTP
/// status + body.
fn agui_prepare_error_response(
    e: super::agui::PrepareAguiError,
) -> (StatusCode, Json<ErrorResponse>) {
    match e {
        super::agui::PrepareAguiError::InterruptBeforeConflict { thread_id, open } => (
            StatusCode::CONFLICT,
            Json(ErrorResponse {
                status: "error".into(),
                error: format!(
                    "thread '{thread_id}' has {open} open interrupt(s); \
                     must provide resume to continue"
                ),
            }),
        ),
        super::agui::PrepareAguiError::BadRequest(msg) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                status: "error".into(),
                error: msg,
            }),
        ),
        super::agui::PrepareAguiError::Internal(msg) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                status: "error".into(),
                error: msg,
            }),
        ),
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
    // Goal 398: queue visibility for the bounded admission gate.
    let runs_waiting = metrics.runs_waiting.load(Ordering::Relaxed);
    // Goal 392: runs currently holding a permit (RAII via RunPermit Drop).
    let runs_in_flight = metrics.runs_in_flight.load(Ordering::Relaxed);
    // Goal 392: aggregate transcript size across live sessions. Estimated
    // on demand by summing each message's `content` character count — a
    // UTF-8 byte total would need an O(n) encode walk per scrape anyway.
    // Sessions whose runtime mutex is contended (mid-turn) are skipped
    // rather than blocking the scrape or reporting a partial lie; the
    // skipped-session count is reported alongside so a stale-looking
    // total is explainable.
    let mut transcript_chars: u64 = 0;
    let mut transcript_bytes_skipped: u64 = 0;
    for session in state.host.sessions().read().await.values() {
        match session.runtime.try_lock() {
            Ok(rt) => {
                transcript_chars += rt
                    .transcript()
                    .iter()
                    .map(|m| m.content.chars().count() as u64)
                    .sum::<u64>();
            }
            Err(_) => transcript_bytes_skipped += 1,
        }
    }

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
         # HELP recursive_runs_waiting Requests waiting for a run permit\n\
         # TYPE recursive_runs_waiting gauge\n\
         recursive_runs_waiting {runs_waiting}\n\
         # HELP recursive_runs_in_flight Runs currently holding a run permit\n\
         # TYPE recursive_runs_in_flight gauge\n\
         recursive_runs_in_flight {runs_in_flight}\n\
         # HELP recursive_transcript_bytes_total Estimated transcript size across live sessions (character count; busy sessions skipped)\n\
         # TYPE recursive_transcript_bytes_total gauge\n\
         recursive_transcript_bytes_total {transcript_chars}\n\
         # HELP recursive_transcript_bytes_skipped Live sessions skipped when sampling transcript size (runtime busy)\n\
         # TYPE recursive_transcript_bytes_skipped gauge\n\
         recursive_transcript_bytes_skipped {transcript_bytes_skipped}\n\
         # HELP recursive_rate_limits_rejected_total Total requests rejected by rate limiting\n\
         # TYPE recursive_rate_limits_rejected_total counter\n\
         recursive_rate_limits_rejected_total {rate_limits_rejected}\n"
    )
}
