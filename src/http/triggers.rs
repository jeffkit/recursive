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

use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;

use super::{ApiError, AppState, AuthIdentity, SessionState};
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
    /// Webhook fire path (`/webhooks/{id}`). Cron triggers omit it. The
    /// keyed form (`?key=...`) is returned **once**, by the create call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webhook_path: Option<String>,
}

impl TriggerResponse {
    fn from_trigger(t: &Trigger) -> Self {
        let (cron, webhook_path) = match &t.spec {
            TriggerSpec::Cron { expr } => (Some(expr.clone()), None),
            // Listing / get / patch never carry the secret, so the path
            // they return is the keyless form; `create_trigger` re-adds
            // both the secret and the keyed path to its own response.
            TriggerSpec::Webhook { .. } => (None, Some(format!("/webhooks/{}", t.id))),
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

/// The keyed webhook invocation URL (`/webhooks/{id}?key=...`). Only the
/// create response carries this; every other response uses the keyless
/// path from [`TriggerResponse::from_trigger`].
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

/// Issue #85: assert the caller may read or mutate this trigger.
///
/// A trigger runs agent work server-side (into a session, as an admin
/// identity), so managing one is managing someone's scheduled work: a foreign
/// caller must not be able to re-goal another caller's trigger and have it run
/// inside that caller's session. Checked before any mutation.
fn ensure_trigger_access(identity: &AuthIdentity, trigger: &Trigger) -> Result<(), ApiError> {
    if identity.may_access_session(trigger.owner.as_deref(), trigger.tenant.as_deref()) {
        Ok(())
    } else {
        Err(ApiError::forbidden("trigger belongs to another identity"))
    }
}

/// POST /triggers — register a cron or webhook trigger.
///
/// Issue #85: a trigger that resumes `session_id` runs turns in that session
/// server-side (as an admin identity), so registration is ownership-asserted —
/// otherwise any caller could point a trigger at another caller's session.
pub(super) async fn create_trigger(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AuthIdentity>,
    Json(body): Json<CreateTriggerRequest>,
) -> Result<(StatusCode, Json<TriggerResponse>), ApiError> {
    if body.goal.trim().is_empty() {
        return Err(ApiError::bad_request("missing or empty 'goal' field"));
    }
    if let Some(session_id) = body.session_id.as_deref() {
        super::handlers::ensure_session_access_by_id(&state, session_id, &identity).await?;
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
    if let Some(target) = &body.notify {
        match target {
            // Best-effort validation: the process context is bound at
            // server startup, so the check is authoritative here.
            NotifyTarget::File { path } => validate_notify_path(&state, path)?,
            NotifyTarget::Webhook { url, .. } => validate_notify_webhook_url(url)?,
        }
    }
    let mut trigger = Trigger::new(id, spec, body.goal, body.session_id, body.notify);
    trigger.enabled = body.enabled;
    // Issue #85: the registrant owns the trigger — the caller that created it
    // is the only one that may later read, re-goal or remove it.
    trigger.owner = Some(identity.subject.clone());
    trigger.tenant = identity.tenant.clone();
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
    // Echo the webhook secret ONCE in the create response, together with
    // the keyed fire path the caller needs to configure its sender.
    let mut resp = TriggerResponse::from_trigger(&trigger);
    if let TriggerSpec::Webhook { secret } = &trigger.spec {
        resp.secret = Some(secret.clone());
        resp.webhook_path = Some(webhook_path(&trigger));
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

/// Validate a webhook notify target's URL: absolute and `http(s)`.
///
/// A typo (`"not a url"`, `"example.com/hook"`) or a non-HTTP scheme would
/// otherwise only surface hours later as a fire-time delivery error.
///
/// The SSRF guard `WebFetch` applies is deliberately **not** reused here:
/// notify targets are supplied by the authenticated caller of
/// `POST /triggers` (who can run arbitrary commands through a session
/// anyway), and a self-hosted receiver on loopback or the LAN is the normal
/// deployment — the same distinction `HttpCall` draws when it puts private
/// endpoints behind an explicit `allow_private` opt-in.
fn validate_notify_webhook_url(url: &str) -> Result<(), ApiError> {
    let parsed = url::Url::parse(url)
        .map_err(|e| ApiError::bad_request(format!("invalid notify webhook url '{url}': {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ApiError::bad_request(format!(
            "notify webhook url must be http:// or https://, got '{}'",
            parsed.scheme()
        )));
    }
    Ok(())
}

/// GET /triggers — list the caller's registered triggers.
///
/// Issue #85: a trigger carries the goal it will run and the session it will
/// run in, so the list is scoped to the caller (admins see every trigger).
pub(super) async fn list_triggers(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AuthIdentity>,
) -> Result<Json<Vec<TriggerResponse>>, ApiError> {
    let all = store(&state).load().map_err(trigger_error)?;
    Ok(Json(
        all.iter()
            .filter(|t| identity.may_access_session(t.owner.as_deref(), t.tenant.as_deref()))
            .map(TriggerResponse::from_trigger)
            .collect(),
    ))
}

/// GET /triggers/:id — one trigger.
pub(super) async fn get_trigger(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AuthIdentity>,
    Path(id): Path<String>,
) -> Result<Json<TriggerResponse>, ApiError> {
    match store(&state).get(&id).map_err(trigger_error)? {
        Some(t) => {
            ensure_trigger_access(&identity, &t)?;
            Ok(Json(TriggerResponse::from_trigger(&t)))
        }
        None => Err(ApiError::not_found(format!("trigger {id} not found"))),
    }
}

/// DELETE /triggers/:id — remove a trigger.
pub(super) async fn delete_trigger(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AuthIdentity>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let s = store(&state);
    let Some(trigger) = s.get(&id).map_err(trigger_error)? else {
        return Err(ApiError::not_found(format!("trigger {id} not found")));
    };
    ensure_trigger_access(&identity, &trigger)?;
    match s.delete(&id).map_err(trigger_error)? {
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
    Extension(identity): Extension<AuthIdentity>,
    Path(id): Path<String>,
    Json(body): Json<PatchTriggerRequest>,
) -> Result<Json<TriggerResponse>, ApiError> {
    let s = store(&state);
    let mut all = s.load().map_err(trigger_error)?;
    let Some(trigger) = all.iter_mut().find(|t| t.id == id) else {
        return Err(ApiError::not_found(format!("trigger {id} not found")));
    };
    ensure_trigger_access(&identity, trigger)?;
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
///
/// Issue #85: firing runs the trigger's goal in its session as an admin
/// identity, so it is ownership-asserted like the management routes — the
/// `?key=` secret authenticates the *request* to the trigger, not the caller
/// to the server, and possession of a leaked id must not be enough to drive
/// someone else's scheduled run. Both credentials are required.
pub(super) async fn fire_webhook(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AuthIdentity>,
    Path(id): Path<String>,
    Query(query): Query<WebhookKeyQuery>,
    body: axum::body::Bytes,
) -> Result<axum::response::Response, ApiError> {
    let s = store(&state);
    let trigger = s
        .get(&id)
        .map_err(trigger_error)?
        .ok_or_else(|| ApiError::not_found(format!("trigger {id} not found")))?;
    ensure_trigger_access(&identity, &trigger)?;
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

    let (session_id, outcome_text, finish_reason) = run_trigger_goal(
        state,
        trigger.session_id.as_deref(),
        trigger.tenant.as_deref(),
        &goal,
    )
    .await;

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
            crate::notify::shared_notifier(),
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
    // Both failures are logged rather than silently dropped: a corrupt or
    // unwritable store is exactly the state where "did my 9am run go out?"
    // must still be answerable from the logs.
    let mut all = match s.load() {
        Ok(all) => all,
        Err(e) => {
            tracing::warn!(
                trigger_id = %trigger.id,
                result = %result,
                error = %e,
                "trigger fire finished but its result could not be recorded"
            );
            return;
        }
    };
    if let Some(t) = all.iter_mut().find(|t| t.id == trigger.id) {
        t.last_fired_at = Some(crate::triggers::format_rfc3339_utc(
            crate::triggers::epoch_now(),
        ));
        t.last_result = Some(result);
    }
    if let Err(e) = s.save(&all) {
        tracing::warn!(
            trigger_id = %trigger.id,
            error = %e,
            "failed to persist trigger last_result"
        );
    }
}

/// Run the trigger's goal: resume `session_id` when given, else a one-shot
/// in-memory session (transcript persisted through the storage backend
/// under a `trigger-<id>` key so it remains inspectable).
///
/// Issue #85: a trigger is server-side work with no inbound request, so it
/// runs as [`AuthIdentity::local`] (an admin identity) — a scheduled turn in a
/// configured session is not scoped to the session's owner.
///
/// Returns (session_id, final_text, finish_reason).
async fn run_trigger_goal(
    state: &Arc<AppState>,
    session_id: Option<&str>,
    tenant: Option<&str>,
    goal: &str,
) -> (String, Option<String>, String) {
    // Goal 431 (#244 W2a): the work belongs to the trigger's owner, so the
    // one-shot catalog is partitioned by the trigger's own labels. (The
    // session-bound path reuses the session's runtime, which was already
    // partitioned at creation.)
    let labels = super::handlers::session_labels(tenant);
    match session_id {
        Some(id) => {
            match super::cold_load::get_or_load_session(state, id, &AuthIdentity::local()).await {
                Ok(session) => run_in_session(state, &session, goal).await,
                Err(e) => {
                    tracing::warn!(
                        session_id = %id,
                        error = %e.message,
                        "trigger: configured session unavailable; falling back to one-shot run"
                    );
                    one_shot_run(state, goal, &labels).await
                }
            }
        }
        None => one_shot_run(state, goal, &labels).await,
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
    let _permit = match super::handlers::acquire_run_timed(state).await {
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
async fn one_shot_run(
    state: &Arc<AppState>,
    goal: &str,
    labels: &super::labels::OpaqueLabels,
) -> (String, Option<String>, String) {
    let _permit = match super::handlers::acquire_run_timed(state).await {
        Ok(p) => p,
        Err(e) => return (String::new(), None, format!("admission failed: {e:?}")),
    };
    let skills = match state.session_skill_catalog(labels).await {
        Ok(s) => s,
        Err(e) => return (String::new(), None, format!("skill catalog failed: {e}")),
    };
    let assembled = crate::assemble_system_prompt(
        &state.config.system_prompt,
        &state.config.workspace,
        &skills,
        state.config.subagent_enabled,
    );
    let tool_registry = match state.session_tool_registry(labels).await {
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
    // Issue #127: same preset assembly as every other channel. A trigger run
    // has no client to report a 400 to, so an unknown `RECURSIVE_AGENT_PRESET`
    // (a server misconfiguration) falls back to the built-in standard preset.
    let preset =
        super::handlers::resolve_session_preset(None, &state.config).unwrap_or_else(|_| {
            crate::preset::STANDARD
                .resolve(&state.config, &crate::preset::PresetEnv::from_process())
        });
    let mut runtime = match super::handlers::build_session_runtime_parts(
        tool_registry,
        system_prompt,
        prompt_segments,
        state.config.max_steps,
        &preset,
        skills.as_ref().clone(),
        super::handlers::HTTP_CHANNEL,
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
            let all = match s.load() {
                Ok(all) => all,
                // Keep ticking (a transient read error must not kill the
                // scheduler) but say so out loud: without this line a
                // corrupt `triggers.json` looks like "my 9am run simply
                // never happened".
                Err(e) => {
                    tracing::warn!(error = %e, "scheduler: cannot read trigger store; skipping tick");
                    continue;
                }
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
                    "400": { "description": "Invalid cron expression / goal / kind / notify target" }
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
                "webhook_path": { "type": "string", "description": "Relative fire path (/webhooks/{id}). The create response instead carries the keyed form (/webhooks/{id}?key=...), which is the only place the secret is returned." }
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
                        "kind": { "type": "string", "enum": ["webhook"] },
                        "url": { "type": "string" },
                        "secret": { "type": "string", "description": "When set, X-Recursive-Signature carries hex(blake3_keyed(blake3(secret), body))." }
                    },
                    "required": ["kind", "url"]
                },
                {
                    "type": "object",
                    "properties": {
                        "kind": { "type": "string", "enum": ["file"] },
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
        assert_eq!(resp.webhook_path.as_deref(), Some("/webhooks/trig-x"));
        assert!(resp.secret.is_none(), "listing must not echo the secret");
        assert_eq!(resp.kind, "webhook");
        // The keyed form exists, but only the create response uses it.
        assert_eq!(webhook_path(&t), "/webhooks/trig-x?key=abc");

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

    /// A malformed URL fails at registration instead of at the first fire.
    #[test]
    fn notify_webhook_url_validation() {
        for ok in [
            "https://example.com/hook",
            "http://127.0.0.1:8080/notify",
            "http://[::1]:9000/x",
        ] {
            assert!(
                validate_notify_webhook_url(ok).is_ok(),
                "{ok} must be accepted"
            );
        }
        for bad in [
            "not a url",
            "example.com/hook",
            "file:///etc/passwd",
            "ftp://example.com/x",
            "http://",
        ] {
            assert!(
                validate_notify_webhook_url(bad).is_err(),
                "{bad} must be rejected"
            );
        }
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

    // ── Issue #245 batch 25: handler-level pins ───────────────────────────

    use crate::llm::{Completion, MockProvider, TokenUsage};
    use crate::runtime::AgentRuntimeBuilder;

    /// Minimal AppState over a temp workspace, modelled on the cold_load
    /// tests' `test_state` — enough for the trigger handlers and a full
    /// one-shot agent run against a scripted `MockProvider`.
    fn trigger_state(workspace: std::path::PathBuf, provider: Arc<MockProvider>) -> Arc<AppState> {
        let storage_dir = workspace.join(".storage");
        let config = crate::config::Config {
            workspace,
            api_base: "https://example.invalid/v1".into(),
            api_key: Some("test-key".into()),
            model: "mock".into(),
            provider_type: "openai".into(),
            preset: None,
            max_steps: 32,
            max_tokens: 65536,
            temperature: 0.0,
            system_prompt: "test prompt".into(),
            retry_max: 0,
            retry_initial_backoff_secs: 1,
            retry_max_backoff_secs: 1,
            retry_network_budget_secs: 600,
            shell_timeout_secs: 5,
            headless: false,
            memory_summary_limit: 5,
            thinking_budget: None,
            reasoning_effort: None,
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
            session_run_code: None,
            goal_eval_transcript_tail: 12,
            sse_replay_capacity: 512,
            workspace_file_max_bytes: 256 * 1024,
            uplink_max_bytes: 1024 * 1024,
            web_search_provider: None,
            web_search_api_key: None,
            web_search_jina_key: None,
            wall_timeout_secs: 0,
        };
        Arc::new(AppState {
            tools: vec![],
            tool_registry: crate::tools::ToolRegistry::default(),
            config,
            provider,
            host: Arc::new(crate::session_host::SessionHost::new(
                std::time::Duration::from_secs(3600),
                crate::http::AdmissionGate::new(
                    8,
                    std::time::Duration::ZERO,
                    Arc::new(std::sync::atomic::AtomicU64::new(0)),
                    Arc::new(std::sync::atomic::AtomicU64::new(0)),
                ),
            )),
            event_channels: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            metrics: Arc::new(crate::http::Metrics::default()),
            slash_commands: Arc::new(Vec::new()),
            rate_limiter: crate::http::RateLimiter::new(10, 1.0),
            skills: vec![],
            partitioning: Default::default(),
            storage: Arc::new(crate::storage::LocalStorageBackend::new(storage_dir)),
            agui_active_runs: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            session_mirror_root: None,
        })
    }

    fn plain_completion(content: &str) -> Completion {
        Completion {
            content: content.to_string(),
            tool_calls: vec![],
            reasoning_content: None,
            usage: Some(TokenUsage::default()),
            finish_reason: None,
        }
    }

    /// Serialise against every other env-mutating test and pin
    /// `RECURSIVE_HOME` to a private tempdir: both the trigger store and the
    /// notify sandbox root derive from `user_workspace_dir`, which is
    /// home-relative, so a concurrent test re-pinning the home would move
    /// the store (500 on upsert) or the sandbox root mid-handler. Field
    /// order matters: the pin restores the env while the lock is still held.
    fn pinned_home() -> (
        crate::test_util::PinnedRecursiveHomeNoLock,
        tempfile::TempDir,
        std::sync::MutexGuard<'static, ()>,
    ) {
        let lock = crate::test_util::env_lock();
        let home = tempfile::tempdir().unwrap();
        let pin = crate::test_util::PinnedRecursiveHomeNoLock::new(home.path(), &lock);
        (pin, home, lock)
    }

    /// POST /triggers id handling: a blank (or whitespace-only) id falls
    /// through to auto-generation, a supplied id must be [A-Za-z0-9_-]
    /// (400 otherwise), and a valid supplied id is preserved verbatim —
    /// underscore included.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // std env lock is fine: only same-crate tests contend
    async fn create_trigger_blank_id_autogenerates_and_charset_is_enforced() {
        let _env = pinned_home();
        let ws = tempfile::tempdir().unwrap();
        let state = trigger_state(ws.path().to_path_buf(), Arc::new(MockProvider::new(vec![])));

        for blank in ["", "   "] {
            let body: CreateTriggerRequest = serde_json::from_str(&format!(
                r#"{{"kind":"webhook","goal":"g","id":"{blank}"}}"#
            ))
            .unwrap();
            let (status, resp) = create_trigger(
                State(state.clone()),
                Extension(AuthIdentity::local()),
                Json(body),
            )
            .await
            .expect("blank id must fall through to auto-generation");
            assert_eq!(status, StatusCode::CREATED);
            assert!(
                resp.id.starts_with("trig-") && resp.id.len() > "trig-".len(),
                "blank id {blank:?} must auto-generate a trig- id, got {:?}",
                resp.id
            );
        }

        let body: CreateTriggerRequest =
            serde_json::from_str(r#"{"kind":"webhook","goal":"g","id":"bad id!"}"#).unwrap();
        let err = create_trigger(
            State(state.clone()),
            Extension(AuthIdentity::local()),
            Json(body),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(err.message.contains("trigger id must be"));

        let body: CreateTriggerRequest =
            serde_json::from_str(r#"{"kind":"webhook","goal":"g","id":"ok_id-1"}"#).unwrap();
        let (status, resp) = create_trigger(
            State(state.clone()),
            Extension(AuthIdentity::local()),
            Json(body),
        )
        .await
        .expect("id inside the charset must be accepted");
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(resp.id, "ok_id-1");
    }

    /// A file notify target outside the bound sandbox root must be rejected
    /// at registration time (400), while a target inside the root passes —
    /// "surface a bad notify target at registration, not at 9am".
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // std env lock is fine: only same-crate tests contend
    async fn create_trigger_rejects_file_notify_target_outside_sandbox() {
        let _env = pinned_home();
        let ws = tempfile::tempdir().unwrap();
        let state = trigger_state(ws.path().to_path_buf(), Arc::new(MockProvider::new(vec![])));

        // Bind the sandbox root exactly as validate_notify_path will, and
        // aim the in-sandbox case at the derived root (which is
        // home-derived, not the raw workspace path).
        crate::notify::set_file_context(&state.config.workspace);
        let root = crate::notify::file_context_root().expect("root bound");

        let body: CreateTriggerRequest = serde_json::from_str(&format!(
            r#"{{"kind":"webhook","goal":"g","notify":{{"kind":"file","path":"{}"}}}}"#,
            root.join("notify.jsonl").display()
        ))
        .unwrap();
        let (status, _) = create_trigger(
            State(state.clone()),
            Extension(AuthIdentity::local()),
            Json(body),
        )
        .await
        .expect("in-sandbox notify path must be accepted");
        assert_eq!(status, StatusCode::CREATED);

        let body: CreateTriggerRequest = serde_json::from_str(
            r#"{"kind":"webhook","goal":"g","notify":{"kind":"file","path":"/etc/passwd"}}"#,
        )
        .unwrap();
        let err = create_trigger(
            State(state.clone()),
            Extension(AuthIdentity::local()),
            Json(body),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.status,
            StatusCode::BAD_REQUEST,
            "out-of-sandbox notify file target must be rejected at registration"
        );
    }

    /// Firing a webhook with a non-empty body must forward that body to the
    /// agent as payload context after the goal — the entire point of the
    /// `?key=` POST body. Kills both halves of the forwarding gate: the
    /// `Some(body)`-only-when-non-empty gate in `fire_webhook` and the
    /// non-empty-append guard in `fire_trigger`.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // std env lock is fine: only same-crate tests contend
    async fn fire_webhook_forwards_body_as_goal_payload_context() {
        let _env = pinned_home();
        let ws = tempfile::tempdir().unwrap();
        let arrived = Arc::new(tokio::sync::Notify::new());
        let provider = Arc::new(
            MockProvider::new(vec![plain_completion("done")]).with_on_complete(arrived.clone()),
        );
        let state = trigger_state(ws.path().to_path_buf(), provider.clone());

        let mut trigger = Trigger::new(
            "trig-payload",
            TriggerSpec::Webhook {
                secret: "s3cr3t".into(),
            },
            "do the thing",
            None,
            None,
        );
        trigger.enabled = true;
        store(&state).upsert(trigger).unwrap();

        let resp = fire_webhook(
            State(state.clone()),
            Extension(AuthIdentity::local()),
            Path("trig-payload".to_string()),
            Query(WebhookKeyQuery {
                key: Some("s3cr3t".into()),
            }),
            axum::body::Bytes::from_static(b"hello payload"),
        )
        .await
        .expect("fire accepted");
        assert_eq!(resp.status(), StatusCode::ACCEPTED);

        // Deterministic wait (no wall-clock sleep): the provider notifier
        // fires exactly when the spawned fire reaches the LLM call.
        tokio::time::timeout(std::time::Duration::from_secs(10), arrived.notified())
            .await
            .expect("trigger fire must reach the provider");

        let calls = provider.calls();
        let last = calls.last().expect("at least one provider call");
        let prompt = last
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            prompt.contains("do the thing\n\nWebhook payload:\nhello payload"),
            "webhook body must be forwarded as payload context after the goal, got: {prompt:?}"
        );
    }

    /// `run_in_session` returns the session's own identity plus the real
    /// turn outcome, and actually enqueues the goal into that session's
    /// runtime — not a fabricated ("xyzzy" / empty) triple.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // std env lock is fine: only same-crate tests contend
    async fn run_in_session_returns_real_session_id_and_turn_outcome() {
        let _env = pinned_home();
        let ws = tempfile::tempdir().unwrap();
        let provider = Arc::new(MockProvider::new(vec![plain_completion("turn answer")]));
        let state = trigger_state(ws.path().to_path_buf(), provider.clone());
        let runtime = AgentRuntimeBuilder::new()
            .llm(provider.clone())
            .tools(crate::tools::ToolRegistry::default())
            .build()
            .expect("runtime build");
        let session = Arc::new(SessionState {
            id: "sess-trig-run".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            title: None,
            owner: None,
            tenant: None,
            runtime: Arc::new(tokio::sync::Mutex::new(runtime)),
            plan_approval_gate: Arc::new(crate::tools::plan_mode::PlanApprovalGate::new()),
            interrupt_token: Arc::new(tokio::sync::Mutex::new(None)),
            non_system_message_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            last_active_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            usage: Arc::new(crate::http::SessionUsage::new("test-model")),
            event_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            event_log: Arc::new(crate::http::SessionEventLog::new(
                crate::http::SESSION_EVENT_LOG_CAPACITY,
            )),
        });
        let before = session.runtime.lock().await.transcript().len();

        let (session_id, final_text, finish_reason) =
            run_in_session(&state, &session, "trigger goal").await;

        assert_eq!(
            session_id, "sess-trig-run",
            "must return the session's own id"
        );
        assert_eq!(
            final_text.as_deref(),
            Some("turn answer"),
            "must return the turn's final text"
        );
        assert_eq!(
            finish_reason, "no_more_tool_calls",
            "must return the turn's finish reason"
        );
        let after = session.runtime.lock().await.transcript().len();
        assert!(
            after > before,
            "the goal must actually be enqueued into the session runtime"
        );
    }
}
