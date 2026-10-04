//! Issue #105 HTTP wiring: inbound triggers (cron scheduler + webhook
//! routes) and outbound notifications.
//!
//! Layering:
//! - `src/triggers.rs` / `src/notify.rs` are transport-free domain logic.
//! - This module is the ONLY axum-aware piece: route handlers, the
//!   per-trigger run fence (via `SessionHost::try_begin_run`), the cron
//!   scheduler task, and the post-run notify hook.
//!
//! Fire path (shared by cron and webhook):
//!   fire_trigger → build/resume session runtime → enqueue(goal) →
//!   notify (fire-and-forget) → advance_cron (cron only).
//!
//! Trigger fire runs are serialized per trigger id through the same
//! `SessionHost` fence `/agui` uses — a webhook retry cannot double-run
//! a still-running trigger.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;

use super::{ApiError, AppState, SessionState};
use crate::notify::NotifyTarget;
use crate::triggers::{Trigger, TriggerSpec, TriggerStore};

// ---------------------------------------------------------------------------
// API types
// ---------------------------------------------------------------------------

/// Request body for `POST /triggers`.
#[derive(serde::Deserialize, Debug)]
pub struct CreateTriggerRequest {
    /// Stable id. Omit to auto-generate (`trig-xxxxxxxx`).
    #[serde(default)]
    pub id: Option<String>,
    /// `"cron"` or `"webhook"`.
    pub kind: String,
    /// Cron expression (`kind: "cron"`), e.g. `"0 9 * * *"`.
    #[serde(default)]
    pub cron: Option<String>,
    /// Shared secret (`kind: "webhook"`). Auto-generated when omitted.
    #[serde(default)]
    pub secret: Option<String>,
    /// Goal text run when the trigger fires.
    pub goal: String,
    /// Resume this session instead of a one-shot run.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Optional outbound notification target.
    #[serde(default)]
    pub notify: Option<NotifyTarget>,
    /// Start enabled (default false — register, verify, then enable).
    #[serde(default)]
    pub enabled: bool,
}

/// Response body for trigger endpoints.
#[derive(serde::Serialize, Debug, Clone)]
pub struct TriggerResponse {
    pub id: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    pub goal: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notify: Option<NotifyTarget>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_fire_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_fired_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_result: Option<String>,
    pub enabled: bool,
    /// Webhook fire URL, path only (`/webhooks/{id}?key=...`). Cron
    /// triggers omit it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webhook_path: Option<String>,
}

impl TriggerResponse {
    fn from_trigger(t: &Trigger) -> Self {
        let (cron, webhook_path) = match &t.spec {
            TriggerSpec::Cron { expr } => (Some(expr.clone()), None),
            // (The webhook secret is never echoed in listings; the creator
            // sees it once in the create response — they chose it or it
            // was returned by this very call.)
            TriggerSpec::Webhook { .. } => (None, Some(webhook_path(t))),
        };
        Self {
            id: t.id.clone(),
            kind: t.spec.kind().to_string(),
            cron,
            secret: None,
            goal: t.goal.clone(),
            session_id: t.session_id.clone(),
            notify: t.notify.clone(),
            next_fire_at: t.next_fire_at.clone(),
            last_fired_at: t.last_fired_at.clone(),
            last_result: t.last_result.clone(),
            enabled: t.enabled,
            webhook_path,
        }
    }
}

/// The webhook invocation URL (path + key query) for a trigger.
fn webhook_path(t: &Trigger) -> String {
    let TriggerSpec::Webhook { secret } = &t.spec else {
        return String::new();
    };
    if secret.is_empty() {
        format!("/webhooks/{}", t.id)
    } else {
        format!("/webhooks/{}?key={}", t.id, secret)
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn store(state: &AppState) -> TriggerStore {
    TriggerStore::for_workspace(&state.config.workspace)
}

fn trigger_error(e: crate::error::Error) -> ApiError {
    ApiError::internal(e.to_string())
}

/// POST /triggers — register a cron or webhook trigger.
pub(super) async fn create_trigger(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateTriggerRequest>,
) -> Result<(StatusCode, Json<TriggerResponse>), ApiError> {
    if body.goal.trim().is_empty() {
        return Err(ApiError::bad_request("missing or empty 'goal' field"));
    }
    let id = match body.id {
        Some(id) if !id.trim().is_empty() => {
            if !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Err(ApiError::bad_request("trigger id must be [A-Za-z0-9_-]"));
            }
            id
        }
        _ => crate::triggers::generate_trigger_id(),
    };
    let spec = match body.kind.as_str() {
        "cron" => {
            let expr = body.cron.as_deref().unwrap_or_default();
            crate::triggers::parse_cron(expr)
                .map_err(|e| ApiError::bad_request(format!("invalid cron expr: {e}")))?;
            TriggerSpec::Cron {
                expr: expr.to_string(),
            }
        }
        "webhook" => TriggerSpec::Webhook {
            secret: body
                .secret
                .clone()
                .unwrap_or_else(crate::triggers::generate_secret),
        },
        other => {
            return Err(ApiError::bad_request(format!(
                "unknown trigger kind '{other}' (expected \"cron\" or \"webhook\")"
            )));
        }
    };
    // Surface a bad notify target at registration time, not at 9am.
    if let Some(NotifyTarget::File { path }) = &body.notify {
        // Best-effort validation: the process context is bound at server
        // startup, so the check is authoritative here.
        validate_notify_path(&state, path)?;
    }
    let mut trigger = Trigger::new(id, spec, body.goal, body.session_id, body.notify);
    trigger.enabled = body.enabled;
    // Compute the first fire time immediately so the caller can echo it.
    if let TriggerSpec::Cron { expr } = &trigger.spec {
        if trigger.next_fire_at.is_none() {
            match crate::triggers::next_after(expr, crate::triggers::epoch_now()) {
                Some(next) => {
                    trigger.next_fire_at = Some(crate::triggers::format_rfc3339_utc(next));
                }
                None => {
                    return Err(ApiError::bad_request(
                        "cron expression never fires within 4 years",
                    ));
                }
            }
        }
    }
    store(&state)
        .upsert(trigger.clone())
        .map_err(trigger_error)?;
    // Echo the webhook secret ONCE in the create response.
    let mut resp = TriggerResponse::from_trigger(&trigger);
    if let TriggerSpec::Webhook { secret } = &trigger.spec {
        resp.secret = Some(secret.clone());
    }
    tracing::info!(trigger_id = %trigger.id, kind = %trigger.spec.kind(), "trigger created");
    Ok((StatusCode::CREATED, Json(resp)))
}

/// Validate a file notify target against the process sandbox root.
fn validate_notify_path(state: &AppState, path: &std::path::Path) -> Result<(), ApiError> {
    // Bind (idempotent) and check — reuses the notifier's containment
    // rules so registration and delivery cannot disagree.
    crate::notify::set_file_context(&state.config.workspace);
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Direct containment check via the same helper the notifier uses.
        crate::notify::file_target_allowed_for_validation(path)
    })) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(ApiError::bad_request(e.to_string())),
        Err(_) => Err(ApiError::internal("notify path validation panicked")),
    }
}

/// GET /triggers — list all registered triggers.
pub(super) async fn list_triggers(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<TriggerResponse>>, ApiError> {
    let all = store(&state).load().map_err(trigger_error)?;
    Ok(Json(
        all.iter().map(TriggerResponse::from_trigger).collect(),
    ))
}

/// GET /triggers/:id — one trigger.
pub(super) async fn get_trigger(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<TriggerResponse>, ApiError> {
    match store(&state).get(&id).map_err(trigger_error)? {
        Some(t) => Ok(Json(TriggerResponse::from_trigger(&t))),
        None => Err(ApiError::not_found(format!("trigger {id} not found"))),
    }
}

/// DELETE /triggers/:id — remove a trigger.
pub(super) async fn delete_trigger(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    match store(&state).delete(&id).map_err(trigger_error)? {
        true => Ok(StatusCode::NO_CONTENT),
        false => Err(ApiError::not_found(format!("trigger {id} not found"))),
    }
}

/// PATCH /triggers/:id — enable/disable or re-goal a trigger.
#[derive(serde::Deserialize, Debug, Default)]
pub struct PatchTriggerRequest {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub goal: Option<String>,
}

pub(super) async fn patch_trigger(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<PatchTriggerRequest>,
) -> Result<Json<TriggerResponse>, ApiError> {
    let s = store(&state);
    let mut all = s.load().map_err(trigger_error)?;
    let Some(trigger) = all.iter_mut().find(|t| t.id == id) else {
        return Err(ApiError::not_found(format!("trigger {id} not found")));
    };
    if let Some(enabled) = body.enabled {
        trigger.enabled = enabled;
        // Re-enabling always recomputes the next window: a cron that sat
        // disabled for days must not instantly fire its stale one. (An
        // already-fired trigger already points at a future window, which
        // `next_after(now)` reproduces.)
        if enabled {
            if let TriggerSpec::Cron { expr } = &trigger.spec {
                if let Some(next) = crate::triggers::next_after(expr, crate::triggers::epoch_now())
                {
                    trigger.next_fire_at = Some(crate::triggers::format_rfc3339_utc(next));
                }
            }
        }
    }
    if let Some(goal) = body.goal {
        if goal.trim().is_empty() {
            return Err(ApiError::bad_request("'goal' must not be empty"));
        }
        trigger.goal = goal;
    }
    let updated = trigger.clone();
    s.save(&all).map_err(trigger_error)?;
    Ok(Json(TriggerResponse::from_trigger(&updated)))
}

/// Query params for `POST /webhooks/:id` (`?key=...`).
#[derive(serde::Deserialize, Debug, Default)]
pub struct WebhookKeyQuery {
    #[serde(default)]
    pub key: Option<String>,
}

/// POST /webhooks/:id — fire a webhook trigger.
///
/// 202 Accepted: the run was queued as a fire-and-forget task (the caller
/// does not block for the agent run). 401 on key mismatch, 404 unknown id,
/// 409 when a run for this trigger is already in flight, 409-disabled when
/// the trigger is registered but disabled.
pub(super) async fn fire_webhook(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<WebhookKeyQuery>,
    body: axum::body::Bytes,
) -> Result<axum::response::Response, ApiError> {
    let s = store(&state);
    let trigger = s
        .get(&id)
        .map_err(trigger_error)?
        .ok_or_else(|| ApiError::not_found(format!("trigger {id} not found")))?;
    let TriggerSpec::Webhook { secret } = &trigger.spec else {
        return Err(ApiError::bad_request(format!(
            "trigger {id} is a cron trigger; it fires on schedule"
        )));
    };
    if !crate::triggers::webhook_key_matches(secret, query.key.as_deref()) {
        return Err(ApiError::unauthorized("invalid webhook key"));
    }
    if !trigger.enabled {
        return Err(ApiError::conflict(format!(
            "trigger {id} is disabled; PATCH /triggers/{id} {{\"enabled\":true}} first"
        )));
    }
    // Take the per-trigger fence HERE so an in-flight run is a 409 to the
    // (non-blocking) caller rather than a lost fire.
    let run_guard = begin_trigger_run(&state, &trigger.id)?;
    let state = Arc::clone(&state);
    let trigger_id = trigger.id.clone();
    let payload = (!body.is_empty()).then_some(body);
    tokio::spawn(async move {
        tracing::debug!(trigger_id = %trigger_id, "webhook fire started");
        fire_trigger(&state, trigger, payload, run_guard).await;
    });
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "status": "accepted",
            "trigger_id": id,
        })),
    )
        .into_response())
}

/// Acquire the per-trigger run fence (`trigger:<id>`), mapping "already
/// running" onto the 409 the webhook contract documents.
fn begin_trigger_run(
    state: &Arc<AppState>,
    id: &str,
) -> Result<crate::session_host::ActiveRunGuard, ApiError> {
    state
        .host
        .try_begin_run(format!("trigger:{id}"))
        .ok_or_else(|| ApiError::conflict(format!("trigger {id} already has a run in flight")))
}

// ---------------------------------------------------------------------------
// Fire path (shared by cron + webhook)
// ---------------------------------------------------------------------------

/// Fire one trigger: build/resume the session, enqueue the goal, notify.
///
/// Runs inside a spawned task; failures are logged into
/// `Trigger::last_result` and never propagated to a client. `_run_guard` is
/// the caller-acquired per-trigger fence — it keeps the trigger marked
/// in-flight for the whole run and releases it on drop.
async fn fire_trigger(
    state: &Arc<AppState>,
    trigger: Trigger,
    webhook_payload: Option<axum::body::Bytes>,
    _run_guard: crate::session_host::ActiveRunGuard,
) {
    let source = format!("{}:{}", trigger.spec.kind(), trigger.id);
    let mut goal = trigger.goal.clone();
    if let Some(bytes) = webhook_payload {
        if !bytes.is_empty() {
            // Forward the webhook body as context after the goal. Kept
            // bounded — a webhook payload is context, not a transcript.
            let text = String::from_utf8_lossy(&bytes);
            let snippet: String = text.chars().take(4000).collect();
            goal.push_str("\n\nWebhook payload:\n");
            goal.push_str(&snippet);
        }
    }

    let (session_id, outcome_text, finish_reason) =
        run_trigger_goal(state, trigger.session_id.as_deref(), &goal).await;

    // Record + notify. Webhooks stamp last_result here too (they are not
    // scheduler-advanced) so GET /triggers shows delivery outcome.
    let result = if let Some(notify_target) = trigger.notify.clone() {
        let payload = crate::notify::NotifyPayload {
            session_id: &session_id,
            source: &source,
            finish_reason: &finish_reason,
            final_text: outcome_text.as_deref(),
        };
        crate::notify::notify_best_effort(
            &crate::notify::HttpNotifier::new(),
            &notify_target,
            &payload,
        )
    } else {
        format!("run finished: {finish_reason}")
    };
    stamp_result(store(state), &trigger, result);
}

/// Record `result` on the trigger without moving its schedule.
///
/// The scheduler advances the cron window itself (before spawning the
/// fire); the fire path only records the outcome. Keeping the two apart
/// means a slow run cannot push the next window — the schedule is decided
/// by the tick, not by the run's duration.
fn stamp_result(s: TriggerStore, trigger: &Trigger, result: String) {
    if let Ok(mut all) = s.load() {
        if let Some(t) = all.iter_mut().find(|t| t.id == trigger.id) {
            t.last_fired_at = Some(crate::triggers::format_rfc3339_utc(
                crate::triggers::epoch_now(),
            ));
            t.last_result = Some(result);
        }
        let _ = s.save(&all);
    }
}

/// Run the trigger's goal: resume `session_id` when given, else a one-shot
/// in-memory session (transcript persisted through the storage backend
/// under a `trigger-<id>` key so it remains inspectable).
///
/// Returns (session_id, final_text, finish_reason).
async fn run_trigger_goal(
    state: &Arc<AppState>,
    session_id: Option<&str>,
    goal: &str,
) -> (String, Option<String>, String) {
    match session_id {
        Some(id) => match super::cold_load::get_or_load_session(state, id).await {
            Ok(session) => run_in_session(state, &session, goal).await,
            Err(e) => {
                tracing::warn!(
                    session_id = %id,
                    error = %e.message,
                    "trigger: configured session unavailable; falling back to one-shot run"
                );
                one_shot_run(state, goal).await
            }
        },
        None => one_shot_run(state, goal).await,
    }
}

/// Run one turn inside an existing session (the `send_session_message`
/// flow, minus SSE plumbing).
async fn run_in_session(
    state: &Arc<AppState>,
    session: &Arc<SessionState>,
    goal: &str,
) -> (String, Option<String>, String) {
    session
        .last_active_ms
        .store(super::now_session_ms(), Ordering::Relaxed);
    let _permit = match state.host.admission().acquire_run().await {
        Ok(p) => p,
        Err(e) => {
            return (session.id.clone(), None, format!("admission failed: {e:?}"));
        }
    };
    let mut runtime = session.runtime.lock().await;
    let (final_text, reason) = match runtime.enqueue(goal).await {
        Ok(Some(o)) => (o.final_text, o.finish_reason.to_string()),
        Ok(None) => (None, "NoMoreToolCalls".to_string()),
        Err(e) => (None, format!("error: {e}")),
    };
    (session.id.clone(), final_text, reason)
}

/// One-shot run for triggers without a session. The transcript is
/// persisted under a deterministic `trigger-<id>-<timestamp>` key so
/// post-hoc inspection works; the in-memory session is not registered.
async fn one_shot_run(state: &Arc<AppState>, goal: &str) -> (String, Option<String>, String) {
    let _permit = match state.host.admission().acquire_run().await {
        Ok(p) => p,
        Err(e) => return (String::new(), None, format!("admission failed: {e:?}")),
    };
    let assembled = crate::assemble_system_prompt(
        &state.config.system_prompt,
        &state.config.workspace,
        &state.skills,
        state.config.subagent_enabled,
    );
    let tool_registry = match state.session_tool_registry().await {
        Ok(r) => r,
        Err(e) => {
            return (String::new(), None, format!("registry build failed: {e}"));
        }
    };
    let (system_prompt, prompt_segments) = super::handlers::inject_environment_segment(
        assembled.full,
        assembled.segments,
        &tool_registry,
    );
    let mut runtime = match super::handlers::build_session_runtime_parts(
        tool_registry,
        system_prompt,
        prompt_segments,
        state.config.max_steps,
        &state.config.model,
    )
    .llm(state.provider.clone())
    .wall_timeout_secs(state.config.wall_timeout_secs)
    .storage(state.storage.clone())
    .build()
    {
        Ok(rt) => rt,
        Err(e) => return (String::new(), None, format!("runtime build failed: {e}")),
    };
    let outcome = match runtime.run(goal).await {
        Ok(o) => o,
        Err(e) => {
            runtime.destroy_environment().await;
            return (String::new(), None, format!("error: {e}"));
        }
    };
    runtime.destroy_environment().await;
    let finish = outcome.finish_reason.to_string();
    let final_text = outcome.final_text.clone();
    // Best-effort transcript persistence under a trigger-namespaced key.
    let key = format!("trigger-run/{}", uuid::Uuid::now_v7());
    let transcript = runtime.transcript().to_vec();
    if let Err(e) = state.storage.save_transcript(&key, &transcript).await {
        tracing::warn!(key = %key, error = %e, "failed to persist trigger run transcript");
    }
    (key, final_text, finish)
}

// ---------------------------------------------------------------------------
// Cron scheduler task
// ---------------------------------------------------------------------------

/// Spawn the cron scheduler: every `tick` it scans enabled cron triggers
/// whose `next_fire_at <= now` and fires each one on its own task.
///
/// `spawn_trigger_run` is injected so tests can observe fires without
/// building a full server runtime.
pub fn spawn_trigger_scheduler(
    state: Arc<AppState>,
    tick: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tick);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let s = TriggerStore::for_workspace(&state.config.workspace);
            let Ok(all) = s.load() else {
                continue;
            };
            let now = crate::triggers::epoch_now();
            for trigger in all.iter().filter(|t| t.is_due(now)) {
                // Consume the due window IMMEDIATELY (before spawning) so a
                // slow fire cannot re-arm on the next tick: the schedule
                // collapses to the first window after now, and the spawned
                // fire only stamps `last_result` (see `stamp_result`).
                if let Err(e) = s.advance_cron(&trigger.id, "firing") {
                    tracing::warn!(trigger_id = %trigger.id, error = %e, "scheduler: advance failed");
                    continue;
                }
                // Same fence the webhook path takes: a fire still in
                // flight for this trigger skips the window instead of
                // starting a second concurrent run.
                let Some(run_guard) = state.host.try_begin_run(format!("trigger:{}", trigger.id))
                else {
                    tracing::warn!(
                        trigger_id = %trigger.id,
                        "scheduler: run already in flight; skipping this window"
                    );
                    continue;
                };
                let state = Arc::clone(&state);
                let trigger = trigger.clone();
                tokio::spawn(async move {
                    fire_trigger(&state, trigger, None, run_guard).await;
                });
            }
        }
    })
}

// ---------------------------------------------------------------------------
// OpenAPI supplement (kept here to avoid bloating build_openapi_spec)
// ---------------------------------------------------------------------------

/// Extend the base OpenAPI spec with the trigger endpoints. Called from
/// `build_openapi_spec`.
pub fn trigger_openapi_paths() -> serde_json::Value {
    serde_json::json!({
        "/triggers": {
            "get": {
                "summary": "List triggers",
                "description": "All registered cron/webhook triggers with their schedule state.",
                "responses": {
                    "200": {
                        "description": "Array of triggers",
                        "content": {
                            "application/json": {
                                "schema": {
                                    "type": "array",
                                    "items": { "$ref": "#/components/schemas/TriggerResponse" }
                                }
                            }
                        }
                    }
                }
            },
            "post": {
                "summary": "Create a trigger",
                "description": "Register a cron (`kind: \"cron\"`, `cron: \"0 9 * * *\"`) or webhook (`kind: \"webhook\"`) trigger that runs `goal` (optionally resuming `session_id`, optionally notifying `notify`).",
                "requestBody": {
                    "required": true,
                    "content": {
                        "application/json": {
                            "schema": { "$ref": "#/components/schemas/CreateTriggerRequest" }
                        }
                    }
                },
                "responses": {
                    "201": {
                        "description": "Trigger created (webhook secret echoed once)",
                        "content": {
                            "application/json": {
                                "schema": { "$ref": "#/components/schemas/TriggerResponse" }
                            }
                        }
                    },
                    "400": { "description": "Invalid cron expression / goal / kind" }
                }
            }
        },
        "/triggers/{id}": {
            "get": {
                "summary": "Get a trigger",
                "parameters": [{ "$ref": "#/components/parameters/TriggerId" }],
                "responses": {
                    "200": {
                        "description": "The trigger",
                        "content": {
                            "application/json": {
                                "schema": { "$ref": "#/components/schemas/TriggerResponse" }
                            }
                        }
                    },
                    "404": { "description": "Unknown trigger id" }
                }
            },
            "patch": {
                "summary": "Update a trigger (enable/disable, re-goal)",
                "parameters": [{ "$ref": "#/components/parameters/TriggerId" }],
                "requestBody": {
                    "required": true,
                    "content": {
                        "application/json": {
                            "schema": { "$ref": "#/components/schemas/PatchTriggerRequest" }
                        }
                    }
                },
                "responses": {
                    "200": {
                        "description": "Updated trigger",
                        "content": {
                            "application/json": {
                                "schema": { "$ref": "#/components/schemas/TriggerResponse" }
                            }
                        }
                    },
                    "404": { "description": "Unknown trigger id" }
                }
            },
            "delete": {
                "summary": "Delete a trigger",
                "parameters": [{ "$ref": "#/components/parameters/TriggerId" }],
                "responses": {
                    "204": { "description": "Deleted" },
                    "404": { "description": "Unknown trigger id" }
                }
            }
        },
        "/webhooks/{id}": {
            "post": {
                "summary": "Fire a webhook trigger",
                "description": "Invoke a registered webhook trigger. Pass the secret as `?key=`. Returns 202 when the run was queued; the goal is augmented with the request body (first 4000 chars).",
                "parameters": [
                    { "$ref": "#/components/parameters/TriggerId" },
                    {
                        "name": "key",
                        "in": "query",
                        "required": false,
                        "schema": { "type": "string" },
                        "description": "Webhook secret (omit for secretless local triggers)."
                    }
                ],
                "responses": {
                    "202": { "description": "Run queued" },
                    "401": { "description": "Invalid or missing key" },
                    "404": { "description": "Unknown trigger id" },
                    "409": { "description": "Trigger disabled or a run already in flight" }
                }
            }
        }
    })
}

/// OpenAPI schema components contributed by the trigger endpoints.
pub fn trigger_openapi_schemas() -> serde_json::Map<String, serde_json::Value> {
    let mut m = serde_json::Map::new();
    m.insert(
        "CreateTriggerRequest".to_string(),
        serde_json::json!({
            "type": "object",
            "required": ["kind", "goal"],
            "properties": {
                "id": { "type": "string", "description": "Stable id; auto-generated when omitted." },
                "kind": { "type": "string", "enum": ["cron", "webhook"] },
                "cron": { "type": "string", "description": "Cron expression (kind=cron): \"min hour dom month dow\"" },
                "secret": { "type": "string", "description": "Webhook shared secret (kind=webhook); auto-generated when omitted." },
                "goal": { "type": "string" },
                "session_id": { "type": "string", "description": "Resume this session instead of a one-shot run." },
                "notify": { "$ref": "#/components/schemas/NotifyTarget" },
                "enabled": { "type": "boolean", "default": false }
            }
        }),
    );
    m.insert(
        "PatchTriggerRequest".to_string(),
        serde_json::json!({
            "type": "object",
            "properties": {
                "enabled": { "type": "boolean" },
                "goal": { "type": "string" }
            }
        }),
    );
    m.insert(
        "TriggerResponse".to_string(),
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string" },
                "kind": { "type": "string", "enum": ["cron", "webhook"] },
                "cron": { "type": "string" },
                "secret": { "type": "string", "description": "Only echoed in the create response." },
                "goal": { "type": "string" },
                "session_id": { "type": "string" },
                "notify": { "$ref": "#/components/schemas/NotifyTarget" },
                "next_fire_at": { "type": "string" },
                "last_fired_at": { "type": "string" },
                "last_result": { "type": "string" },
                "enabled": { "type": "boolean" },
                "webhook_path": { "type": "string", "description": "Relative fire URL including ?key= for secretless path building." }
            }
        }),
    );
    m.insert(
        "NotifyTarget".to_string(),
        serde_json::json!({
            "type": "object",
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "kind": { "const": "webhook" },
                        "url": { "type": "string" },
                        "secret": { "type": "string", "description": "When set, X-Recursive-Signature: blake3-keyed(secret, body) is attached." }
                    },
                    "required": ["kind", "url"]
                },
                {
                    "type": "object",
                    "properties": {
                        "kind": { "const": "file" },
                        "path": { "type": "string", "description": "JSONL sink under the server's user-data dir." }
                    },
                    "required": ["kind", "path"]
                }
            ]
        }),
    );
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_response_round_trips_webhook_path() {
        let mut t = Trigger::new(
            "trig-x",
            TriggerSpec::Webhook {
                secret: "abc".into(),
            },
            "g",
            None,
            None,
        );
        t.enabled = true;
        let resp = TriggerResponse::from_trigger(&t);
        assert_eq!(
            resp.webhook_path.as_deref(),
            Some("/webhooks/trig-x?key=abc")
        );
        assert!(resp.secret.is_none(), "listing must not echo the secret");
        assert_eq!(resp.kind, "webhook");

        let cron = Trigger::new(
            "trig-c",
            TriggerSpec::Cron {
                expr: "0 9 * * *".into(),
            },
            "g",
            None,
            None,
        );
        let resp = TriggerResponse::from_trigger(&cron);
        assert_eq!(resp.cron.as_deref(), Some("0 9 * * *"));
        assert!(resp.webhook_path.is_none());
    }

    /// The create handler rejects malformed input before touching the
    /// store — pinned against the request struct.
    #[test]
    fn create_trigger_request_defaults() {
        let body: CreateTriggerRequest =
            serde_json::from_str(r#"{"kind":"cron","goal":"x"}"#).expect("parse");
        assert!(body.id.is_none());
        assert!(body.cron.is_none());
        assert!(!body.enabled, "triggers default to disabled");
        assert!(body.notify.is_none());
        assert!(body.session_id.is_none());
    }

    #[test]
    fn patch_trigger_request_defaults() {
        let body: PatchTriggerRequest = serde_json::from_str(r#"{}"#).expect("parse");
        assert!(body.enabled.is_none());
        assert!(body.goal.is_none());
    }

    #[test]
    fn webhook_query_defaults() {
        let q: WebhookKeyQuery = serde_json::from_str("{}").expect("parse");
        assert!(q.key.is_none());
    }

    #[test]
    fn trigger_openapi_documents_all_endpoints() {
        let paths = trigger_openapi_paths();
        assert!(paths.get("/triggers").is_some());
        assert!(paths.get("/triggers/{id}").is_some());
        assert!(paths.get("/webhooks/{id}").is_some());
        let schemas = trigger_openapi_schemas();
        for name in [
            "CreateTriggerRequest",
            "PatchTriggerRequest",
            "TriggerResponse",
            "NotifyTarget",
        ] {
            assert!(schemas.contains_key(name), "missing schema {name}");
        }
    }
}
