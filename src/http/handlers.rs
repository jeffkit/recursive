//! HTTP handler functions for the agent API.

use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    response::sse::{Event, Sse},
    Json,
};
use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::broadcast;
use tokio_stream::{wrappers::BroadcastStream, wrappers::IntervalStream, StreamExt};

use crate::event::{AgentEvent, EnvelopeSink, NullSink};
use crate::message::Role;
use crate::permissions::{LayeredPermissionsConfig, PermissionMode};
use crate::runtime::AgentRuntimeBuilder;
use crate::tools::ToolRegistry;

use super::{
    build_openapi_spec, AcquireError, AdmissionGate, ApiError, AppState, AuthIdentity,
    CreateSessionRequest, CreateSessionResponse, ErrorResponse, ListSessionsQuery, PresetInfo,
    RunRequest, RunResponse, SessionDetailResponse, SessionInfo, SessionMessageRequest,
    SessionMessageResponse, SessionOverrides, SessionState, SetGoalRequest, SlashCommandInfo,
    SseContentBlock, SseEvent, SseFrame, ToolInfo, UsageInfo,
};

// Constant body — no branching worth scoring.
#[cfg_attr(test, mutants::skip)]
pub(super) async fn health() -> &'static str {
    "ok"
}

/// Issue #85: assert the request's identity may reach this session.
///
/// Every `/sessions/:id*` route goes through here, so a credential can only
/// touch the sessions it created (or every session, when it carries the
/// `admin` role). 403 — the session exists, the caller is simply not its
/// owner; ids are UUIDv7, so there is no enumerable id space to protect by
/// pretending it does not exist.
///
/// `pub(super)` so `cold_load` can assert ownership against an entry it just
/// loaded, before building the runtime for it.
pub(super) fn ensure_access(
    identity: &AuthIdentity,
    session: &SessionState,
) -> Result<(), ApiError> {
    ensure_owned(
        identity,
        session.owner.as_deref(),
        session.tenant.as_deref(),
    )
}

/// The same decision against a bare (owner, tenant) pair — for callers that
/// only have the persisted metadata at hand (`POST /triggers` referencing a
/// session it does not materialize).
pub(super) fn ensure_owned(
    identity: &AuthIdentity,
    owner: Option<&str>,
    tenant: Option<&str>,
) -> Result<(), ApiError> {
    if identity.may_access_session(owner, tenant) {
        Ok(())
    } else {
        Err(ApiError::forbidden("session belongs to another identity"))
    }
}

/// Issue #85: assert the identity may act on the session `id` **without
/// materializing it** — the live table decides when the session is in memory,
/// otherwise the ownership recorded in the persisted metadata does.
///
/// An id the server knows nothing about is allowed through: there is no
/// session to hijack, and the caller (trigger registration) has its own
/// unknown-id behaviour at fire time.
pub(super) async fn ensure_session_access_by_id(
    state: &Arc<AppState>,
    id: &str,
    identity: &AuthIdentity,
) -> Result<(), ApiError> {
    {
        let sessions_lock = state.host.sessions();
        let sessions = sessions_lock.read().await;
        if let Some(session) = sessions.get(id) {
            return ensure_access(identity, session);
        }
    }
    match super::cold_load::load_session_meta(state, id).await {
        Some(meta) => ensure_owned(identity, meta.owner.as_deref(), meta.tenant.as_deref()),
        None => Ok(()),
    }
}

/// Issue #123: reserved storage key for the `/readyz` storage probe. No other
/// code path reads or writes it, so it can never collide with a session
/// transcript or a memory entry.
const READYZ_PROBE_KEY: &str = "__readyz_probe__";

/// Issue #123: consecutive failed **LLM** runs after which `/readyz` reports
/// not-ready. Catches a hard-broken key/gateway within a few requests while
/// letting a single transient 5xx recover without flapping a healthy pod out
/// of rotation.
pub const READYZ_MAX_LLM_FAILURES: u64 = 3;

/// Issue #123: how long a failure streak keeps `/readyz` not-ready.
///
/// Clearing the streak on the next success is not enough on its own: once the
/// probe fails, a k8s Service drops the pod from its endpoints, so it receives
/// no runs — and therefore no success — and a three-request gateway blip would
/// latch until a human restarted the pod (with every replica tripped, the
/// Service would have no endpoints left and could not recover on its own).
/// So a streak whose most recent failure is older than this window no longer
/// describes the present: `/readyz` decays it and reports ready again. A
/// genuinely broken gateway re-trips the streak within
/// [`READYZ_MAX_LLM_FAILURES`] requests once traffic returns.
pub const READYZ_LLM_FAILURE_WINDOW_MS: u64 = 60_000;

/// Issue #123: how long a `/readyz` storage verdict is reused.
///
/// The endpoint is on the unauthenticated public router (a k8s probe cannot
/// carry a key), so probing storage on every request would let an anonymous
/// caller drive one write — an S3 PUT, in cloud deployments — per request just
/// by scraping it.
pub const READYZ_PROBE_TTL_MS: u64 = 5_000;

/// Issue #123: [`super::now_session_ms`] with `0` reserved for the "never
/// happened" sentinel the readiness bookkeeping uses.
///
/// The epoch is initialised lazily by the first caller in the process, so that
/// first call reads `0` — a success, failure or probe landing in that first
/// millisecond would otherwise be indistinguishable from "never happened"
/// (`/readyz` would report `last_success_ms_ago: null` for a run that just
/// succeeded).
fn now_stamp_ms() -> u64 {
    super::now_session_ms().max(1)
}

/// Issue #123: does a failure streak still describe the present?
///
/// Only a *recent* last failure does (see [`READYZ_LLM_FAILURE_WINDOW_MS`]);
/// an unstamped streak (`last_failure_ms == 0`) is no evidence of an outage
/// either, so it decays like a stale one.
fn llm_streak_is_current(failures: u64, last_failure_ms: u64, now_ms: u64) -> bool {
    failures > 0
        && last_failure_ms != 0
        && now_ms.saturating_sub(last_failure_ms) < READYZ_LLM_FAILURE_WINDOW_MS
}

/// Issue #123: is the cached storage verdict still usable?
///
/// A stamp of `0` means "never probed" — probe now instead of trusting a
/// verdict nobody took (see [`READYZ_PROBE_TTL_MS`]).
fn readyz_probe_is_fresh(probed_ms: u64, now_ms: u64) -> bool {
    probed_ms != 0 && now_ms.saturating_sub(probed_ms) < READYZ_PROBE_TTL_MS
}

/// Value written by the `/readyz` storage probe.
///
/// Constant for the life of the process — not per request — so two concurrent
/// probes write identical bytes and cannot make each other's read-back look
/// like a mismatch, while still differing from whatever a previous process
/// left behind (a backend that accepts our write and silently drops it then
/// fails the read-back instead of reading a stale but plausible value).
fn readyz_probe_value() -> String {
    format!("readyz-{}", std::process::id())
}

/// Issue #123: probe storage writability with a real write + read-back.
///
/// This is the only way a read-only mount or a full disk shows up *before* a
/// session teardown silently loses data. Shares its implementation with
/// `recursive doctor --probe` so the two probes cannot drift.
async fn probe_storage(storage: &Arc<dyn crate::storage::StorageBackend>) -> bool {
    match crate::storage::memory_round_trip(
        storage.as_ref(),
        READYZ_PROBE_KEY,
        &readyz_probe_value(),
    )
    .await
    {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(error = %e, "readyz: storage probe failed");
            false
        }
    }
}

/// `GET /readyz` — k8s-style readiness probe (issue #123).
///
/// `/health` is a constant `"ok"`; this endpoint exists to make the three
/// "半死" states a live process still answers 200 for visible to a load
/// balancer:
///
/// 1. **storage** — a real write *and read-back* round-trip to the configured
///    backend, so a read-only mount or a full disk fails here exactly as it
///    would on a session teardown save. The verdict is cached for
///    [`READYZ_PROBE_TTL_MS`] because this endpoint is public and a probe
///    costs a write.
/// 2. **llm** — a *recent* streak of failed LLM runs
///    (`READYZ_MAX_LLM_FAILURES` → a dead API key / unreachable gateway),
///    decayed after [`READYZ_LLM_FAILURE_WINDOW_MS`] so a transient blip
///    cannot latch the pod out of rotation forever.
/// 3. **admission** — the run pool is saturated *and* requests are queued;
///    fully-busy-but-draining is normal load, not unreadiness.
///
/// Returns `200` with a per-check JSON body when ready, `503` otherwise.
pub(super) async fn readyz(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let now = super::now_session_ms();

    // 1. Storage round-trip probe. The failure reason is logged, not echoed:
    //    this endpoint is unauthenticated (a k8s probe cannot carry a key) and
    //    backend errors carry absolute paths.
    let probed_ms = state
        .metrics
        .readyz_storage_probed_ms
        .load(Ordering::Relaxed);
    let storage_ok = if readyz_probe_is_fresh(probed_ms, now) {
        state.metrics.readyz_storage_ok.load(Ordering::Relaxed)
    } else {
        let ok = probe_storage(&state.storage).await;
        // Verdict first, then the stamp: a concurrent reader that sees a fresh
        // stamp must also see the verdict it belongs to.
        state.metrics.readyz_storage_ok.store(ok, Ordering::Relaxed);
        state
            .metrics
            .readyz_storage_probed_ms
            .store(now_stamp_ms(), Ordering::Relaxed);
        ok
    };
    let storage = serde_json::json!({ "ok": storage_ok });

    // 2. LLM reachability: a failure streak that is still current.
    let mut failures = state
        .metrics
        .llm_failures_consecutive
        .load(Ordering::Relaxed);
    let last_failure = state.metrics.last_llm_failure_ms.load(Ordering::Relaxed);
    if failures > 0 && !llm_streak_is_current(failures, last_failure, now) {
        // See READYZ_LLM_FAILURE_WINDOW_MS: a streak nothing has refreshed
        // must not keep the pod out of rotation. Compare-and-swap so a failure
        // recorded concurrently is not thrown away with the decayed streak.
        failures = match state.metrics.llm_failures_consecutive.compare_exchange(
            failures,
            0,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => 0,
            Err(current) => current,
        };
    }
    let last_success = state.metrics.last_llm_success_ms.load(Ordering::Relaxed);
    let llm_ok = failures < READYZ_MAX_LLM_FAILURES;
    let llm = serde_json::json!({
        "ok": llm_ok,
        "consecutive_failures": failures,
        "last_success_ms_ago": if last_success == 0 {
            serde_json::Value::Null
        } else {
            serde_json::json!(now.saturating_sub(last_success))
        },
    });

    // 3. Admission saturation.
    let in_flight = state.metrics.runs_in_flight.load(Ordering::Relaxed);
    let waiting = state.metrics.runs_waiting.load(Ordering::Relaxed);
    let max_concurrent = state.host.admission().max_concurrent_runs() as u64;
    let saturated = max_concurrent > 0 && in_flight >= max_concurrent;
    let admission_ok = !(saturated && waiting > 0);
    let admission = serde_json::json!({
        "ok": admission_ok,
        "in_flight": in_flight,
        "max_concurrent": max_concurrent,
        "saturated": saturated,
    });

    let ready = storage_ok && llm_ok && admission_ok;
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(serde_json::json!({
            "ready": ready,
            "checks": { "storage": storage, "llm": llm, "admission": admission },
        })),
    )
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

/// Goal-393 / issue #127: the one place where HTTP session runtimes get
/// built. Every build point (`POST /run`, `POST /sessions`, session fork)
/// goes through here so the channels cannot drift apart — the compactor /
/// microcompactor / transcript cap / post-compaction re-injection assembly
/// comes from the same frontend-neutral preset the CLI and TUI use
/// ([`crate::preset::apply`]).
///
/// `/agui` builds through [`build_session_runtime_parts`] (same preset
/// assembly); the provider / wall-timeout / storage / streaming setters are
/// layered on in `super::agui::build_agui_runtime` — fed from `AppState`
/// here, so every channel stays on the same budget. Its runtime assembly
/// lives in `super::agui`, away from the axum types.
///
/// The caller resolves the session's preset with
/// [`resolve_session_preset`] and passes it in, so a request-scoped preset
/// (`POST /sessions`'s `preset` field) is applied exactly once, and an
/// unknown id fails the request instead of silently falling back.
///
/// Callers add what is genuinely request-specific on top of the returned
/// builder (`seed_transcript` for `/agui` resume, then `build()`).
///
/// `overrides` carries the per-session knobs from the request body (issue
/// #94: `max_budget_usd`, `thinking_budget`) — see [`SessionOverrides`].
///
/// `pub(super)` since issue #98: `http::cold_load` reuses this exact path for
/// restored sessions so they cannot drift from freshly created ones.
pub(super) fn build_session_runtime(
    state: &AppState,
    tool_registry: ToolRegistry,
    system_prompt: String,
    prompt_segments: crate::system_prompt::PromptSegments,
    max_steps: usize,
    preset: &crate::preset::ResolvedPreset,
    overrides: SessionOverrides,
) -> AgentRuntimeBuilder {
    let skills = state.skills.clone();
    build_session_runtime_parts(
        tool_registry,
        crate::preset::apply_prompt(system_prompt, preset),
        prompt_segments,
        max_steps,
        preset,
        skills.clone(),
        HTTP_CHANNEL,
    )
    // #74 拆单 3/3: the merged skill catalog (directory + service-level
    // SkillSource entries) rides into the kernel, which ships it as the
    // per-turn `<system-reminder>` — without this the catalog is
    // computed at startup but never reaches any run's context.
    .skills(skills)
    // Issue #94: a per-request thinking budget needs its own provider
    // (the budget is a request field in the Anthropic body); everything
    // else reuses the server's shared provider.
    .llm(provider_for_request(state, overrides.thinking_budget))
    // Issue #94: `max_budget_usd` used to be a stored-but-dead field —
    // the step loop now stops with `BudgetExceeded` once the turn's
    // spend reaches the ceiling.
    .cost_budget(
        overrides.max_budget_usd.or(state.config.max_budget_usd),
        crate::llm::pricing_for(&state.config.model),
    )
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
    .streaming(true)
}

/// What an HTTP session can host (issue #127): the plan-mode tools block on a
/// live human answering `confirm_plan()`, and an HTTP session is polled
/// asynchronously — so the preset's plan tools stay unregistered here, exactly
/// as before presets existed. Change this to `interactive: true` only together
/// with a real plan-approval flow, not to satisfy a preset declaration.
pub(super) const HTTP_CHANNEL: crate::preset::ChannelSupport =
    crate::preset::ChannelSupport { interactive: false };

/// Resolve the preset a request (or a restored session) runs with, mapping an
/// unknown id to a 400 instead of silently falling back to `standard`
/// (issue #127).
pub(super) fn resolve_session_preset(
    explicit: Option<&str>,
    config: &crate::config::Config,
) -> Result<crate::preset::ResolvedPreset, ApiError> {
    crate::preset::resolve_session(explicit, config, &crate::preset::PresetEnv::from_process())
        .map_err(|e| ApiError::bad_request(e.to_string()))
}

/// Issue #94: resolve the provider for one request.
///
/// `thinking_budget` is a provider-level setting (Anthropic sends it as
/// `thinking.budget_tokens`), so a value that differs from the server's
/// default can only be honoured by building a provider for this request. The
/// shared server provider is returned for every other case — no explicit
/// budget, an unauthenticated `Config`, or a failed build — so behaviour is
/// unchanged apart from a logged warning.
fn provider_for_request(
    state: &AppState,
    thinking_budget: Option<u32>,
) -> Arc<dyn crate::llm::ChatProvider> {
    let Some(budget) = thinking_budget.filter(|b| Some(*b) != state.config.thinking_budget) else {
        return state.provider.clone();
    };
    let Some(api_key) = state.config.api_key.as_deref().filter(|k| !k.is_empty()) else {
        return state.provider.clone();
    };
    let retry = crate::llm::RetryPolicy {
        max_retries: state.config.retry_max,
        initial_backoff: Duration::from_secs(state.config.retry_initial_backoff_secs),
        max_backoff: Duration::from_secs(state.config.retry_max_backoff_secs),
    };
    match crate::llm::build_llm_provider(&state.config, api_key, retry, None, Some(budget)) {
        Ok(provider) => provider,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "failed to build provider for per-request thinking_budget; \
                 falling back to the server provider"
            );
            state.provider.clone()
        }
    }
}

/// Provider-agnostic core of [`build_session_runtime`]: the preset assembly
/// (context management + post-compaction re-injection + tool flags) on top of
/// an empty builder. The AG-UI layer (`super::agui::build_agui_runtime`)
/// layers the provider and wall-clock budget on top, keeping its runtime
/// build free of `AppState`.
pub(super) fn build_session_runtime_parts(
    tool_registry: ToolRegistry,
    system_prompt: String,
    prompt_segments: crate::system_prompt::PromptSegments,
    max_steps: usize,
    preset: &crate::preset::ResolvedPreset,
    skills: Vec<crate::skills::Skill>,
    channel: crate::preset::ChannelSupport,
) -> AgentRuntimeBuilder {
    // The assets (shared read state, skill catalog) come from the registry
    // BEFORE it moves into the builder, so post-compaction re-injection has
    // something to re-inject on every channel — not just the CLI (issue #127).
    let assets = crate::preset::assets_from_registry(&tool_registry, skills);
    let builder = AgentRuntimeBuilder::new()
        .tools(tool_registry)
        .system_prompt(system_prompt)
        .prompt_segments(prompt_segments)
        .max_steps(max_steps);
    crate::preset::apply(builder, preset, &assets, channel)
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
///
/// `usage` is the token spend of the steps that completed before the failure
/// (`AgentRuntime::last_failed_usage`) — issue #115: a failed turn still burns
/// real tokens, so they are added to `tokens_wasted_on_failure_total` instead
/// of vanishing with the error.
///
/// Run bookkeeping only — a failure here says nothing about the LLM endpoint,
/// so readiness is driven by [`record_llm_failure`] instead (a client
/// cancellation or a tool/storage fault must not take a healthy pod out of
/// rotation).
pub(super) fn record_run_failed(metrics: &super::Metrics, usage: &crate::llm::TokenUsage) {
    metrics.agent_runs_total.fetch_add(1, Ordering::Relaxed);
    metrics.agent_runs_failed.fetch_add(1, Ordering::Relaxed);
    let wasted = (usage.prompt_tokens as u64).saturating_add(usage.completion_tokens as u64);
    metrics
        .tokens_wasted_on_failure_total
        .fetch_add(wasted, Ordering::Relaxed);
}

/// Issue #123: a run that really completed proves the LLM endpoint answered —
/// reset the readiness streak and stamp the success instant.
///
/// Deliberately separate from [`record_run_success`]: a turn the client
/// interrupted can finish as `Cancelled` without the provider ever having
/// answered, so only call sites that know the run completed may clear the
/// streak.
pub(super) fn record_llm_success(metrics: &super::Metrics) {
    metrics.llm_failures_consecutive.store(0, Ordering::Relaxed);
    metrics
        .last_llm_success_ms
        .store(now_stamp_ms(), Ordering::Relaxed);
}

/// Issue #123: the LLM call failed *provider-side* — a revoked key, a rate
/// limit, a 5xx, an unreachable gateway, a malformed provider response (see
/// [`crate::error::Error::is_llm_failure`]; a request-caused 4xx is not one).
/// Drives the `/readyz` streak.
pub(super) fn record_llm_failure(metrics: &super::Metrics) {
    metrics
        .llm_failures_consecutive
        .fetch_add(1, Ordering::Relaxed);
    metrics
        .last_llm_failure_ms
        .store(now_stamp_ms(), Ordering::Relaxed);
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

/// GET /presets — the built-in agent presets, each with its capability
/// inventory and the context management it currently resolves to (issue #127).
///
/// Read-only and cheap: the resolution is pure, so this is also the honest
/// answer to "what would a session created right now actually run with?".
pub(super) async fn list_presets(State(state): State<Arc<AppState>>) -> Json<Vec<PresetInfo>> {
    let env = crate::preset::PresetEnv::from_process();
    Json(
        crate::preset::builtin()
            .iter()
            .map(|p| PresetInfo {
                id: p.id.to_string(),
                description: p.description.to_string(),
                capabilities: p.capabilities.to_vec(),
                resolved: p.resolve(&state.config, &env),
            })
            .collect(),
    )
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

    let preset = resolve_session_preset(None, &state.config)?;
    let mut runtime = build_session_runtime(
        &state,
        tool_registry,
        system_prompt,
        prompt_segments,
        max_steps,
        &preset,
        SessionOverrides {
            max_budget_usd: body.max_budget_usd,
            thinking_budget: body.thinking_budget,
        },
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
            record_run_failed(&state.metrics, &runtime.last_failed_usage());
            // Issue #123: only a failure of the LLM call itself says the
            // endpoint is down — a tool or storage error must not.
            if e.is_llm_failure() {
                record_llm_failure(&state.metrics);
            }
            return Err(map_run_error(&e));
        }
    };
    runtime.destroy_environment().await;

    record_run_success(&state.metrics, outcome.steps, &outcome.total_usage);
    record_llm_success(&state.metrics);

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
///
/// `pub(super)` since issue #98: cold load re-parses the persisted
/// `permission_mode` through here, so the `allow_bypass_permissions` guard
/// applies to restored sessions too.
pub(super) fn parse_permission_mode(s: &str, allow_bypass: bool) -> PermissionMode {
    match s.to_ascii_lowercase().as_str() {
        "auto" => PermissionMode::Auto,
        "strict" => PermissionMode::Strict,
        "bypass" | "bypass_permissions" if allow_bypass => PermissionMode::BypassPermissions,
        _ => PermissionMode::Default,
    }
}

/// Render a [`PermissionMode`] in the API's request vocabulary (the strings
/// [`parse_permission_mode`] accepts) so `GET /sessions/:id` can report the
/// live mode. Variants unreachable from the HTTP surface keep their serde
/// camelCase names.
fn permission_mode_label(mode: &PermissionMode) -> String {
    match mode {
        PermissionMode::Default => "default",
        PermissionMode::Auto => "auto",
        PermissionMode::Strict => "strict",
        PermissionMode::BypassPermissions => "bypass",
        PermissionMode::AcceptEdits => "acceptEdits",
        PermissionMode::DontAsk => "dontAsk",
        PermissionMode::Plan { .. } => "plan",
    }
    .to_string()
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
///
/// Issue #85: the session is owned by the caller's identity (subject +
/// tenant). Every later `/sessions/:id*` request is asserted against it.
pub(super) async fn create_session(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AuthIdentity>,
    Json(body): Json<CreateSessionRequest>,
) -> Result<(StatusCode, Json<CreateSessionResponse>), ApiError> {
    let id = generate_session_id();
    let created_at = format_timestamp(SystemTime::now());
    let custom_base = body.system_prompt.is_some() || body.append_system_prompt.is_some();
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
    // Issue #98: only a REQUEST-supplied base is persisted. A default session
    // keeps tracking the server default (the pre-#98 behaviour), while a custom
    // persona survives a restart instead of being silently swapped.
    let base_system_prompt = if custom_base {
        Some(system_prompt.clone())
    } else {
        None
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

    // Issue #127: the session's agent preset — an explicit request id wins,
    // else `RECURSIVE_AGENT_PRESET`, else `standard`. An unknown id is a 400,
    // never a silent fallback.
    let preset = resolve_session_preset(body.preset.as_deref(), &state.config)?;
    let mut runtime = build_session_runtime(
        &state,
        tool_registry,
        system_prompt,
        prompt_segments,
        max_steps,
        &preset,
        SessionOverrides {
            max_budget_usd: body.max_budget_usd,
            thinking_budget: body.thinking_budget,
        },
    )
    .build()
    .map_err(|e| ApiError::internal(format!("failed to build session runtime: {e}")))?;

    // Register the session ID so all turns emit tracing spans with
    // session_id. The transcript is NOT saved per turn — the host layer
    // persists it once on teardown (DELETE / idle eviction / graceful
    // shutdown) through the storage backend that `build_session_runtime`
    // wires into the builder (Goal 396).
    runtime.set_session_id(&id);

    // Issue #98: persist the per-session configuration so a cold-loaded
    // session keeps its custom persona / permission mode / title / step cap
    // instead of silently reverting to the server defaults. Best-effort:
    // a storage failure must not fail session creation.
    //
    // Issue #127: the preset is persisted even when it is the default. Unlike
    // the persona (a default session should track the server default), a preset
    // *is* the session's runtime wiring — restoring one with a different preset
    // would silently change its tools and context management mid-life.
    super::cold_load::persist_session_meta(
        &state,
        &id,
        &super::cold_load::SessionMeta {
            system_prompt: base_system_prompt,
            permission_mode: body.permission_mode.clone(),
            title: body.session_name.clone(),
            max_steps: body.max_steps.map(|n| n as usize),
            preset: Some(preset.id.clone()),
            // Issue #94: keep the per-session budget / thinking overrides
            // across a cold load — dropping them on restart would silently
            // remove the client's protection (same drift class as #98).
            overrides: SessionOverrides {
                max_budget_usd: body.max_budget_usd,
                thinking_budget: body.thinking_budget,
            },
            // Issue #85: ownership must survive a restart, otherwise a
            // cold-loaded session would fall back to admin-only and the
            // original caller would lose access to its own session.
            owner: Some(identity.subject.clone()),
            tenant: identity.tenant.clone(),
        },
    )
    .await;

    // Extract the gate before moving runtime into the Mutex so HTTP handlers
    // can approve/reject without acquiring the per-session runtime lock.
    let plan_approval_gate = runtime.plan_approval_gate();

    let session = SessionState {
        id: id.clone(),
        created_at: created_at.clone(),
        title: body.session_name,
        owner: Some(identity.subject.clone()),
        tenant: identity.tenant.clone(),
        runtime: Arc::new(tokio::sync::Mutex::new(runtime)),
        plan_approval_gate,
        interrupt_token: Arc::new(tokio::sync::Mutex::new(None)),
        non_system_message_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        last_active_ms: Arc::new(AtomicU64::new(super::now_session_ms())),
        prompt_tokens: Arc::new(AtomicU64::new(0)),
        completion_tokens: Arc::new(AtomicU64::new(0)),
        event_seq: Arc::new(AtomicU64::new(0)),
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
///
/// Issue #85: the list is scoped to the caller's identity — a credential only
/// sees the sessions it created (admins see every session). `total` counts the
/// visible sessions, so pagination stays consistent with the page slice.
pub(super) async fn list_sessions(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AuthIdentity>,
    axum::extract::Query(params): axum::extract::Query<ListSessionsQuery>,
) -> Json<SessionList> {
    let sessions_lock = state.host.sessions();
    let sessions = sessions_lock.read().await;
    let mut infos = Vec::with_capacity(sessions.len());
    for s in sessions.values() {
        if !identity.may_access_session(s.owner.as_deref(), s.tenant.as_deref()) {
            continue;
        }
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
///
/// Issue #85: ownership is asserted by [`super::cold_load::get_or_load_session`]
/// before the session is materialized or restored.
pub(super) async fn get_session(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AuthIdentity>,
    Path(id): Path<String>,
) -> Result<Json<SessionDetailResponse>, ApiError> {
    let session = super::cold_load::get_or_load_session(&state, &id, &identity).await?;

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
    let (messages, todos, goal, permission_mode, preset) = match session.runtime.try_lock() {
        Ok(runtime) => {
            let msgs = runtime
                .transcript()
                .iter()
                .filter_map(|msg| serde_json::to_value(msg).ok())
                .collect();
            let todos = runtime.current_todos();
            let goal = runtime.current_goal();
            // Issue #98: read the live mode straight off the tool registry so
            // a restored session reports what it actually runs with.
            let permission_mode =
                permission_mode_label(&runtime.kernel().tools().permission_mode());
            // Issue #127: same principle for the preset — the runtime knows
            // what it was assembled from, so a restored session reports the
            // preset it was rebuilt with, not the server's current default.
            (
                msgs,
                todos,
                goal,
                Some(permission_mode),
                runtime.preset_id().map(str::to_string),
            )
        }
        Err(_) => (vec![], vec![], None, None, None),
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
        permission_mode,
        preset,
    }))
}

/// DELETE /sessions/:id — remove a session.
///
/// Issue #85: only the session's owner (or an admin) may delete it.
pub(super) async fn delete_session(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AuthIdentity>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    // Look up the runtime under a read lock so we can take the per-session
    // runtime Mutex and call `close()` without holding the global write
    // lock across an await point.
    let session_runtime = {
        let sessions_lock = state.host.sessions();
        let sessions = sessions_lock.read().await;
        match sessions.get(&id) {
            Some(s) => {
                ensure_access(&identity, s)?;
                Some(s.runtime.clone())
            }
            None => None,
        }
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
            // Issue #123: a failed teardown save is real data loss — count it
            // so it is visible on `/metrics`, not just in the log.
            state
                .metrics
                .persist_failures
                .fetch_add(1, Ordering::Relaxed);
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
            // Issue #123: a missing tombstone means a deleted session can be
            // cold-loaded back to life — a correctness failure, so count it.
            state
                .metrics
                .persist_failures
                .fetch_add(1, Ordering::Relaxed);
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
///
/// Issue #85: only the session's owner (or an admin) may rename it.
pub(super) async fn patch_session(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AuthIdentity>,
    Path(id): Path<String>,
    Json(body): Json<PatchSessionRequest>,
) -> Result<Json<SessionInfo>, ApiError> {
    let (info, title) = {
        let sessions_lock = state.host.sessions();
        let mut sessions = sessions_lock.write().await;
        let session = sessions
            .get_mut(&id)
            .ok_or_else(|| ApiError::not_found("session not found"))?;
        ensure_access(&identity, session)?;

        if let Some(title) = body.title {
            session.title = if title.is_empty() { None } else { Some(title) };
        }

        // Read the pre-computed non-system message count directly from the
        // atomic. It is updated whenever a non-system message is appended, so
        // we don't need to acquire the runtime lock here.
        let info = SessionInfo {
            id: session.id.clone(),
            created_at: session.created_at.clone(),
            message_count: session.non_system_message_count.load(Ordering::Relaxed),
            title: session.title.clone(),
        };
        (info, session.title.clone())
    };

    // Issue #98: mirror the new title into the persisted metadata — outside
    // the sessions lock, since this is storage IO — so a restart restores it.
    super::cold_load::update_persisted_title(&state, &id, title).await;

    Ok(Json(info))
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
///
/// Issue #85: forking a session copies its transcript, so only its owner (or
/// an admin) may do it; the fork is owned by the caller.
pub(super) async fn fork_session(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AuthIdentity>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<ForkSessionResponse>), ApiError> {
    // Snapshot the source transcript while holding the write lock.
    let (transcript_snapshot, source_preset) = {
        let sessions_lock = state.host.sessions();
        let sessions = sessions_lock.read().await;
        let src = sessions
            .get(&id)
            .ok_or_else(|| ApiError::not_found("session not found"))?;
        ensure_access(&identity, src)?;
        let rt = src
            .runtime
            .try_lock()
            .map_err(|_| ApiError::conflict("session is busy"))?;
        (
            rt.transcript().to_vec(),
            // Issue #127: a fork inherits the source's preset, not the server
            // default — otherwise the same transcript continues under
            // different tools and context management.
            rt.preset_id().map(str::to_string),
        )
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

    let preset = resolve_session_preset(source_preset.as_deref(), &state.config)?;
    let mut runtime = build_session_runtime(
        &state,
        tool_registry,
        system_prompt,
        prompt_segments,
        state.config.max_steps,
        &preset,
        SessionOverrides::default(),
    )
    .build()
    .map_err(|_| ApiError::internal("failed to build forked session runtime"))?;

    // Issue #85: the fork needs its own metadata blob, exactly like
    // `create_session` writes one. The fork's transcript is flushed on
    // shutdown, so without a blob the restarted server cold-loads it with no
    // owner — the creator's own fork would 403, disappear from `GET /sessions`
    // and stop being deletable. Best-effort, same contract as creation.
    super::cold_load::persist_session_meta(
        &state,
        &new_id,
        &super::cold_load::SessionMeta {
            // The fork is assembled from the server default base prompt and no
            // permission override, so both stay `None` (= server default) —
            // persisting them would rebuild it with settings it never ran.
            // The preset is NOT a default: it is inherited from the source
            // (issue #127), and a restart must not rewire the fork.
            preset: Some(preset.id.clone()),
            owner: Some(identity.subject.clone()),
            tenant: identity.tenant.clone(),
            ..Default::default()
        },
    )
    .await;

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
        owner: Some(identity.subject.clone()),
        tenant: identity.tenant.clone(),
        runtime: Arc::new(tokio::sync::Mutex::new(runtime)),
        plan_approval_gate,
        interrupt_token: Arc::new(tokio::sync::Mutex::new(None)),
        non_system_message_count: Arc::new(std::sync::atomic::AtomicUsize::new(non_system_count)),
        last_active_ms: Arc::new(AtomicU64::new(super::now_session_ms())),
        prompt_tokens: Arc::new(AtomicU64::new(0)),
        completion_tokens: Arc::new(AtomicU64::new(0)),
        event_seq: Arc::new(AtomicU64::new(0)),
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
///
/// Issue #85: approving is a mutation of someone's session, so it is asserted
/// like every other `/sessions/:id*` route — detection of the session alone is
/// not authorization.
pub(super) async fn session_plan_confirm(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AuthIdentity>,
    Path(session_id): Path<String>,
    Json(body): Json<PlanConfirmRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let sessions_lock = state.host.sessions();
    let sessions = sessions_lock.read().await;
    let session = sessions
        .get(&session_id)
        .ok_or_else(|| ApiError::not_found("session not found"))?;
    ensure_access(&identity, session)?;
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
///
/// Issue #85: ownership-asserted like `plan/confirm`.
pub(super) async fn session_plan_reject(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AuthIdentity>,
    Path(session_id): Path<String>,
    Json(body): Json<PlanRejectRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let sessions_lock = state.host.sessions();
    let sessions = sessions_lock.read().await;
    let session = sessions
        .get(&session_id)
        .ok_or_else(|| ApiError::not_found("session not found"))?;
    ensure_access(&identity, session)?;
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
///
/// Issue #85: only the session's owner (or an admin) may start a loop on it.
pub(super) async fn session_set_goal(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AuthIdentity>,
    Path(session_id): Path<String>,
    Json(body): Json<SetGoalRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let runtime_arc = {
        let sessions_lock = state.host.sessions();
        let sessions = sessions_lock.read().await;
        let session = sessions
            .get(&session_id)
            .ok_or_else(|| ApiError::not_found("session not found"))?;
        ensure_access(&identity, session)?;
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
///
/// Issue #85: only the session's owner (or an admin) may clear its goal.
pub(super) async fn session_clear_goal(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AuthIdentity>,
    Path(session_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let runtime_arc = {
        let sessions_lock = state.host.sessions();
        let sessions = sessions_lock.read().await;
        let session = sessions
            .get(&session_id)
            .ok_or_else(|| ApiError::not_found("session not found"))?;
        ensure_access(&identity, session)?;
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
///
/// Issue #85: cancelling is a mutation, so only the session's owner (or an
/// admin) may do it.
pub(super) async fn session_interrupt(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AuthIdentity>,
    Path(session_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let token_arc = {
        let sessions_lock = state.host.sessions();
        let sessions = sessions_lock.read().await;
        let session = sessions
            .get(&session_id)
            .ok_or_else(|| ApiError::not_found("session not found"))?;
        ensure_access(&identity, session)?;
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

// ── #74 拆单 3/3: service-level skill sources ─────────────────────────────

/// One skill as advertised by `GET /skills`.
#[derive(serde::Serialize)]
pub(crate) struct SkillInfo {
    pub name: String,
    pub description: String,
    pub mode: String,
    pub refs: usize,
    pub sections: usize,
    /// Where the skill came from: `content` = service-level (never lands on
    /// disk), `filesystem` = directory discovery.
    pub source: String,
}

/// GET /skills — list every skill the server loaded, including those
/// delivered via service-level skill sources (`RECURSIVE_SKILL_SOURCE_URL`).
/// Lets an external harness confirm injected skills are live without any
/// local file writes.
#[cfg_attr(test, mutants::skip)]
pub(super) async fn list_skills(State(state): State<Arc<AppState>>) -> Json<Vec<SkillInfo>> {
    use crate::skills::SkillMode;
    Json(
        state
            .skills
            .iter()
            .map(|s| SkillInfo {
                name: s.name.clone(),
                description: s.description.clone(),
                mode: match s.mode {
                    SkillMode::Always => "always",
                    SkillMode::Trigger => "trigger",
                    SkillMode::Globs => "globs",
                    SkillMode::Manual => "manual",
                }
                .to_string(),
                refs: s.refs.len(),
                sections: s.sections.len(),
                source: if s.body.is_some() {
                    "content"
                } else {
                    "filesystem"
                }
                .to_string(),
            })
            .collect(),
    )
}

/// Number of user messages in a session transcript so far — the 0-based turn
/// index reported to observability for the next run (issue #124).
fn prior_turn_index(transcript: &[crate::message::Message]) -> u32 {
    transcript.iter().filter(|m| m.role == Role::User).count() as u32
}

/// POST /sessions/:id/messages — send a message in a session.
///
/// Issue #85: only the session's owner (or an admin) may run a turn in it —
/// `get_or_load_session` asserts ownership before the session is materialized.
pub(super) async fn send_session_message(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AuthIdentity>,
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
    let session = super::cold_load::get_or_load_session(&state, &id, &identity).await?;
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

    // Per-session run fence (issue #96): at most one in-flight turn per
    // session. Before this fence, a mobile timeout-retry would first consume
    // a global admission permit, then queue on the runtime Mutex, then re-run
    // the same prompt — double LLM spend, and enough retries parked the whole
    // pool as waiters. Refuse the duplicate with 409 *before* it touches a
    // permit, so a retry never occupies a global run slot.
    let _run_guard = state
        .host
        .try_begin_run(format!("session:{id}"))
        .ok_or_else(|| {
            ApiError::conflict(format!(
                "a run is already active for session '{id}'; \
                 wait for it to finish before sending another message"
            ))
            .with_retry_after(5)
        })?;

    // Acquire a run permit with a bounded wait (Goal 398): a saturated pool
    // now fails fast with 503 + Retry-After instead of hanging the request.
    let _permit = state
        .host
        .admission()
        .acquire_run()
        .await
        .map_err(|e| admission_error(e, &state.host.admission()))?;
    // Lock the runtime for this turn. The fence above already serializes the
    // session path, so this lock is uncontended for concurrent same-session
    // requests (it still guards against cross-path holders, e.g. triggers).
    let mut runtime = runtime_arc.lock().await;

    // Goal-170: install a fresh cancellation token so `POST .../interrupt`
    // can cancel this turn without affecting future turns.
    let interrupt_token = tokio_util::sync::CancellationToken::new();
    {
        let mut stored = interrupt_token_arc.lock().await;
        *stored = Some(interrupt_token.clone());
    }
    runtime.set_interrupt_token(interrupt_token);

    // Issue #124: one Langfuse trace per HTTP turn when the observability env
    // vars are set; an inert no-op otherwise. The turn index is the count of
    // prior user messages still in the transcript, so multi-turn sessions get
    // distinct `langfuse.trace.metadata.turn` values.
    let turn = prior_turn_index(runtime.transcript());
    // Issue #117: wire an EnvelopeSink so every event forwarded to SSE
    // subscribers carries the session timeline key — wall clock + monotonic
    // per-session seq + session id + turn. `step` alone restarts every turn,
    // so without this a goal-loop's frames cannot be stitched to one timeline.
    let (sink, mut event_rx) =
        EnvelopeSink::with_correlation(Some(id.clone()), turn, session.event_seq.clone());
    let langfuse_run = crate::observability::LangfuseRun::try_new(
        crate::observability::RunMeta::new(
            id.clone(),
            state.config.model.clone(),
            state.config.provider_type.clone(),
        )
        .with_turn(turn),
    );
    let mut event_sinks: Vec<Box<dyn crate::event::EventSink>> = vec![Box::new(sink)];
    crate::observability::with_sink(&langfuse_run, &mut event_sinks);
    runtime.set_event_sink(Arc::new(crate::event::CompositeSink::new(event_sinks)));

    // Spawn a forwarder: EnvelopedEvent → SseFrame → broadcast channel.
    // SDK Phase B: emit a tool_progress frame per finished tool, carrying the
    // duration the runtime measured for that tool (issue #118).
    // Goal 274: also maintain the non_system_message_count atomic so the
    // count stays correct even when the turn errors out mid-run.
    let initial_count = msg_count_arc.load(std::sync::atomic::Ordering::Relaxed);
    let count_arc = msg_count_arc.clone();
    let forward_handle = tokio::spawn(async move {
        let mut count: usize = initial_count;
        while let Some(envelope) = event_rx.recv().await {
            // Issue #117: the frame id is the envelope key, so it is reused for
            // every frame derived from this one event.
            let frame_id = envelope.id();
            let agent_event = &envelope.event;
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
            if let Some(sse_event) = map_agent_event(agent_event) {
                let _ = broadcast_tx.send(SseFrame {
                    id: frame_id.clone(),
                    event: sse_event,
                });
            }
            // After forwarding the tool_result, emit tool_progress with the
            // duration the runtime measured for this tool.
            if let Some(sse_event) = tool_progress_event(agent_event) {
                let _ = broadcast_tx.send(SseFrame {
                    id: format!("{frame_id}:progress"),
                    event: sse_event,
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

    if let Err(e) = &run_result {
        // Issue #124: mark the Langfuse trace failed on provider/transport errors.
        langfuse_run.finish(None, Some(&e.to_string())).await;
    }
    // Issue #115: snapshot the failed turn's spend before the error is mapped
    // — the runtime keeps it out of band because `Err` cannot carry it.
    let failed_usage = runtime.last_failed_usage();
    let outcome = run_result.map_err(|e| {
        record_run_failed(&state.metrics, &failed_usage);
        // Issue #123: as in `/run`, only a failure of the LLM call itself
        // counts against readiness.
        if e.is_llm_failure() {
            record_llm_failure(&state.metrics);
        }
        map_run_error(&e)
    })?;
    // Issue #124: close the run trace with its terminal finish reason.
    langfuse_run
        .finish(Some(&outcome.finish_reason.to_string()), None)
        .await;

    // Update per-session token counters and global metrics.
    prompt_tokens_arc.fetch_add(outcome.total_usage.prompt_tokens as u64, Ordering::Relaxed);
    completion_tokens_arc.fetch_add(
        outcome.total_usage.completion_tokens as u64,
        Ordering::Relaxed,
    );
    record_run_success(&state.metrics, outcome.steps, &outcome.total_usage);
    // Issue #123: an interrupted turn may have been cancelled before the
    // provider ever answered, so it must not clear the readiness streak.
    if !matches!(
        &outcome.finish_reason,
        crate::agent::FinishReason::Cancelled
    ) {
        record_llm_success(&state.metrics);
    }

    // Extract the last assistant message from the runtime's transcript.
    let last_assistant = runtime
        .transcript()
        .iter()
        .rev()
        .find(|m| m.role == crate::message::Role::Assistant)
        .map(|m| m.content.clone())
        .unwrap_or_default();

    // Issue #105: optional outbound delivery of this turn's result.
    // Non-fatal from the turn's perspective: a delivery failure is reported
    // via `notify_result`, never as a failed request (the agent work
    // already happened). Delivery stays on this path because that field
    // *is* the outcome — spawning it away would drop it — but it is
    // bounded by the notifier's 15 s request timeout and reuses the
    // process-wide client.
    let notify_result = match &body.notify {
        Some(target) => {
            let payload = crate::notify::NotifyPayload {
                session_id: &id,
                source: "session:message",
                finish_reason: &outcome.finish_reason.to_string(),
                final_text: Some(&last_assistant),
            };
            Some(crate::notify::notify_best_effort(
                crate::notify::shared_notifier(),
                target,
                &payload,
            ))
        }
        None => None,
    };

    Ok(Json(SessionMessageResponse {
        role: "assistant".into(),
        content: last_assistant,
        notify_result,
    }))
}

// ── SSE endpoint ─────────────────────────────────────────────────────────

/// GET /sessions/:id/events — subscribe to SSE stream of agent events.
///
/// Issue #85: the stream carries the session's transcript-derived events, so
/// only the session's owner (or an admin) may subscribe.
pub(super) async fn session_events(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AuthIdentity>,
    Path(id): Path<String>,
) -> Result<Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    // Verify session exists (and that the caller may see it)
    {
        let sessions_lock = state.host.sessions();
        let sessions = sessions_lock.read().await;
        match sessions.get(&id) {
            Some(session) => ensure_access(&identity, session)?,
            None => return Err(ApiError::not_found("session not found")),
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
        Ok(SseFrame { id, event }) => {
            let event_type = match &event {
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
            let data = serde_json::to_string(&event).unwrap_or_default();
            // Issue #117: the frame carries its timeline id so clients can
            // order/dedupe across turns and know where they left off.
            Some(Ok::<Event, Infallible>(
                Event::default().id(id).event(event_type).data(data),
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
///
/// The resulting frame's `id:` (issue #117) is the originating event's
/// envelope key — `<ts_ms>-<turn>-<seq>` — not the per-turn `step` number,
/// which restarts every turn. Within one turn the `step`-keyed deltas are
/// still unambiguous; across a goal-loop's turns, key on the frame `id`.
pub fn map_agent_event(event: &AgentEvent) -> Option<SseEvent> {
    match event {
        // Streaming token deltas — clients reconstruct the final text by
        // concatenating deltas keyed on `step` (see the frame `id:` for
        // cross-turn ordering).
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

/// Build the `tool_progress` frame for a finished tool call.
///
/// Issue #118: `elapsed_ms` is the duration the runtime measured for *this*
/// tool, forwarded verbatim. The HTTP layer used to derive it from the
/// `ToolCall` / `ToolResult` arrival times, which made every tool of a step
/// report the same number — results are emitted only after the whole batch
/// finished — and folded the `TuiPermissionHook` approval wait into it.
fn tool_progress_event(event: &AgentEvent) -> Option<SseEvent> {
    match event {
        AgentEvent::ToolResult {
            id,
            name,
            duration_ms,
            ..
        } => Some(SseEvent::ToolProgress {
            tool_use_id: id.clone(),
            tool_name: name.clone(),
            elapsed_ms: *duration_ms,
        }),
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

    // ── Per-thread run fence (issue #57 §④) ─────────────────────────────
    // At most one in-flight run per thread. Mobile retries / double
    // submits used to run two drivers concurrently against one transcript
    // (measured lost-update); refuse the second run instead of queueing
    // it — a queued duplicate would run the same prompt twice. The guard
    // is released when the driver task finishes (or on unwind).
    //
    // Ordering invariant (restored from main's monolith): the fence MUST
    // close BEFORE `prepare_run` touches disk. The resume branch of
    // `prepare_run` rewrites the persisted transcript
    // (`apply_resume_tool_results`) and consumes the open-interrupt store
    // (`clear_open_interrupts`); fencing first means a duplicate POST that
    // races the tail of an in-flight run is refused with 409 while the
    // thread's on-disk state is still untouched — not after its resume
    // payload was already spliced in and the interrupts cleared.
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

    // ── Transport-free prepare: resume/interrupt state machine ─────────
    // (Runs after the fence above — see the ordering invariant there.)
    let prepared = super::agui::prepare_run(super::agui::AguiRunInput {
        workspace: &state.config.workspace,
        input: &input,
    })
    .map_err(agui_prepare_error_response)?;

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

    // Issue #68: per-request system prompt. Priority: explicit
    // `systemPrompt` field > `forwardedProps.systemPrompt` (the standard
    // AG-UI slot for app data) > process-level `state.config.system_prompt`;
    // `appendSystemPrompt` (either form) appends instead of replacing and
    // is ignored when a replace-level prompt was given — the same fallback
    // chain the REST channels (`/run`, `/sessions`) already implement.
    // This is *business/tenant* configuration (the caller is the C-end
    // integrator), not end-user input; `assemble_system_prompt` still
    // appends project context + the coordinator note on top, and the skill
    // catalog ships per-turn as a system-reminder, so the server-owned
    // prompt parts cannot be overridden from the request body.
    let request_prompt = input
        .system_prompt
        .clone()
        .or_else(|| forwarded_props_str(&input, "systemPrompt"))
        .or_else(|| {
            input
                .state
                .as_ref()
                .and_then(|s| s.get("systemPrompt"))
                .and_then(|v| v.as_str().map(str::to_string))
        });
    let request_append = input
        .append_system_prompt
        .clone()
        .or_else(|| forwarded_props_str(&input, "appendSystemPrompt"))
        .or_else(|| {
            input
                .state
                .as_ref()
                .and_then(|s| s.get("appendSystemPrompt"))
                .and_then(|v| v.as_str().map(str::to_string))
        });
    let base_prompt = match request_prompt {
        Some(s) if !s.trim().is_empty() => s,
        _ => {
            let mut p = state.config.system_prompt.clone();
            if let Some(extra) = request_append {
                p.push('\n');
                p.push_str(&extra);
            }
            p
        }
    };

    // Common system-prompt assembly: project context + skill index +
    // coordinator/sub_agent note (when enabled).
    let assembled_system_prompt = crate::assemble_system_prompt(
        &base_prompt,
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
    // Issue #31 §2: inject the `<environment>` segment only when the
    // session's transport is a real sandbox (non-local capabilities) —
    // same parity the REST channels apply.
    let (system_prompt, prompt_segments) = inject_environment_segment(
        assembled_system_prompt.full,
        assembled_system_prompt.segments,
        &tool_registry,
    );

    // Issue #127: the AG-UI channel runs under the server-default preset —
    // the same object the REST channels resolve, so thresholds and
    // re-injection cannot drift between channels.
    let preset = crate::preset::resolve_session(
        None,
        &state.config,
        &crate::preset::PresetEnv::from_process(),
    )
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                status: "error".into(),
                error: e.to_string(),
            }),
        )
    })?;

    let (runtime, hooks) = super::agui::build_agui_runtime(
        &state.config.workspace,
        &input.thread_id,
        super::agui::AguiRuntimeDeps {
            llm: state.provider.clone(),
            tool_registry,
            system_prompt,
            prompt_segments,
            max_steps: state.config.max_steps,
            seed_transcript: prepared.seed_transcript,
            interrupt_before: input.interrupt_before.as_deref().unwrap_or(&[]),
            client_tools: &input.tools,
            preset,
            wall_timeout_secs: state.config.wall_timeout_secs,
            storage: state.storage.clone(),
            skills: state.skills.clone(),
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

    let stream = tokio_stream::wrappers::UnboundedReceiverStream::new(sse_rx).map(|ev| {
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

/// Read a string field out of `input.forwardedProps` (issue #68) — the
/// AG-UI-spec slot for app-provided data, which the server previously
/// parsed and dropped.
fn forwarded_props_str(input: &agui_protocol::RunAgentInput, key: &str) -> Option<String> {
    input
        .forwarded_props
        .as_ref()?
        .get(key)?
        .as_str()
        .map(str::to_string)
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
    let tokens_wasted_on_failure_total = metrics
        .tokens_wasted_on_failure_total
        .load(Ordering::Relaxed);
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
    // Issue #123: capacity / data-loss gauges that were previously invisible.
    // `sse_clients` is derived from each per-session channel's subscriber
    // count (`event_channels` has no other reader of `receiver_count`); the
    // two `persist_failures` / `sessions_evicted` counters are bumped on the
    // reaper, DELETE and shutdown paths. AG-UI runs have no `SessionState`
    // row, so `agui_active_runs` is the only place they are counted.
    let sse_clients: u64 = state
        .event_channels
        .read()
        .await
        .values()
        .map(|tx| tx.receiver_count() as u64)
        .sum();
    let agui_runs = state
        .agui_active_runs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len() as u64;
    let persist_failures = metrics.persist_failures.load(Ordering::Relaxed);
    let sessions_evicted = metrics.sessions_evicted.load(Ordering::Relaxed);
    let last_llm_success_ms = metrics.last_llm_success_ms.load(Ordering::Relaxed);
    let llm_failures_consecutive = metrics.llm_failures_consecutive.load(Ordering::Relaxed);

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
         # HELP recursive_tokens_wasted_on_failure_total Tokens burned by failed agent runs\n\
         # TYPE recursive_tokens_wasted_on_failure_total counter\n\
         recursive_tokens_wasted_on_failure_total {tokens_wasted_on_failure_total}\n\
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
         recursive_rate_limits_rejected_total {rate_limits_rejected}\n\
         # HELP recursive_sse_clients SSE subscribers across all session channels\n\
         # TYPE recursive_sse_clients gauge\n\
         recursive_sse_clients {sse_clients}\n\
         # HELP recursive_agui_runs AG-UI runs currently in flight (issue #123)\n\
         # TYPE recursive_agui_runs gauge\n\
         recursive_agui_runs {agui_runs}\n\
         # HELP recursive_persist_failures Transcript/memory persists that failed or were skipped with data loss (issue #123)\n\
         # TYPE recursive_persist_failures counter\n\
         recursive_persist_failures {persist_failures}\n\
         # HELP recursive_sessions_evicted Total sessions removed by the idle reaper (issue #123)\n\
         # TYPE recursive_sessions_evicted counter\n\
         recursive_sessions_evicted {sessions_evicted}\n\
         # HELP recursive_llm_last_success_ms Milliseconds since server epoch of the last successful run (0 = never; issue #123)\n\
         # TYPE recursive_llm_last_success_ms gauge\n\
         recursive_llm_last_success_ms {last_llm_success_ms}\n\
         # HELP recursive_llm_failures_consecutive Failed LLM runs since the last success; decayed by /readyz once the streak is older than the readiness window (issue #123)\n\
         # TYPE recursive_llm_failures_consecutive gauge\n\
         recursive_llm_failures_consecutive {llm_failures_consecutive}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AgentEvent, EventSink};
    use crate::http::SseEvent;
    use std::collections::HashMap;

    /// Issue #117: every SSE frame derived from an event carries that event's
    /// envelope key as its `id:` — so a client can order/dedupe frames across
    /// turns (the per-turn `step` alone cannot).
    #[tokio::test]
    async fn sse_frame_id_is_the_event_envelope_key() {
        let seq = Arc::new(AtomicU64::new(0));
        let (sink, mut rx) =
            crate::event::EnvelopeSink::with_correlation(Some("sess-9".into()), 3, seq.clone());
        sink.emit(AgentEvent::ToolCall {
            name: "Bash".into(),
            id: "tc-1".into(),
            arguments: "{}".into(),
            step: 0,
        })
        .await;

        let envelope = rx.recv().await.expect("envelope");
        assert_eq!(envelope.meta.turn, 3);
        assert_eq!(envelope.meta.session_id.as_deref(), Some("sess-9"));
        assert_eq!(envelope.meta.seq, 0);

        let frame = SseFrame {
            id: envelope.id(),
            event: map_agent_event(&envelope.event).expect("tool_call maps to an SSE event"),
        };
        assert!(matches!(frame.event, SseEvent::ToolCall { .. }));
        assert_eq!(
            frame.id,
            format!("{}-3-0", envelope.meta.ts_ms),
            "frame id must be <ts_ms>-<turn>-<seq>"
        );
    }

    /// Issue #100: a `RateLimited` that exhausts the step-level retry budget
    /// must still map to 429 + `Retry-After`. The `/run` integration test now
    /// covers the recover path (a transient 429 is retried), so this pins the
    /// error mapping itself.
    #[test]
    fn map_run_error_maps_rate_limited_to_429_with_retry_after() {
        let err = crate::error::Error::RateLimited {
            provider: "mock".into(),
            retry_after_ms: 1234,
        };
        let api = map_run_error(&err);
        assert_eq!(api.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(api.retry_after_secs, Some(1), "1234ms floors to 1s");
        assert!(api.message.contains("rate limited"));

        let cancelled = map_run_error(&crate::error::Error::Cancelled);
        assert_eq!(cancelled.status, StatusCode::SERVICE_UNAVAILABLE);

        let llm = map_run_error(&crate::error::Error::Llm {
            provider: "p".into(),
            message: "HTTP 500 Internal Server Error: boom".into(),
        });
        assert_eq!(llm.status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// Goal-393: `build_session_runtime` must install the same context
    /// management the CLI gets — compactor (auto threshold from the model),
    /// microcompactor (opt-in), transcript cap (env). Asserted at the
    /// builder level: `AgentRuntime` deliberately has no public accessors.
    #[test]
    fn build_session_runtime_installs_compactor_and_transcript_cap() {
        // The env matrix itself lives in
        // `src/runtime/context_management.rs` (single merged test — env is
        // process-global). Here: one representative configuration.
        let saved_threshold = std::env::var("RECURSIVE_COMPACT_THRESHOLD").ok();
        let saved_cap = std::env::var("RECURSIVE_MAX_TRANSCRIPT_CHARS").ok();
        let _guard = crate::test_util::env_lock();
        std::env::set_var("RECURSIVE_COMPACT_THRESHOLD", "7777");
        std::env::set_var("RECURSIVE_MAX_TRANSCRIPT_CHARS", "99999");

        let config = crate::config::Config::from_env().expect("config");
        let state = crate::http::AppState {
            tools: vec![],
            tool_registry: ToolRegistry::default(),
            config,
            provider: Arc::new(crate::llm::MockProvider::new(vec![])),
            host: Arc::new(crate::session_host::SessionHost::new(
                Duration::from_secs(0),
                crate::http::AdmissionGate::new(
                    1,
                    Duration::ZERO,
                    Arc::new(AtomicU64::new(0)),
                    Arc::new(AtomicU64::new(0)),
                ),
            )),
            event_channels: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            metrics: Arc::new(crate::http::Metrics::default()),
            slash_commands: Arc::new(Vec::new()),
            rate_limiter: crate::http::RateLimiter::new(10, 1.0),
            skills: vec![],
            storage: std::sync::Arc::new(crate::storage::LocalStorageBackend::new(
                std::env::temp_dir()
                    .join(format!("recursive-handlers-test-{}", std::process::id())),
            )),
            agui_active_runs: Arc::new(std::sync::Mutex::new(HashMap::new())),
        };

        let preset = resolve_session_preset(None, &state.config).expect("preset");
        let builder = build_session_runtime(
            &state,
            ToolRegistry::default(),
            "sys".to_string(),
            crate::system_prompt::PromptSegments::default(),
            16,
            &preset,
            SessionOverrides::default(),
        );
        let compactor = builder.compactor_for_test().expect("compactor installed");
        assert_eq!(compactor.threshold_chars, 7777);
        assert_eq!(builder.max_transcript_chars_for_test(), Some(99999));

        if let Some(v) = saved_threshold {
            std::env::set_var("RECURSIVE_COMPACT_THRESHOLD", v);
        } else {
            std::env::remove_var("RECURSIVE_COMPACT_THRESHOLD");
        }
        if let Some(v) = saved_cap {
            std::env::set_var("RECURSIVE_MAX_TRANSCRIPT_CHARS", v);
        } else {
            std::env::remove_var("RECURSIVE_MAX_TRANSCRIPT_CHARS");
        }
    }

    /// #74 拆单 3/3: `build_session_runtime` must hand the merged skill
    /// catalog (`AppState.skills` — directory + service-level SkillSource
    /// entries) to the runtime; the kernel ships it as the per-turn
    /// `<system-reminder>` and drives Globs-mode injection from it.
    /// Without the wiring the catalog is computed at startup but never
    /// reaches any run's context.
    #[test]
    fn build_session_runtime_installs_the_skill_catalog() {
        let config = crate::config::Config::from_env().expect("config");
        let state = crate::http::AppState {
            tools: vec![],
            tool_registry: ToolRegistry::default(),
            config,
            provider: Arc::new(crate::llm::MockProvider::new(vec![])),
            host: Arc::new(crate::session_host::SessionHost::new(
                Duration::from_secs(0),
                crate::http::AdmissionGate::new(
                    1,
                    Duration::ZERO,
                    Arc::new(AtomicU64::new(0)),
                    Arc::new(AtomicU64::new(0)),
                ),
            )),
            event_channels: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            metrics: Arc::new(crate::http::Metrics::default()),
            slash_commands: Arc::new(Vec::new()),
            rate_limiter: crate::http::RateLimiter::new(10, 1.0),
            skills: vec![crate::skills::skill_from_content(
                "remote-skill",
                "---\nname: remote-skill\ndescription: from the wire\n---\n\nBody",
                Vec::new(),
            )],
            storage: std::sync::Arc::new(crate::storage::LocalStorageBackend::new(
                std::env::temp_dir()
                    .join(format!("recursive-handlers-test-{}", std::process::id())),
            )),
            agui_active_runs: Arc::new(std::sync::Mutex::new(HashMap::new())),
        };

        let preset = resolve_session_preset(None, &state.config).expect("preset");
        let builder = build_session_runtime(
            &state,
            ToolRegistry::default(),
            "sys".to_string(),
            crate::system_prompt::PromptSegments::default(),
            16,
            &preset,
            SessionOverrides::default(),
        );
        let skills = builder.skills_for_test();
        assert_eq!(skills.len(), 1, "catalog must ride into the runtime");
        assert_eq!(skills[0].name, "remote-skill");
        assert!(
            skills[0].body.is_some(),
            "service-level skills stay content-backed (never on disk)"
        );
    }

    // ── Issue #127: agent preset observability ───────────────────────────

    /// Build the `AppState` the preset tests need: a standard tool registry
    /// (so the session has a shared read state for re-injection) and a
    /// temp-dir storage backend.
    fn preset_test_state(root: &std::path::Path) -> crate::http::AppState {
        crate::http::AppState {
            tools: vec![],
            tool_registry: crate::tools::build_standard_tools(root, &[], 60),
            config: crate::config::Config::from_env().expect("config"),
            provider: Arc::new(crate::llm::MockProvider::new(vec![])),
            host: Arc::new(crate::session_host::SessionHost::new(
                Duration::from_secs(0),
                crate::http::AdmissionGate::new(
                    1,
                    Duration::ZERO,
                    Arc::new(AtomicU64::new(0)),
                    Arc::new(AtomicU64::new(0)),
                ),
            )),
            event_channels: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            metrics: Arc::new(crate::http::Metrics::default()),
            slash_commands: Arc::new(Vec::new()),
            rate_limiter: crate::http::RateLimiter::new(10, 1.0),
            skills: vec![],
            storage: Arc::new(crate::storage::LocalStorageBackend::new(
                root.join("storage"),
            )),
            agui_active_runs: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Acceptance 1 (HTTP half): a `standard`-preset session is assembled
    /// exactly as the preset declares — context management AND post-compaction
    /// re-injection. The CLI and TUI crates assert the same reference in their
    /// own tests, so either channel drifting fails a test.
    ///
    /// Before #127 the HTTP channel wired NO re-injectors at all (only the CLI
    /// did), so this also pins the drift that motivated the issue.
    #[test]
    fn http_assembly_matches_the_resolved_standard_preset() {
        let _guard = crate::test_util::env_lock();
        std::env::set_var("RECURSIVE_API_KEY", "test-key");
        std::env::set_var("RECURSIVE_MODEL", "test-model");
        std::env::remove_var("RECURSIVE_AGENT_PRESET");
        for var in [
            "RECURSIVE_COMPACT_THRESHOLD",
            "RECURSIVE_MAX_TRANSCRIPT_CHARS",
            "RECURSIVE_MICROCOMPACT_TRIGGER",
            "RECURSIVE_REINJECT_FILES",
            "RECURSIVE_REINJECT_SKILLS",
        ] {
            std::env::remove_var(var);
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = preset_test_state(tmp.path());
        let preset = resolve_session_preset(Some("standard"), &state.config).expect("preset");

        let builder = build_session_runtime(
            &state,
            crate::tools::build_standard_tools(tmp.path(), &[], 60),
            "sys".to_string(),
            crate::system_prompt::PromptSegments::default(),
            16,
            &preset,
            SessionOverrides::default(),
        );

        assert_eq!(builder.preset_id(), Some("standard"));
        assert_eq!(
            builder.context_management_facts(),
            preset.context,
            "the assembled runtime must be exactly what the preset resolved to"
        );
        assert!(
            builder
                .context_management_facts()
                .reinject_recent_files
                .is_some(),
            "the HTTP channel must now re-inject recently-read files like the CLI"
        );
        assert!(
            !builder.with_plan_mode_tools_for_test(),
            "an HTTP session has no live human for the plan-approval prompt"
        );
    }

    /// Acceptance 2: `POST /sessions`'s `preset` is persisted (the #98 path, so
    /// a restart restores it) and echoed by `GET /sessions/:id`. An unknown id
    /// is a 400 with the known ids — never a silent fallback.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // the std env lock only guards same-crate tests
    async fn session_preset_is_persisted_and_echoed() {
        let _guard = crate::test_util::env_lock();
        std::env::set_var("RECURSIVE_API_KEY", "test-key");
        std::env::set_var("RECURSIVE_MODEL", "test-model");
        std::env::remove_var("RECURSIVE_AGENT_PRESET");

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = Arc::new(preset_test_state(tmp.path()));
        let body = |preset: Option<&str>| CreateSessionRequest {
            system_prompt: None,
            append_system_prompt: None,
            session_name: None,
            max_steps: None,
            thinking_budget: None,
            permission_mode: None,
            max_budget_usd: None,
            preset: preset.map(str::to_string),
        };

        let (status, Json(created)) = create_session(
            State(state.clone()),
            Extension(AuthIdentity::local()),
            Json(body(Some("standard"))),
        )
        .await
        .expect("create session");
        assert_eq!(status, StatusCode::CREATED);

        // GET echoes the effective preset, read off the live runtime.
        let Json(detail) = get_session(
            State(state.clone()),
            Extension(AuthIdentity::local()),
            Path(created.id.clone()),
        )
        .await
        .expect("session detail");
        assert_eq!(detail.preset.as_deref(), Some("standard"));

        // ...and it is in the #98 metadata blob, so a restart restores it.
        let meta = state
            .storage
            .load_memory(&crate::http::cold_load::session_meta_key(&created.id))
            .await
            .expect("storage read")
            .expect("metadata persisted");
        assert!(
            meta.contains("\"preset\":\"standard\""),
            "preset must ride the #98 metadata path, got: {meta}"
        );

        // An unknown id is rejected with the list of what does exist.
        let err = create_session(
            State(state.clone()),
            Extension(AuthIdentity::local()),
            Json(body(Some("no-such-preset"))),
        )
        .await
        .expect_err("unknown preset must be rejected");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(
            err.message.contains("standard"),
            "the 400 must name the known presets: {}",
            err.message
        );
    }

    /// `GET /presets` exposes the capability inventory — including the
    /// capabilities that exist but default to off, which is the whole point of
    /// listing them (issue #127, item 4).
    #[tokio::test]
    async fn list_presets_exposes_the_capability_inventory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = Arc::new(preset_test_state(tmp.path()));
        let Json(presets) = list_presets(State(state)).await;
        assert_eq!(presets.len(), crate::preset::builtin().len());
        let standard = presets
            .iter()
            .find(|p| p.id == "standard")
            .expect("standard is built in");
        assert!(
            standard.capabilities.iter().any(|c| {
                c.name == "proactive-tool-result-pruning"
                    && c.default == crate::preset::CapabilityDefault::Disabled
            }),
            "a disabled-by-default capability must be discoverable here"
        );
        assert_eq!(standard.resolved.id, "standard");
    }

    // ── SDK Phase B: tool_progress forwarder ─────────────────────────────

    /// Issue #118: the forwarder must emit ToolProgress carrying the duration
    /// the runtime measured for that tool — not a value re-derived from event
    /// arrival times.
    #[test]
    fn tool_progress_forwards_runtime_duration() {
        let result_event = AgentEvent::ToolResult {
            id: "tc-1".to_string(),
            name: "Bash".to_string(),
            output: "ok".to_string(),
            step: 0,
            is_error: false,
            duration_ms: 137,
        };

        let SseEvent::ToolProgress {
            tool_use_id,
            tool_name,
            elapsed_ms,
        } = tool_progress_event(&result_event).expect("tool_progress expected")
        else {
            panic!("expected ToolProgress");
        };
        assert_eq!(tool_use_id, "tc-1");
        assert_eq!(tool_name, "Bash");
        assert_eq!(elapsed_ms, 137);
    }

    /// Only `ToolResult` produces a `tool_progress` frame; the ToolCall that
    /// preceded it must not.
    #[test]
    fn tool_progress_only_for_tool_result() {
        let call_event = AgentEvent::ToolCall {
            name: "Bash".to_string(),
            id: "tc-1".to_string(),
            arguments: "{}".to_string(),
            step: 0,
        };
        assert!(tool_progress_event(&call_event).is_none());
        assert!(tool_progress_event(&AgentEvent::TurnFinished {
            reason: "no_more_tool_calls".to_string(),
            steps: 1,
        })
        .is_none());
    }

    /// A call rejected before dispatch (duration 0) is forwarded as 0 — the
    /// forwarder must not substitute a batch wall clock.
    #[test]
    fn tool_progress_forwards_zero_for_undispatched_call() {
        let result_event = AgentEvent::ToolResult {
            id: "tc-orphan".to_string(),
            name: "Read".to_string(),
            output: "ERROR: denied".to_string(),
            step: 0,
            is_error: true,
            duration_ms: 0,
        };
        let SseEvent::ToolProgress { elapsed_ms, .. } =
            tool_progress_event(&result_event).expect("tool_progress expected")
        else {
            panic!("expected ToolProgress");
        };
        assert_eq!(elapsed_ms, 0);
    }

    /// Goal-268 + Goal-H J2: /agui must respect run_semaphore. The
    /// handler uses `try_acquire_run` (J2) so a saturated (0-permit)
    /// semaphore returns 503 SERVICE_UNAVAILABLE **immediately** rather
    /// than blocking forever on `acquire_owned().await`. A 0-permit
    /// `Semaphore` is the natural test fixture — no `close()` workaround
    /// needed (the previous form tested a *closed* semaphore, which is a
    /// different code path inside `try_acquire_owned`).
    #[tokio::test]
    async fn agui_run_respects_run_semaphore() {
        use crate::llm::MockProvider;
        use crate::tools::ToolRegistry;
        use tokio::sync::Semaphore;

        std::env::set_var("RECURSIVE_API_KEY", "test-key");
        std::env::set_var("RECURSIVE_MODEL", "test-model");
        let config = crate::config::Config::from_env().unwrap();

        // 0-permit admission gate: every `try_acquire_run` call
        // returns `TryAcquireError::NoPermits` immediately.
        let metrics = Arc::new(crate::http::Metrics::default());
        let host = Arc::new(crate::session_host::SessionHost::new(
            Duration::from_secs(3600),
            crate::http::AdmissionGate::from_semaphore(
                Arc::new(Semaphore::new(0)),
                Duration::ZERO,
                Arc::clone(&metrics.runs_waiting),
                Arc::clone(&metrics.runs_in_flight),
            ),
        ));

        let state = Arc::new(crate::http::AppState {
            tools: vec![],
            tool_registry: ToolRegistry::default(),
            config,
            provider: Arc::new(MockProvider::new(vec![])),
            host,
            event_channels: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            metrics,
            slash_commands: Arc::new(vec![]),
            rate_limiter: crate::http::RateLimiter::new(10, 1.0),
            skills: vec![],
            storage: std::sync::Arc::new(crate::storage::LocalStorageBackend::new(
                std::env::temp_dir()
                    .join(format!("recursive-handlers-test-{}", std::process::id())),
            )),
            agui_active_runs: Arc::new(std::sync::Mutex::new(HashMap::new())),
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

    // ── Issue #68: /agui per-request system prompt ────────────────────

    /// Shared fixture for the issue-#68 acceptance tests: a full AppState
    /// with a MockProvider that records the messages it was shown, wired
    /// through the real router so `POST /agui` is exercised end-to-end.
    /// `config_system_prompt` becomes the process-level fallback prompt.
    ///
    /// Returns (base_url, provider, workspace_dir). The caller must keep
    /// the workspace tempdir alive for the duration of the test.
    /// (clippy::await_holding_lock: the std env guard spans the two
    /// `TcpListener::bind`-adjacent awaits inside this test helper; the
    /// lock is a std Mutex held by the same-crate test task only.)
    #[allow(clippy::await_holding_lock)]
    async fn agui_prompt_fixture(
        config_system_prompt: &str,
    ) -> (url::Url, Arc<crate::llm::MockProvider>, tempfile::TempDir) {
        let _env = crate::test_util::env_lock();
        let ws = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        std::env::set_var("RECURSIVE_WORKSPACE", ws.path());
        std::env::set_var("RECURSIVE_HOME", home.path());
        // The env guard is released when this helper RETURNS — before the
        // caller's `agui_post` drives the actual HTTP run — so nothing here
        // can keep `RECURSIVE_SESSIONS_DIR` pinned for the run itself. Pin
        // it to an isolated root explicitly: an inherited (or empty)
        // override makes `user_sessions_dir` resolve CWD-relative and
        // session transcripts leak into the repo (the `var-folders-*` /
        // `agui-*` artifact dirs this fixture once committed).
        std::env::set_var(
            "RECURSIVE_SESSIONS_DIR",
            home.path().join("agui-prompt-sessions"),
        );
        std::env::set_var("RECURSIVE_API_KEY", "test-key");
        std::env::set_var("RECURSIVE_MODEL", "test-model");
        std::env::set_var("RECURSIVE_HTTP_AUTH_INSECURE_OK", "1");

        let config = crate::config::Config::from_env().expect("config");
        // from_env builds the process-level prompt from the default
        // template + memory layers; pin the *base* we assert against by
        // overwriting it after construction (same as the handler reads it).
        let mut config = config;
        config.system_prompt =
            format!("{config_system_prompt}\n\n---\n\n# Memory summary\n\n- process-level marker");

        let provider = Arc::new(crate::llm::MockProvider::new(vec![
            crate::llm::Completion::default(),
        ]));
        let metrics = Arc::new(crate::http::Metrics::default());
        let state = AppState {
            tools: vec![],
            tool_registry: ToolRegistry::default(),
            config,
            provider: provider.clone(),
            event_channels: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            metrics,
            slash_commands: Arc::new(vec![]),
            host: Arc::new(crate::session_host::SessionHost::new(
                Duration::from_secs(3600),
                crate::http::AdmissionGate::new(
                    8,
                    Duration::ZERO,
                    Arc::new(std::sync::atomic::AtomicU64::new(0)),
                    Arc::new(std::sync::atomic::AtomicU64::new(0)),
                ),
            )),
            rate_limiter: crate::http::RateLimiter::new(100, 1.0),
            skills: vec![],
            agui_active_runs: Arc::new(std::sync::Mutex::new(HashMap::new())),
            storage: Arc::new(crate::storage::LocalStorageBackend::new(
                std::env::temp_dir()
                    .join(format!("recursive-agui-prompt-test-{}", std::process::id())),
            )),
        };
        let app = crate::http::build_router_with_auth_and_rate_limit(
            state,
            crate::http::auth::AuthConfig::default(),
            crate::http::RateLimiter::new(100, 1.0),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let base = url::Url::parse(&format!("http://{addr}")).expect("base url");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (base, provider, ws)
    }

    /// POST one /agui run and drain the SSE body.
    async fn agui_post(base: &url::Url, body: serde_json::Value) {
        let client = reqwest::Client::new();
        let url = base.join("agui").expect("agui url");
        let resp = client
            .post(url)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .expect("post /agui");
        assert!(resp.status().is_success(), "POST /agui failed: {resp:?}");
        let _ = resp.bytes().await.expect("sse body");
    }

    /// The system message the provider received for the single run.
    fn assert_system_msg(provider: &crate::llm::MockProvider) -> String {
        let calls = provider.calls();
        assert_eq!(calls.len(), 1, "expected exactly one provider call");
        calls[0]
            .iter()
            .find(|m| m.role == Role::System)
            .map(|m| m.content.clone())
            .expect("provider request must carry a system message")
    }

    /// Acceptance 1a: `systemPrompt` on the request body overrides the
    /// process-level prompt.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // std env lock is fine: only same-crate tests contend
    async fn agui_explicit_system_prompt_overrides_process_level() {
        let (base, provider, _ws) = agui_prompt_fixture("PROCESS-LEVEL-PROMPT-MARKER").await;
        agui_post(
            &base,
            serde_json::json!({
                "threadId": "prompt-thread-a",
                "runId": "r1",
                "systemPrompt": "TENANT-A CUSTOM PROMPT",
                "messages": [{"id": "m1", "role": "user", "content": "hi"}],
            }),
        )
        .await;
        let sys = assert_system_msg(&provider);
        assert!(
            sys.contains("TENANT-A CUSTOM PROMPT"),
            "explicit systemPrompt must reach the provider; got: {sys}"
        );
        assert!(
            !sys.contains("PROCESS-LEVEL-PROMPT-MARKER"),
            "process-level prompt must be fully replaced, not appended; got: {sys}"
        );
    }

    /// Acceptance 1b: `forwardedProps.systemPrompt` — the standard AG-UI
    /// slot for app data, previously parsed and dropped — also overrides.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn agui_forwarded_props_system_prompt_overrides_process_level() {
        let (base, provider, _ws) = agui_prompt_fixture("PROCESS-LEVEL-PROMPT-MARKER").await;
        agui_post(
            &base,
            serde_json::json!({
                "threadId": "prompt-thread-b",
                "runId": "r1",
                "forwardedProps": {"systemPrompt": "TENANT-B FORWARDED PROMPT"},
                "messages": [{"id": "m1", "role": "user", "content": "hi"}],
            }),
        )
        .await;
        let sys = assert_system_msg(&provider);
        assert!(
            sys.contains("TENANT-B FORWARDED PROMPT"),
            "forwardedProps.systemPrompt must reach the provider; got: {sys}"
        );
        assert!(
            !sys.contains("PROCESS-LEVEL-PROMPT-MARKER"),
            "process-level prompt must be replaced; got: {sys}"
        );
    }

    /// Acceptance 1c: `state.systemPrompt` is the last override slot.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn agui_state_system_prompt_overrides_process_level() {
        let (base, provider, _ws) = agui_prompt_fixture("PROCESS-LEVEL-PROMPT-MARKER").await;
        agui_post(
            &base,
            serde_json::json!({
                "threadId": "prompt-thread-c",
                "runId": "r1",
                "state": {"systemPrompt": "TENANT-C STATE PROMPT"},
                "messages": [{"id": "m1", "role": "user", "content": "hi"}],
            }),
        )
        .await;
        let sys = assert_system_msg(&provider);
        assert!(
            sys.contains("TENANT-C STATE PROMPT"),
            "state.systemPrompt must reach the provider; got: {sys}"
        );
    }

    /// Acceptance 1d: no prompt in the request → process-level prompt,
    /// byte-identical to the pre-#68 behaviour (plus the assembled
    /// layers the handler always appends).
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn agui_without_request_prompt_falls_back_to_process_level() {
        let (base, provider, _ws) = agui_prompt_fixture("PROCESS-LEVEL-FALLBACK-MARKER").await;
        agui_post(
            &base,
            serde_json::json!({
                "threadId": "prompt-thread-d",
                "runId": "r1",
                "forwardedProps": {"someAppData": {"unrelated": true}},
                "messages": [{"id": "m1", "role": "user", "content": "hi"}],
            }),
        )
        .await;
        let sys = assert_system_msg(&provider);
        assert!(
            sys.contains("PROCESS-LEVEL-FALLBACK-MARKER"),
            "no request prompt → process-level prompt must be used; got: {sys}"
        );
        assert!(
            !sys.contains("someAppData"),
            "unrelated forwardedProps must not leak into the prompt; got: {sys}"
        );
    }

    /// Acceptance 1e: `appendSystemPrompt` appends to the process-level
    /// prompt instead of replacing it.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn agui_append_system_prompt_appends_to_process_level() {
        let (base, provider, _ws) = agui_prompt_fixture("PROCESS-LEVEL-FALLBACK-MARKER").await;
        agui_post(
            &base,
            serde_json::json!({
                "threadId": "prompt-thread-e",
                "runId": "r1",
                "appendSystemPrompt": "EXTRA APPENDED RULES",
                "messages": [{"id": "m1", "role": "user", "content": "hi"}],
            }),
        )
        .await;
        let sys = assert_system_msg(&provider);
        assert!(
            sys.contains("PROCESS-LEVEL-FALLBACK-MARKER"),
            "append keeps the process-level base; got: {sys}"
        );
        assert!(
            sys.contains("EXTRA APPENDED RULES"),
            "appended text must be present; got: {sys}"
        );
        // Append lands at the very end of the base (after the memory
        // layers folded in at config build time) — identical to the
        // `/run` / `/sessions` append semantics.
        let append_pos = sys.find("EXTRA APPENDED RULES").expect("append present");
        let layer_pos = sys
            .find("process-level marker")
            .expect("base memory layer present");
        assert!(
            append_pos > layer_pos,
            "append must come after the base prompt content"
        );
    }

    /// Acceptance 2: two threads, different prompts, same process — each
    /// run's provider request carries its OWN prompt (per-request
    /// isolation, no cross-talk through the shared config).
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn agui_two_threads_different_prompts_are_isolated() {
        let (base, provider, _ws) = agui_prompt_fixture("PROCESS-LEVEL-PROMPT-MARKER").await;
        agui_post(
            &base,
            serde_json::json!({
                "threadId": "tenant-alpha",
                "runId": "r1",
                "systemPrompt": "ALPHA PROMPT",
                "messages": [{"id": "m1", "role": "user", "content": "hi"}],
            }),
        )
        .await;
        agui_post(
            &base,
            serde_json::json!({
                "threadId": "tenant-beta",
                "runId": "r2",
                "systemPrompt": "BETA PROMPT",
                "messages": [{"id": "m1", "role": "user", "content": "hi"}],
            }),
        )
        .await;
        let calls = provider.calls();
        assert_eq!(calls.len(), 2, "expected two provider calls");
        let sys_of = |call: &[crate::message::Message]| {
            call.iter()
                .find(|m| m.role == Role::System)
                .map(|m| m.content.clone())
                .expect("system message")
        };
        let (sys_a, sys_b) = (sys_of(&calls[0]), sys_of(&calls[1]));
        assert!(
            sys_a.contains("ALPHA PROMPT") && !sys_a.contains("BETA PROMPT"),
            "thread alpha must see only its own prompt; got: {sys_a}"
        );
        assert!(
            sys_b.contains("BETA PROMPT") && !sys_b.contains("ALPHA PROMPT"),
            "thread beta must see only its own prompt; got: {sys_b}"
        );
    }

    /// Acceptance 3: the server-owned suffix that `assemble_system_prompt`
    /// appends (project-context rules; skills ship per-turn as a reminder,
    /// represented here by the memory-layer text) must survive a
    /// request-level prompt override.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn agui_request_prompt_cannot_drop_server_owned_segments() {
        // Plant an AGENTS.md in the workspace so prepend_project_context
        // contributes its `# Project context` segment.
        let (base, provider, ws) = agui_prompt_fixture("PROCESS-LEVEL-PROMPT-MARKER").await;
        std::fs::write(
            ws.path().join("AGENTS.md"),
            "PROJECT CONTEXT SENTINEL FROM AGENTS MD",
        )
        .expect("write AGENTS.md");
        agui_post(
            &base,
            serde_json::json!({
                "threadId": "prompt-thread-g",
                "runId": "r1",
                "systemPrompt": "TENANT-G CUSTOM PROMPT",
                "messages": [{"id": "m1", "role": "user", "content": "hi"}],
            }),
        )
        .await;
        let sys = assert_system_msg(&provider);
        assert!(
            sys.contains("TENANT-G CUSTOM PROMPT"),
            "override must apply; got: {sys}"
        );
        assert!(
            sys.contains("PROJECT CONTEXT SENTINEL FROM AGENTS MD"),
            "server-assembled project context must survive the override; got: {sys}"
        );
        // The process-level config text was REPLACED (that is the point of
        // the override) — assert the tenant prompt sits where the base was.
        assert!(
            sys.find("# Project context").is_some(),
            "project-context header must be present; got: {sys}"
        );
    }

    /// Protocol level: unknown-tolerance is preserved — a payload WITHOUT
    /// the new fields round-trips exactly as before, and `systemPrompt`
    /// serialises as camelCase.
    #[test]
    fn run_agent_input_prompt_fields_camel_case_and_backward_compatible() {
        use agui_protocol::RunAgentInput;
        let old = serde_json::json!({
            "threadId": "t",
            "runId": "r",
            "messages": [],
            "tools": [],
            "context": [],
        });
        let parsed: RunAgentInput = serde_json::from_value(old.clone()).expect("old payload");
        assert!(parsed.system_prompt.is_none());
        assert!(parsed.append_system_prompt.is_none());
        // Round-trip: no new keys appear when unset (byte-compat serialise).
        let back = serde_json::to_value(&parsed).expect("serialise");
        assert_eq!(back, old, "unset prompt fields must not appear");
        // New fields serialise camelCase.
        let with_prompt: RunAgentInput = serde_json::from_value(serde_json::json!({
            "threadId": "t", "runId": "r",
            "messages": [], "tools": [], "context": [],
            "systemPrompt": "s", "appendSystemPrompt": "a",
        }))
        .expect("new payload");
        assert_eq!(with_prompt.system_prompt.as_deref(), Some("s"));
        assert_eq!(with_prompt.append_system_prompt.as_deref(), Some("a"));
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
            owner: None,
            tenant: None,
            runtime: runtime_arc.clone(),
            plan_approval_gate: Arc::new(crate::tools::plan_mode::PlanApprovalGate::new()),
            interrupt_token: Arc::new(tokio::sync::Mutex::new(None)),
            non_system_message_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            last_active_ms: Arc::new(AtomicU64::new(0)),
            prompt_tokens: Arc::new(AtomicU64::new(0)),
            completion_tokens: Arc::new(AtomicU64::new(0)),
            event_seq: Arc::new(AtomicU64::new(0)),
        };

        let sessions: HashMap<String, SessionState> = [(session_id.clone(), session)].into();
        let host = Arc::new(crate::session_host::SessionHost::new(
            Duration::from_secs(3600),
            crate::http::AdmissionGate::new(
                8,
                Duration::ZERO,
                Arc::new(std::sync::atomic::AtomicU64::new(0)),
                Arc::new(std::sync::atomic::AtomicU64::new(0)),
            ),
        ));
        host.sessions().write().await.extend(sessions);
        let state = Arc::new(AppState {
            tools: vec![],
            tool_registry: ToolRegistry::default(),
            config,
            provider,
            event_channels: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            metrics: Arc::new(crate::http::Metrics::default()),
            slash_commands: Arc::new(vec![]),
            host,
            rate_limiter: crate::http::RateLimiter::new(10, 1.0),
            skills: vec![],
            storage: std::sync::Arc::new(crate::storage::LocalStorageBackend::new(
                std::env::temp_dir()
                    .join(format!("recursive-handlers-test-{}", std::process::id())),
            )),
            agui_active_runs: Arc::new(std::sync::Mutex::new(HashMap::new())),
        });

        // Acquire the runtime mutex to simulate a busy runtime.
        let guard = runtime_arc.lock().await;

        // Call the handler while the mutex is held → should get 409
        // (Result::Err(ApiError::conflict(...).with_retry_after(5))).
        let resp = session_clear_goal(
            State(state.clone()),
            Extension(AuthIdentity::local()),
            Path(session_id.clone()),
        )
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
        let resp = session_clear_goal(
            State(state),
            Extension(AuthIdentity::local()),
            Path(session_id),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK, "expected 200 after unlock");
    }

    /// Helper: build a minimal AppState with one session for handler unit tests.
    async fn test_app_state_with_session(
        session_id: &str,
    ) -> (
        Arc<AppState>,
        Arc<tokio::sync::Mutex<crate::runtime::AgentRuntime>>,
    ) {
        use crate::llm::MockProvider;
        use crate::tools::ToolRegistry;

        std::env::set_var("RECURSIVE_API_KEY", "test-key");
        std::env::set_var("RECURSIVE_MODEL", "test-model");
        let config = crate::config::Config::from_env().unwrap();
        let provider = Arc::new(MockProvider::new(vec![]));
        let runtime = AgentRuntimeBuilder::new()
            .llm(provider.clone())
            .tools(ToolRegistry::default())
            .build()
            .expect("runtime build");
        let runtime_arc = Arc::new(tokio::sync::Mutex::new(runtime));
        let session = SessionState {
            id: session_id.to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            title: Some("old".into()),
            owner: None,
            tenant: None,
            runtime: runtime_arc.clone(),
            plan_approval_gate: Arc::new(crate::tools::plan_mode::PlanApprovalGate::new()),
            interrupt_token: Arc::new(tokio::sync::Mutex::new(None)),
            non_system_message_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            last_active_ms: Arc::new(AtomicU64::new(0)),
            prompt_tokens: Arc::new(AtomicU64::new(0)),
            completion_tokens: Arc::new(AtomicU64::new(0)),
            event_seq: Arc::new(AtomicU64::new(0)),
        };
        let sessions: HashMap<String, SessionState> = [(session_id.to_string(), session)].into();
        let host = Arc::new(crate::session_host::SessionHost::new(
            Duration::from_secs(3600),
            crate::http::AdmissionGate::new(
                8,
                Duration::ZERO,
                Arc::new(std::sync::atomic::AtomicU64::new(0)),
                Arc::new(std::sync::atomic::AtomicU64::new(0)),
            ),
        ));
        host.sessions().write().await.extend(sessions);
        let state = Arc::new(AppState {
            tools: vec![],
            tool_registry: ToolRegistry::default(),
            config,
            provider,
            event_channels: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            metrics: Arc::new(crate::http::Metrics::default()),
            slash_commands: Arc::new(vec![]),
            host,
            rate_limiter: crate::http::RateLimiter::new(10, 1.0),
            skills: vec![],
            storage: std::sync::Arc::new(crate::storage::LocalStorageBackend::new(
                std::env::temp_dir()
                    .join(format!("recursive-handlers-test-{}", std::process::id())),
            )),
            agui_active_runs: Arc::new(std::sync::Mutex::new(HashMap::new())),
        });
        (state, runtime_arc)
    }

    // ── issue #96: sessions path fences concurrent runs ───────────────

    /// A second POST for a session whose run is already in flight must be
    /// refused with 409 + Retry-After immediately — it must not queue on the
    /// runtime lock or consume a global admission permit while waiting.
    #[tokio::test]
    async fn send_session_message_fences_concurrent_run_with_409() {
        use axum::response::IntoResponse;

        let sid = "test-run-fence";
        let (state, runtime_arc) = test_app_state_with_session(sid).await;

        // Simulate the in-flight turn: hold the per-session fence and the
        // runtime lock exactly as the first request does while its turn runs.
        let fence = state
            .host
            .try_begin_run(format!("session:{sid}"))
            .expect("first run acquires the fence");
        let lock = runtime_arc.lock().await;

        let resp = send_session_message(
            State(state.clone()),
            Extension(AuthIdentity::local()),
            Path(sid.to_string()),
            Json(SessionMessageRequest {
                content: "retry the same prompt".into(),
                notify: None,
            }),
        )
        .await
        .into_response();

        assert_eq!(
            resp.status(),
            StatusCode::CONFLICT,
            "duplicate POST must be fenced with 409"
        );
        assert_eq!(
            resp.headers()
                .get(axum::http::header::RETRY_AFTER)
                .expect("Retry-After header missing")
                .to_str()
                .unwrap(),
            "5"
        );
        assert_eq!(
            state.host.admission().runs_in_flight(),
            0,
            "a fenced duplicate must not consume a global run slot"
        );

        drop(lock);
        drop(fence);
    }

    /// The fence must be released when the handler returns, so the next turn
    /// for the same session is not blocked (here the turn itself fails — the
    /// mock has no scripted completions — which also covers the error path).
    #[tokio::test]
    async fn send_session_message_releases_fence_after_turn() {
        let sid = "test-run-fence-release";
        let (state, _) = test_app_state_with_session(sid).await;

        let _ = send_session_message(
            State(state.clone()),
            Extension(AuthIdentity::local()),
            Path(sid.to_string()),
            Json(SessionMessageRequest {
                content: "hello".into(),
                notify: None,
            }),
        )
        .await;

        assert!(
            state.host.try_begin_run(format!("session:{sid}")).is_some(),
            "the run fence must be free once the handler returns"
        );
    }

    #[test]
    fn prior_turn_index_counts_only_user_messages() {
        use crate::message::Message;
        // User count (2) differs from non-user count (3) so an inverted
        // predicate would be caught.
        let transcript = vec![
            Message::user("first"),
            Message::user("second"),
            Message::assistant("one"),
            Message::assistant("two"),
            Message::system("sys"),
        ];
        assert_eq!(prior_turn_index(&transcript), 2);
        assert_eq!(prior_turn_index(&[]), 0);
    }

    #[tokio::test]
    async fn get_session_status_idle_vs_plan_pending() {
        let sid = "test-plan-status";
        let (state, _) = test_app_state_with_session(sid).await;

        let idle = match get_session(
            State(state.clone()),
            Extension(AuthIdentity::local()),
            Path(sid.to_string()),
        )
        .await
        {
            Ok(Json(v)) => v,
            Err(_) => panic!("get_session idle"),
        };
        assert_eq!(idle.status, "idle");
        assert!(idle.pending_plan.is_none());

        // Set pending plan via the gate.
        {
            let sessions_lock = state.host.sessions();
            let sessions = sessions_lock.read().await;
            let session = sessions.get(sid).unwrap();
            *session.plan_approval_gate.pending_plan.write().unwrap() = Some("do the thing".into());
        }
        let pending = match get_session(
            State(state),
            Extension(AuthIdentity::local()),
            Path(sid.to_string()),
        )
        .await
        {
            Ok(Json(v)) => v,
            Err(_) => panic!("get_session pending"),
        };
        assert_eq!(pending.status, "plan_pending_approval");
        assert_eq!(pending.pending_plan.as_deref(), Some("do the thing"));
    }

    #[tokio::test]
    async fn get_session_busy_runtime_returns_empty_messages() {
        let sid = "test-busy-get";
        let (state, runtime_arc) = test_app_state_with_session(sid).await;
        let _guard = runtime_arc.lock().await;
        let detail = match get_session(
            State(state),
            Extension(AuthIdentity::local()),
            Path(sid.to_string()),
        )
        .await
        {
            Ok(Json(v)) => v,
            Err(_) => panic!("busy get_session must still 200"),
        };
        assert!(
            detail.messages.is_empty(),
            "busy runtime must fall back to empty messages"
        );
        assert_eq!(detail.status, "idle");
    }

    #[tokio::test]
    async fn patch_session_empty_title_clears() {
        let sid = "test-patch-title";
        let (state, _) = test_app_state_with_session(sid).await;

        let cleared = match patch_session(
            State(state.clone()),
            Extension(AuthIdentity::local()),
            Path(sid.to_string()),
            Json(PatchSessionRequest {
                title: Some("".into()),
            }),
        )
        .await
        {
            Ok(Json(v)) => v,
            Err(_) => panic!("patch empty title"),
        };
        assert!(cleared.title.is_none(), "empty title must clear to None");

        let set = match patch_session(
            State(state.clone()),
            Extension(AuthIdentity::local()),
            Path(sid.to_string()),
            Json(PatchSessionRequest {
                title: Some("new".into()),
            }),
        )
        .await
        {
            Ok(Json(v)) => v,
            Err(_) => panic!("patch new title"),
        };
        assert_eq!(set.title.as_deref(), Some("new"));

        // Omitting title must leave existing value unchanged.
        let keep = match patch_session(
            State(state),
            Extension(AuthIdentity::local()),
            Path(sid.to_string()),
            Json(PatchSessionRequest { title: None }),
        )
        .await
        {
            Ok(Json(v)) => v,
            Err(_) => panic!("patch omit title"),
        };
        assert_eq!(keep.title.as_deref(), Some("new"));
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
            event_channels: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            slash_commands: Arc::new(vec![]),
            host: Arc::new(crate::session_host::SessionHost::new(
                Duration::from_secs(3600),
                crate::http::AdmissionGate::new(
                    8,
                    Duration::ZERO,
                    Arc::new(std::sync::atomic::AtomicU64::new(0)),
                    Arc::new(std::sync::atomic::AtomicU64::new(0)),
                ),
            )),
            rate_limiter: crate::http::RateLimiter::new(10, 1.0),
            skills: vec![],
            storage: std::sync::Arc::new(crate::storage::LocalStorageBackend::new(
                std::env::temp_dir()
                    .join(format!("recursive-handlers-test-{}", std::process::id())),
            )),
            agui_active_runs: Arc::new(std::sync::Mutex::new(HashMap::new())),
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
        // Goal 398: queue gauge is always exposed (0 when nothing waits).
        assert!(
            output.contains("recursive_runs_waiting 0"),
            "output should contain runs_waiting gauge: {output}"
        );
    }

    /// Goal-392: the two new gauges are always exposed.
    #[tokio::test]
    async fn metrics_handler_includes_gauges() {
        use crate::http::Metrics;
        use crate::tools::ToolRegistry;
        use std::sync::atomic::AtomicU64;
        std::env::set_var("RECURSIVE_API_KEY", "test-key");
        std::env::set_var("RECURSIVE_MODEL", "test-model");
        let config = crate::config::Config::from_env().unwrap();
        let metrics = Metrics {
            runs_in_flight: Arc::new(AtomicU64::new(7)),
            ..Metrics::default()
        };
        let state = Arc::new(AppState {
            metrics: Arc::new(metrics),
            tools: vec![],
            tool_registry: ToolRegistry::default(),
            config,
            provider: Arc::new(crate::llm::MockProvider::new(vec![])),
            event_channels: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            slash_commands: Arc::new(vec![]),
            host: Arc::new(crate::session_host::SessionHost::new(
                Duration::from_secs(3600),
                crate::http::AdmissionGate::new(
                    8,
                    Duration::ZERO,
                    Arc::new(AtomicU64::new(0)),
                    Arc::new(AtomicU64::new(0)),
                ),
            )),
            rate_limiter: crate::http::RateLimiter::new(10, 1.0),
            skills: vec![],
            storage: std::sync::Arc::new(crate::storage::LocalStorageBackend::new(
                std::env::temp_dir()
                    .join(format!("recursive-handlers-test-{}", std::process::id())),
            )),
            agui_active_runs: Arc::new(std::sync::Mutex::new(HashMap::new())),
        });
        let output = metrics_handler(State(state)).await;
        assert!(
            output.contains("recursive_runs_in_flight 7"),
            "output should contain runs_in_flight: {output}"
        );
        assert!(
            output.contains("recursive_transcript_bytes_total 0"),
            "output should contain transcript_bytes_total (empty host): {output}"
        );
        assert!(
            output.contains("recursive_transcript_bytes_skipped 0"),
            "output should contain transcript_bytes_skipped (empty host): {output}"
        );
    }

    /// Issue #123: minimal `AppState` for the readiness / metric-surface
    /// tests, mirroring `metrics_handler_includes_new_fields`.
    fn readyz_state(
        metrics: crate::http::Metrics,
        storage: Arc<dyn crate::storage::StorageBackend>,
        max_concurrent: usize,
    ) -> Arc<AppState> {
        use crate::tools::ToolRegistry;
        use std::sync::atomic::AtomicU64;
        std::env::set_var("RECURSIVE_API_KEY", "test-key");
        std::env::set_var("RECURSIVE_MODEL", "test-model");
        let config = crate::config::Config::from_env().unwrap();
        Arc::new(AppState {
            metrics: Arc::new(metrics),
            tools: vec![],
            tool_registry: ToolRegistry::default(),
            config,
            provider: Arc::new(crate::llm::MockProvider::new(vec![])),
            event_channels: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            slash_commands: Arc::new(vec![]),
            host: Arc::new(crate::session_host::SessionHost::new(
                Duration::from_secs(3600),
                crate::http::AdmissionGate::new(
                    max_concurrent,
                    Duration::ZERO,
                    Arc::new(AtomicU64::new(0)),
                    Arc::new(AtomicU64::new(0)),
                ),
            )),
            rate_limiter: crate::http::RateLimiter::new(10, 1.0),
            skills: vec![],
            storage,
            agui_active_runs: Arc::new(std::sync::Mutex::new(HashMap::new())),
        })
    }

    fn readyz_local_storage(tag: &str) -> Arc<dyn crate::storage::StorageBackend> {
        Arc::new(crate::storage::LocalStorageBackend::new(
            std::env::temp_dir().join(format!("recursive-readyz-{tag}-{}", std::process::id())),
        ))
    }

    /// Metrics carrying a fixed admission gauge pair.
    fn admission_metrics(in_flight: u64, waiting: u64) -> crate::http::Metrics {
        use std::sync::atomic::AtomicU64;
        crate::http::Metrics {
            runs_in_flight: Arc::new(AtomicU64::new(in_flight)),
            runs_waiting: Arc::new(AtomicU64::new(waiting)),
            ..crate::http::Metrics::default()
        }
    }

    /// Issue #123: a healthy server is ready; the body reports each check.
    #[tokio::test]
    async fn readyz_reports_ready_when_checks_pass() {
        let state = readyz_state(
            crate::http::Metrics::default(),
            readyz_local_storage("ok"),
            8,
        );
        let (status, Json(body)) = readyz(State(state)).await;
        assert_eq!(status, StatusCode::OK, "healthy server must be ready");
        assert_eq!(body["ready"], true);
        assert_eq!(body["checks"]["storage"]["ok"], true);
        assert_eq!(body["checks"]["llm"]["ok"], true);
        assert_eq!(body["checks"]["llm"]["consecutive_failures"], 0);
        assert_eq!(body["checks"]["admission"]["max_concurrent"], 8);
        assert_eq!(body["checks"]["admission"]["saturated"], false);
    }

    /// Issue #123: a broken key / unreachable gateway (a run-failure streak
    /// at the threshold) makes `/readyz` report 503 instead of 200.
    #[tokio::test]
    async fn readyz_reports_503_when_llm_streak_reaches_threshold() {
        let metrics = crate::http::Metrics::default();
        metrics
            .llm_failures_consecutive
            .store(READYZ_MAX_LLM_FAILURES, Ordering::Relaxed);
        // The failures must be *recent* to count (see
        // READYZ_LLM_FAILURE_WINDOW_MS), so stamp them.
        metrics
            .last_llm_failure_ms
            .store(now_stamp_ms(), Ordering::Relaxed);
        let state = readyz_state(metrics, readyz_local_storage("llm-down"), 8);
        let (status, Json(body)) = readyz(State(state)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["ready"], false);
        assert_eq!(body["checks"]["llm"]["ok"], false);
        assert_eq!(
            body["checks"]["llm"]["consecutive_failures"],
            READYZ_MAX_LLM_FAILURES
        );
        // Storage stays healthy — only the LLM check fails.
        assert_eq!(body["checks"]["storage"]["ok"], true);
    }

    /// Issue #123: one failure below the threshold still counts as ready —
    /// a single transient error must not flap the pod out of rotation.
    #[tokio::test]
    async fn readyz_stays_ready_below_llm_failure_threshold() {
        let metrics = crate::http::Metrics::default();
        metrics
            .llm_failures_consecutive
            .store(READYZ_MAX_LLM_FAILURES - 1, Ordering::Relaxed);
        metrics
            .last_llm_failure_ms
            .store(now_stamp_ms(), Ordering::Relaxed);
        let state = readyz_state(metrics, readyz_local_storage("llm-flaky"), 8);
        let (status, Json(body)) = readyz(State(state)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["checks"]["llm"]["ok"], true);
    }

    /// Issue #123: a failure streak nobody has refreshed must not latch the
    /// pod out of rotation forever. Once the probe fails, a k8s Service drops
    /// the pod from its endpoints, so it receives no runs — and therefore no
    /// success that could clear the streak — and nothing short of a restart
    /// would ever make it ready again.
    #[tokio::test]
    async fn readyz_decays_a_stale_llm_streak_instead_of_latching() {
        let metrics = crate::http::Metrics::default();
        metrics
            .llm_failures_consecutive
            .store(READYZ_MAX_LLM_FAILURES, Ordering::Relaxed);
        // A failure instant at least one window old: in a young process the
        // subtraction saturates to the "no instant recorded" sentinel, which
        // is stale too — either way the streak is not evidence of an outage
        // *now*.
        metrics.last_llm_failure_ms.store(
            crate::http::now_session_ms().saturating_sub(READYZ_LLM_FAILURE_WINDOW_MS),
            Ordering::Relaxed,
        );
        let state = readyz_state(metrics, readyz_local_storage("llm-stale"), 8);

        let (status, Json(body)) = readyz(State(state.clone())).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "a streak older than the window must not keep the pod unready"
        );
        assert_eq!(body["checks"]["llm"]["ok"], true);
        assert_eq!(body["checks"]["llm"]["consecutive_failures"], 0);
        assert_eq!(
            state
                .metrics
                .llm_failures_consecutive
                .load(Ordering::Relaxed),
            0,
            "the decay must reset the counter, not only the response body"
        );
    }

    /// Issue #123: the streak decision itself, at and around the window.
    #[test]
    fn llm_streak_is_current_only_while_the_last_failure_is_recent() {
        let window = READYZ_LLM_FAILURE_WINDOW_MS;
        assert!(
            !llm_streak_is_current(0, 1_000, 1_000),
            "no failures is not a streak"
        );
        assert!(
            llm_streak_is_current(3, 1_000, 1_000),
            "fresh failures are a live streak"
        );
        assert!(
            llm_streak_is_current(3, 1_000, 1_000 + window - 1),
            "still inside the window"
        );
        assert!(
            !llm_streak_is_current(3, 1_000, 1_000 + window),
            "at the window boundary the streak stops counting"
        );
        assert!(
            !llm_streak_is_current(3, 0, 1_000),
            "a streak with no recorded failure instant proves nothing"
        );
    }

    /// Issue #123: a saturated pool *with queued requests* is capacity
    /// exhaustion → 503; but fully-busy-but-draining is normal load, and an
    /// unlimited pool (`max_concurrent == 0`) can never be saturated.
    #[tokio::test]
    async fn readyz_reports_503_only_when_saturated_with_waiters() {
        // 8/8 in flight with 5 waiting → not ready.
        let state = readyz_state(
            admission_metrics(8, 5),
            readyz_local_storage("sat-queue"),
            8,
        );
        let (status, Json(body)) = readyz(State(state)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["checks"]["admission"]["ok"], false);
        assert_eq!(body["checks"]["admission"]["saturated"], true);

        // 8/8 in flight, nobody waiting → still ready.
        let state = readyz_state(
            admission_metrics(8, 0),
            readyz_local_storage("sat-drain"),
            8,
        );
        let (status, Json(body)) = readyz(State(state)).await;
        assert_eq!(status, StatusCode::OK, "draining load is not unreadiness");
        assert_eq!(body["checks"]["admission"]["saturated"], true);
        assert_eq!(body["checks"]["admission"]["ok"], true);

        // Unlimited pool (0) with a huge in-flight count → never saturated.
        let state = readyz_state(
            admission_metrics(100, 50),
            readyz_local_storage("sat-unbounded"),
            0,
        );
        let (status, Json(body)) = readyz(State(state)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["checks"]["admission"]["max_concurrent"], 0);
        assert_eq!(body["checks"]["admission"]["saturated"], false);
    }

    /// Issue #123: a read-only / full storage backend makes `/readyz` 503.
    #[tokio::test]
    async fn readyz_reports_503_when_storage_write_fails() {
        struct FailingStorage;
        #[async_trait::async_trait]
        impl crate::storage::StorageBackend for FailingStorage {
            async fn load_transcript(
                &self,
                _session_id: &str,
            ) -> crate::error::Result<Vec<crate::message::Message>> {
                Ok(vec![])
            }
            async fn save_transcript(
                &self,
                _session_id: &str,
                _messages: &[crate::message::Message],
            ) -> crate::error::Result<()> {
                Ok(())
            }
            async fn load_memory(&self, _key: &str) -> crate::error::Result<Option<String>> {
                Ok(None)
            }
            async fn save_memory(&self, _key: &str, _value: &str) -> crate::error::Result<()> {
                Err(crate::error::Error::Storage {
                    message: "disk full".into(),
                })
            }
        }
        let state = readyz_state(crate::http::Metrics::default(), Arc::new(FailingStorage), 8);
        let (status, Json(body)) = readyz(State(state)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["ready"], false);
        assert_eq!(body["checks"]["storage"]["ok"], false);
    }

    /// Issue #123: a backend that accepts the probe write and then loses it
    /// fails the read-back — a write-only check would call this healthy.
    #[tokio::test]
    async fn readyz_reports_503_when_the_storage_round_trip_is_lost() {
        struct DroppingStorage;
        #[async_trait::async_trait]
        impl crate::storage::StorageBackend for DroppingStorage {
            async fn load_transcript(
                &self,
                _session_id: &str,
            ) -> crate::error::Result<Vec<crate::message::Message>> {
                Ok(vec![])
            }
            async fn save_transcript(
                &self,
                _session_id: &str,
                _messages: &[crate::message::Message],
            ) -> crate::error::Result<()> {
                Ok(())
            }
            async fn load_memory(&self, _key: &str) -> crate::error::Result<Option<String>> {
                Ok(None)
            }
            async fn save_memory(&self, _key: &str, _value: &str) -> crate::error::Result<()> {
                Ok(())
            }
        }
        let state = readyz_state(
            crate::http::Metrics::default(),
            Arc::new(DroppingStorage),
            8,
        );
        let (status, Json(body)) = readyz(State(state)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["checks"]["storage"]["ok"], false);
    }

    /// Issue #123: the storage verdict is cached, so scraping this public
    /// route cannot drive one write per request; a stale verdict is re-probed.
    #[tokio::test]
    async fn readyz_caches_the_storage_verdict_within_the_ttl() {
        #[derive(Default)]
        struct ProbeCountingStorage {
            writes: std::sync::atomic::AtomicUsize,
            broken: std::sync::atomic::AtomicBool,
            stored: std::sync::Mutex<Option<String>>,
        }
        #[async_trait::async_trait]
        impl crate::storage::StorageBackend for ProbeCountingStorage {
            async fn load_transcript(
                &self,
                _session_id: &str,
            ) -> crate::error::Result<Vec<crate::message::Message>> {
                Ok(vec![])
            }
            async fn save_transcript(
                &self,
                _session_id: &str,
                _messages: &[crate::message::Message],
            ) -> crate::error::Result<()> {
                Ok(())
            }
            async fn load_memory(&self, _key: &str) -> crate::error::Result<Option<String>> {
                Ok(self
                    .stored
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone())
            }
            async fn save_memory(&self, _key: &str, value: &str) -> crate::error::Result<()> {
                self.writes.fetch_add(1, Ordering::Relaxed);
                if self.broken.load(Ordering::Relaxed) {
                    return Err(crate::error::Error::Storage {
                        message: "read-only filesystem".into(),
                    });
                }
                *self.stored.lock().unwrap_or_else(|e| e.into_inner()) = Some(value.to_string());
                Ok(())
            }
        }

        let storage = Arc::new(ProbeCountingStorage::default());
        let backend: Arc<dyn crate::storage::StorageBackend> = storage.clone();
        let state = readyz_state(crate::http::Metrics::default(), backend, 8);

        let (status, _) = readyz(State(state.clone())).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(storage.writes.load(Ordering::Relaxed), 1);

        // A second probe inside the TTL reuses the verdict instead of writing.
        storage.broken.store(true, Ordering::Relaxed);
        let (status, Json(body)) = readyz(State(state.clone())).await;
        assert_eq!(status, StatusCode::OK, "a fresh verdict is reused");
        assert_eq!(body["checks"]["storage"]["ok"], true);
        assert_eq!(
            storage.writes.load(Ordering::Relaxed),
            1,
            "a cached verdict must not write again"
        );

        // With the cache marked stale (`0` = never probed) the probe runs
        // again — and now sees the broken backend.
        state
            .metrics
            .readyz_storage_probed_ms
            .store(0, Ordering::Relaxed);
        let (status, Json(body)) = readyz(State(state)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["checks"]["storage"]["ok"], false);
        assert_eq!(
            storage.writes.load(Ordering::Relaxed),
            2,
            "a stale verdict must be re-probed"
        );
    }

    /// Issue #123: the probe-freshness decision, at and around the TTL.
    #[test]
    fn readyz_probe_is_fresh_only_inside_the_ttl() {
        let ttl = READYZ_PROBE_TTL_MS;
        assert!(!readyz_probe_is_fresh(0, 10), "never probed");
        assert!(readyz_probe_is_fresh(1_000, 1_000), "just probed");
        assert!(readyz_probe_is_fresh(1_000, 1_000 + ttl - 1));
        assert!(
            !readyz_probe_is_fresh(1_000, 1_000 + ttl),
            "at the TTL boundary the verdict is stale"
        );
    }

    /// Issue #123: the new capacity / data-loss series reach `/metrics`, and
    /// the SSE gauge counts live subscribers on `event_channels`.
    #[tokio::test]
    async fn metrics_handler_exposes_capacity_and_loss_series() {
        let metrics = crate::http::Metrics {
            persist_failures: AtomicU64::new(4),
            sessions_evicted: AtomicU64::new(2),
            last_llm_success_ms: AtomicU64::new(1234),
            llm_failures_consecutive: AtomicU64::new(1),
            ..crate::http::Metrics::default()
        };
        let state = readyz_state(metrics, readyz_local_storage("series"), 8);
        let (tx, _rx) = tokio::sync::broadcast::channel(4);
        state.event_channels.write().await.insert("s".into(), tx);
        state
            .agui_active_runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert("t".into(), tokio_util::sync::CancellationToken::new());

        let output = metrics_handler(State(state)).await;
        for expected in [
            "recursive_persist_failures 4",
            "recursive_sessions_evicted 2",
            "recursive_llm_last_success_ms 1234",
            "recursive_llm_failures_consecutive 1",
            "recursive_sse_clients 1",
            "recursive_agui_runs 1",
        ] {
            assert!(output.contains(expected), "missing {expected}: {output}");
        }
    }

    /// Issue #123: a run that completed resets the failure streak and stamps
    /// the success instant; an LLM *call* failure bumps the streak and stamps
    /// its own instant — while a plain run failure (a cancellation, a tool or
    /// storage error) only moves the run counters.
    ///
    /// Both stamps are asserted `> 0`, which is only meaningful because the
    /// stamps are never 0: the session epoch is initialised by the first
    /// caller in the process, so this test would otherwise fail whenever it
    /// runs first (0 doubles as the "never happened" sentinel).
    #[test]
    fn record_run_metrics_track_llm_streak() {
        let metrics = crate::http::Metrics::default();
        let none = crate::llm::TokenUsage::default();
        record_run_failed(&metrics, &none);
        // Issue #115: a failed run that burned tokens must move the wasted
        // counter — the failure path used to record zero usage.
        let wasted = crate::llm::TokenUsage {
            prompt_tokens: 30,
            completion_tokens: 12,
            total_tokens: 42,
            ..Default::default()
        };
        record_run_failed(&metrics, &wasted);
        assert_eq!(
            metrics.agent_runs_failed.load(Ordering::Relaxed),
            2,
            "run failures are still counted"
        );
        assert_eq!(
            metrics
                .tokens_wasted_on_failure_total
                .load(Ordering::Relaxed),
            42,
            "wasted tokens (prompt + completion) from failed runs must be counted"
        );
        assert_eq!(
            metrics.llm_failures_consecutive.load(Ordering::Relaxed),
            0,
            "a cancellation / tool / storage failure says nothing about the LLM"
        );
        assert_eq!(
            metrics.last_llm_failure_ms.load(Ordering::Relaxed),
            0,
            "no LLM failure yet must read as 0"
        );

        record_llm_failure(&metrics);
        assert_eq!(metrics.llm_failures_consecutive.load(Ordering::Relaxed), 1);
        assert!(
            metrics.last_llm_failure_ms.load(Ordering::Relaxed) > 0,
            "an LLM failure stamps its instant"
        );

        record_run_success(&metrics, 1, &crate::llm::TokenUsage::default());
        record_llm_success(&metrics);
        assert_eq!(
            metrics.llm_failures_consecutive.load(Ordering::Relaxed),
            0,
            "a success clears the streak"
        );
        assert!(
            metrics.last_llm_success_ms.load(Ordering::Relaxed) > 0,
            "a success stamps the timestamp"
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
            event_channels: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            metrics: metrics.clone(),
            slash_commands: Arc::new(vec![]),
            host: Arc::new(crate::session_host::SessionHost::new(
                Duration::from_secs(3600),
                crate::http::AdmissionGate::new(
                    8,
                    Duration::ZERO,
                    Arc::new(std::sync::atomic::AtomicU64::new(0)),
                    Arc::new(std::sync::atomic::AtomicU64::new(0)),
                ),
            )),
            rate_limiter: crate::http::RateLimiter::new(100, 1.0),
            skills: vec![],
            storage: std::sync::Arc::new(crate::storage::LocalStorageBackend::new(
                std::env::temp_dir()
                    .join(format!("recursive-handlers-test-{}", std::process::id())),
            )),
            agui_active_runs: Arc::new(std::sync::Mutex::new(HashMap::new())),
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
        let spec = super::super::build_openapi_spec();
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
        let spec = super::super::build_openapi_spec();
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
        let spec = super::super::build_openapi_spec();
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
        // Issue #123: the capacity/data-loss series must be documented.
        for series in [
            "recursive_sse_clients",
            "recursive_agui_runs",
            "recursive_persist_failures",
            "recursive_sessions_evicted",
            "recursive_llm_last_success_ms",
            "recursive_llm_failures_consecutive",
        ] {
            assert!(
                description.contains(series),
                "metrics description should mention {series}: {description}"
            );
        }
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

    /// Verify the skill catalog is produced as a `system-reminder` and is
    /// NOT inlined into the static system prompt (it ships per-request via
    /// `skill_reminder`, so a skill change never rewrites the transcript).
    #[test]
    fn skill_reminder_produced_and_not_inlined() {
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
            body: None,
        }];

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

        // The catalog ships as a system-reminder block.
        let reminder = crate::skills::skill_reminder(&skills);
        assert!(reminder.contains("<system-reminder>"), "reminder wrapper");
        assert!(
            reminder.contains("Available skills"),
            "reminder should contain header"
        );
        assert!(
            reminder.contains("rust-patch-discipline"),
            "reminder should contain skill name"
        );

        // The static system prompt must NOT inline it.
        let base_prompt = "You are a helpful agent.";
        let assembled =
            crate::assemble_system_prompt(base_prompt, std::path::Path::new(""), &skills, false)
                .into_full();
        assert!(
            !assembled.contains("Available skills"),
            "system prompt must not inline skill catalog: {assembled}"
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

    // ── parse_permission_mode ───────────────────────────────────────────────

    #[test]
    fn parse_permission_mode_all_variants() {
        assert_eq!(parse_permission_mode("auto", false), PermissionMode::Auto);
        assert_eq!(parse_permission_mode("AUTO", true), PermissionMode::Auto);
        assert_eq!(
            parse_permission_mode("strict", false),
            PermissionMode::Strict
        );
        assert_eq!(
            parse_permission_mode("bypass", true),
            PermissionMode::BypassPermissions
        );
        assert_eq!(
            parse_permission_mode("bypass_permissions", true),
            PermissionMode::BypassPermissions
        );
        // Bypass rejected when allow_bypass=false → Default (kills match-guard mutant).
        assert_eq!(
            parse_permission_mode("bypass", false),
            PermissionMode::Default
        );
        assert_eq!(
            parse_permission_mode("bypass_permissions", false),
            PermissionMode::Default
        );
        assert_eq!(
            parse_permission_mode("default", true),
            PermissionMode::Default
        );
        assert_eq!(
            parse_permission_mode("unknown", true),
            PermissionMode::Default
        );
    }

    /// Issue #98: `GET /sessions/:id` reports the mode in the same vocabulary
    /// `parse_permission_mode` accepts, so a client can round-trip it.
    #[test]
    fn permission_mode_label_covers_every_variant() {
        assert_eq!(permission_mode_label(&PermissionMode::Default), "default");
        assert_eq!(permission_mode_label(&PermissionMode::Auto), "auto");
        assert_eq!(permission_mode_label(&PermissionMode::Strict), "strict");
        assert_eq!(
            permission_mode_label(&PermissionMode::BypassPermissions),
            "bypass"
        );
        assert_eq!(
            permission_mode_label(&PermissionMode::AcceptEdits),
            "acceptEdits"
        );
        assert_eq!(permission_mode_label(&PermissionMode::DontAsk), "dontAsk");
        assert_eq!(
            permission_mode_label(&PermissionMode::Plan {
                pre_plan_mode: Box::new(PermissionMode::Default),
                bypass_available: false,
            }),
            "plan"
        );
    }

    // ── map_agent_event / sse_message_from_canonical ────────────────────────

    #[test]
    fn map_agent_event_tool_result_success_flag() {
        let ok = AgentEvent::ToolResult {
            id: "tc-1".into(),
            name: "Bash".into(),
            output: "ok".into(),
            step: 0,
            is_error: false,
            duration_ms: 0,
        };
        let err = AgentEvent::ToolResult {
            id: "tc-2".into(),
            name: "Bash".into(),
            output: "fail".into(),
            step: 0,
            is_error: true,
            duration_ms: 0,
        };
        match map_agent_event(&ok) {
            Some(SseEvent::ToolResult { success, .. }) => assert!(success),
            other => panic!("expected ToolResult success=true, got {other:?}"),
        }
        match map_agent_event(&err) {
            Some(SseEvent::ToolResult { success, .. }) => assert!(!success),
            other => panic!("expected ToolResult success=false, got {other:?}"),
        }
    }

    #[test]
    fn map_agent_event_suppresses_non_sse_variants() {
        // Latency / Usage / AssistantText have no SSE equivalent.
        assert!(map_agent_event(&AgentEvent::Latency { step: 0, llm_ms: 1 }).is_none());
        assert!(map_agent_event(&AgentEvent::Usage {
            input_tokens: 1,
            output_tokens: 1,
            cache_hit_tokens: 0,
            cache_miss_tokens: 0,
            step: 0,
        })
        .is_none());
        assert!(map_agent_event(&AgentEvent::AssistantText {
            text: "hi".into(),
            step: 0
        })
        .is_none());
    }

    #[test]
    fn map_agent_event_forwards_goal_loop_events() {
        // kills delete-match-arm on GoalContinuing / GoalAchieved
        match map_agent_event(&AgentEvent::GoalContinuing {
            reason: "still working".into(),
            turns: 3,
        }) {
            Some(SseEvent::GoalContinuing { reason, turns }) => {
                assert_eq!(reason, "still working");
                assert_eq!(turns, 3);
            }
            other => panic!("expected GoalContinuing, got {other:?}"),
        }
        match map_agent_event(&AgentEvent::GoalAchieved {
            condition: "tests pass".into(),
            turns: 5,
        }) {
            Some(SseEvent::GoalAchieved { condition, turns }) => {
                assert_eq!(condition, "tests pass");
                assert_eq!(turns, 5);
            }
            other => panic!("expected GoalAchieved, got {other:?}"),
        }
    }

    #[test]
    fn map_agent_event_forwards_core_sse_arms() {
        // kills delete-match-arm on PartialToken / ToolCall / TurnFinished / PlanProposed
        match map_agent_event(&AgentEvent::PartialToken {
            text: "tok".into(),
            step: 2,
        }) {
            Some(SseEvent::PartialMessage { text, step }) => {
                assert_eq!(text, "tok");
                assert_eq!(step, 2);
            }
            other => panic!("expected PartialMessage, got {other:?}"),
        }
        match map_agent_event(&AgentEvent::ToolCall {
            name: "Bash".into(),
            id: "tc-1".into(),
            arguments: "{}".into(),
            step: 1,
        }) {
            Some(SseEvent::ToolCall { name, step }) => {
                assert_eq!(name, "Bash");
                assert_eq!(step, 1);
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
        match map_agent_event(&AgentEvent::TurnFinished {
            reason: "no_more_tool_calls".into(),
            steps: 4,
        }) {
            Some(SseEvent::Done {
                finish_reason,
                total_steps,
            }) => {
                assert_eq!(finish_reason, "no_more_tool_calls");
                assert_eq!(total_steps, 4);
            }
            other => panic!("expected Done, got {other:?}"),
        }
        match map_agent_event(&AgentEvent::PlanProposed {
            plan_text: "do X".into(),
            tool_calls: vec![],
        }) {
            Some(SseEvent::PlanProposed { plan }) => assert_eq!(plan, "do X"),
            other => panic!("expected PlanProposed, got {other:?}"),
        }
    }

    #[test]
    fn sse_message_from_canonical_filters_system_tool_and_empty() {
        assert!(
            sse_message_from_canonical(&crate::message::Message::system("seed")).is_none(),
            "system messages must be filtered"
        );
        let mut tool = crate::message::Message::user("unused");
        tool.role = crate::message::Role::Tool;
        tool.tool_call_id = Some("tc-1".into());
        assert!(
            sse_message_from_canonical(&tool).is_none(),
            "tool messages must be filtered"
        );
        assert!(
            sse_message_from_canonical(&crate::message::Message::assistant("")).is_none(),
            "empty assistant with no tool_calls must be None"
        );
        match sse_message_from_canonical(&crate::message::Message::assistant("hello")) {
            Some(SseEvent::Message { role, content }) => {
                assert_eq!(role, "assistant");
                assert!(!content.is_empty());
            }
            other => panic!("expected Message event, got {other:?}"),
        }
        match sse_message_from_canonical(&crate::message::Message::user("hi")) {
            Some(SseEvent::Message { role, .. }) => assert_eq!(role, "user"),
            other => panic!("expected user Message, got {other:?}"),
        }
    }

    #[test]
    fn sse_message_from_canonical_emits_tool_use_without_text() {
        // kills early-empty return before tool_calls loop / ToolUse arm delete
        let msg = crate::message::Message::assistant_with_tool_calls(
            "",
            vec![crate::llm::ToolCall {
                id: "tc-9".into(),
                name: "Read".into(),
                arguments: serde_json::json!({"path":"a.rs"}),
            }],
        );
        match sse_message_from_canonical(&msg) {
            Some(SseEvent::Message { role, content }) => {
                assert_eq!(role, "assistant");
                assert!(
                    content.iter().any(|b| matches!(
                        b,
                        SseContentBlock::ToolUse { id, name, .. }
                            if id == "tc-9" && name == "Read"
                    )),
                    "expected ToolUse block, got {content:?}"
                );
            }
            other => panic!("expected Message with ToolUse, got {other:?}"),
        }
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

    // ── Issue #62: agui non-resume seeding (through HTTP) ─────────────

    /// End-to-end issue #62 acceptance: two turns on the same thread where
    /// the second turn carries the FULL `messages` array (no resume). The
    /// provider must see turn-one history in its request — multi-turn
    /// context works without any tool side effect.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // std env lock is fine: only same-crate tests contend
    async fn agui_non_resume_turn_seeds_full_messages_history() {
        use crate::llm::MockProvider;
        use crate::tools::ToolRegistry;
        use tower::ServiceExt;

        let _env = crate::test_util::env_lock();
        let ws = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        std::env::set_var("RECURSIVE_WORKSPACE", ws.path());
        std::env::set_var("RECURSIVE_HOME", home.path());
        let saved_sessions_dir = std::env::var_os("RECURSIVE_SESSIONS_DIR");
        std::env::remove_var("RECURSIVE_SESSIONS_DIR");
        std::env::set_var("RECURSIVE_API_KEY", "test-key");
        std::env::set_var("RECURSIVE_MODEL", "test-model");
        std::env::set_var("RECURSIVE_HTTP_AUTH_INSECURE_OK", "1");

        let config = crate::config::Config::from_env().unwrap();
        let provider = Arc::new(MockProvider::new(vec![
            crate::llm::Completion {
                content: "the codename is bluebird".into(),
                ..Default::default()
            },
            crate::llm::Completion {
                content: "i remember: bluebird".into(),
                ..Default::default()
            },
        ]));
        let metrics = Arc::new(crate::http::Metrics::default());
        let state = AppState {
            tools: vec![],
            tool_registry: ToolRegistry::default(),
            config,
            provider: provider.clone(),
            event_channels: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            metrics,
            slash_commands: Arc::new(vec![]),
            host: Arc::new(crate::session_host::SessionHost::new(
                Duration::from_secs(3600),
                crate::http::AdmissionGate::new(
                    8,
                    Duration::ZERO,
                    Arc::new(std::sync::atomic::AtomicU64::new(0)),
                    Arc::new(std::sync::atomic::AtomicU64::new(0)),
                ),
            )),
            rate_limiter: crate::http::RateLimiter::new(100, 1.0),
            skills: vec![],
            storage: std::sync::Arc::new(crate::storage::LocalStorageBackend::new(
                std::env::temp_dir()
                    .join(format!("recursive-agui-seed-test-{}", std::process::id())),
            )),
            agui_active_runs: Arc::new(std::sync::Mutex::new(HashMap::new())),
        };
        let app = crate::http::build_router_with_auth_and_rate_limit(
            state,
            crate::http::auth::AuthConfig::default(),
            crate::http::RateLimiter::new(100, 1.0),
        );

        let post = |body: serde_json::Value| {
            app.clone().oneshot(
                axum::extract::Request::builder()
                    .method("POST")
                    .uri("/agui")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
            )
        };
        let drain = |resp: axum::response::Response| async move {
            let _ = axum::body::to_bytes(resp.into_body(), 1 << 20).await;
        };

        // Turn 1: single user message.
        drain(
            post(serde_json::json!({
                "threadId": "bluebird-thread",
                "runId": "r1",
                "messages": [
                    {"id": "m1", "role": "user", "content": "the codename is bluebird, remember it"}
                ],
            }))
            .await
            .unwrap(),
        )
        .await;

        // Turn 2: full history, no resume — the standard AG-UI client shape.
        drain(
            post(serde_json::json!({
                "threadId": "bluebird-thread",
                "runId": "r2",
                "messages": [
                    {"id": "m1", "role": "user", "content": "the codename is bluebird, remember it"},
                    {"id": "m2", "role": "assistant", "content": "the codename is bluebird"},
                    {"id": "m3", "role": "user", "content": "what was the codename?"}
                ],
            }))
            .await
            .unwrap(),
        )
        .await;

        let calls = provider.calls();
        assert!(
            calls.len() >= 2,
            "expected ≥2 provider calls, got {}",
            calls.len()
        );
        let turn2 = &calls[1];
        let flat: Vec<(&crate::message::Role, &str)> = turn2
            .iter()
            .map(|m| (&m.role, m.content.as_str()))
            .collect();
        assert!(
            flat.iter().any(|(r, c)| {
                matches!(r, crate::message::Role::User)
                    && c.contains("codename is bluebird, remember it")
            }),
            "turn-2 provider request must contain turn-1 history; got {flat:?}"
        );
        // The goal message must appear exactly once (not duplicated by seeding).
        let goal_count = flat
            .iter()
            .filter(|(_, c)| *c == "what was the codename?")
            .count();
        assert_eq!(goal_count, 1, "goal must appear exactly once; got {flat:?}");
        // Invariant #8: no tool roles in the seeded request at all.
        assert!(
            !turn2
                .iter()
                .any(|m| matches!(m.role, crate::message::Role::Tool)),
            "no tool-role messages expected"
        );
        std::env::remove_var("RECURSIVE_WORKSPACE");
        std::env::remove_var("RECURSIVE_SESSIONS_DIR");
        if let Some(v) = saved_sessions_dir {
            std::env::set_var("RECURSIVE_SESSIONS_DIR", v);
        }
    }

    // ── Issue #66: token streaming + cancellation ────────────────────────

    /// POST /agui/{thread_id}/cancel cancels the registered token; an
    /// unknown (or already-finished) thread is an idempotent 200 with
    /// `"cancelled": false`.
    #[tokio::test]
    async fn agui_cancel_cancels_registered_thread_and_is_idempotent() {
        use crate::llm::MockProvider;
        use crate::tools::ToolRegistry;

        std::env::set_var("RECURSIVE_API_KEY", "test-key");
        std::env::set_var("RECURSIVE_MODEL", "test-model");
        let config = crate::config::Config::from_env().unwrap();
        let metrics = Arc::new(crate::http::Metrics::default());
        let state = Arc::new(crate::http::AppState {
            tools: vec![],
            tool_registry: ToolRegistry::default(),
            config,
            provider: Arc::new(MockProvider::new(vec![])),
            host: Arc::new(crate::session_host::SessionHost::new(
                Duration::from_secs(3600),
                crate::http::AdmissionGate::new(
                    8,
                    Duration::ZERO,
                    Arc::new(AtomicU64::new(0)),
                    Arc::new(AtomicU64::new(0)),
                ),
            )),
            event_channels: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            metrics,
            slash_commands: Arc::new(vec![]),
            rate_limiter: crate::http::RateLimiter::new(100, 1.0),
            skills: vec![],
            storage: Arc::new(crate::storage::LocalStorageBackend::new(
                std::env::temp_dir().join(format!("recursive-agui-cancel-{}", std::process::id())),
            )),
            agui_active_runs: Arc::new(std::sync::Mutex::new(HashMap::new())),
        });

        let token = tokio_util::sync::CancellationToken::new();
        state
            .agui_active_runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert("t-cancel".into(), token.clone());

        let Json(res) = agui_cancel(State(Arc::clone(&state)), Path("t-cancel".into())).await;
        assert_eq!(res["status"], "interrupted");
        assert_eq!(res["cancelled"], true);
        assert!(token.is_cancelled(), "registered token must be cancelled");

        let Json(res) = agui_cancel(State(state), Path("no-such-thread".into())).await;
        assert_eq!(res["cancelled"], false, "unknown thread stays idempotent");
    }

    /// Dropping the wrapped SSE body (client disconnect) must cancel the
    /// run token; polling through the wrapper still forwards events.
    #[tokio::test]
    async fn agui_sse_drop_cancels_run_token() {
        let token = tokio_util::sync::CancellationToken::new();
        // `Ready` (not an async block): the wrapper's Stream impl requires
        // `S: Unpin`, which `Once<Ready<_>>` satisfies the same way the
        // production stream (map over an unbounded receiver) does.
        let inner = futures_util::stream::once(futures_util::future::ready(Ok::<_, Infallible>(
            Event::default().data("x"),
        )));
        let mut guarded = CancelOnDrop {
            inner,
            token: Some(token.clone()),
        };
        // File scope imports tokio_stream::StreamExt; disambiguate.
        assert!(
            futures_util::StreamExt::next(&mut guarded).await.is_some(),
            "wrapper must forward items"
        );
        assert!(!token.is_cancelled());
        drop(guarded);
        assert!(
            token.is_cancelled(),
            "dropping the SSE body cancels the run"
        );
    }

    /// Issue #66 §3.2 end-to-end: a chunked provider must reach the wire as
    /// multiple `TextMessageContent` frames whose concatenation carries the
    /// answer EXACTLY once (the finalising `AssistantText` must not
    /// duplicate it), and the stream must end with `RunFinished`.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // std env lock is fine: only same-crate tests contend
    async fn agui_streams_token_deltas_without_duplicating_final_text() {
        use crate::llm::MockProvider;
        use crate::tools::ToolRegistry;
        use tower::ServiceExt;

        let _env = crate::test_util::env_lock();
        let ws = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        std::env::set_var("RECURSIVE_WORKSPACE", ws.path());
        std::env::set_var("RECURSIVE_HOME", home.path());
        std::env::set_var("RECURSIVE_API_KEY", "test-key");
        std::env::set_var("RECURSIVE_MODEL", "test-model");
        std::env::set_var("RECURSIVE_HTTP_AUTH_INSECURE_OK", "1");

        let config = crate::config::Config::from_env().unwrap();
        let provider = Arc::new(
            MockProvider::new(vec![crate::llm::Completion {
                content: "abcdefgh".into(),
                ..Default::default()
            }])
            .with_stream_chunk_chars(3),
        );
        let metrics = Arc::new(crate::http::Metrics::default());
        let state = AppState {
            tools: vec![],
            tool_registry: ToolRegistry::default(),
            config,
            provider,
            event_channels: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            metrics,
            slash_commands: Arc::new(vec![]),
            host: Arc::new(crate::session_host::SessionHost::new(
                Duration::from_secs(3600),
                crate::http::AdmissionGate::new(
                    8,
                    Duration::ZERO,
                    Arc::new(AtomicU64::new(0)),
                    Arc::new(AtomicU64::new(0)),
                ),
            )),
            rate_limiter: crate::http::RateLimiter::new(100, 1.0),
            skills: vec![],
            storage: Arc::new(crate::storage::LocalStorageBackend::new(
                std::env::temp_dir().join(format!("recursive-agui-stream-{}", std::process::id())),
            )),
            agui_active_runs: Arc::new(std::sync::Mutex::new(HashMap::new())),
        };
        let app = crate::http::build_router_with_auth_and_rate_limit(
            state,
            crate::http::auth::AuthConfig::default(),
            crate::http::RateLimiter::new(100, 1.0),
        );

        let resp = app
            .oneshot(
                axum::extract::Request::builder()
                    .method("POST")
                    .uri("/agui")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::json!({
                            "threadId": "stream-th",
                            "runId": "r-stream",
                            "messages": [
                                {"id": "m1", "role": "user", "content": "hi"}
                            ]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .expect("response");
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .expect("body");

        let mut deltas = String::new();
        let mut saw_run_finished = false;
        let mut content_frames = 0usize;
        for line in String::from_utf8_lossy(&bytes).lines() {
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
                continue;
            };
            match v["type"].as_str() {
                Some("TextMessageContent") => {
                    content_frames += 1;
                    deltas.push_str(v["delta"].as_str().unwrap_or_default());
                }
                Some("RunFinished") => saw_run_finished = true,
                _ => {}
            }
        }
        assert!(
            content_frames >= 2,
            "chunked provider must yield multiple content frames, got {content_frames}"
        );
        assert_eq!(
            deltas, "abcdefgh",
            "deltas must carry the answer exactly once — no duplicated final message"
        );
        assert!(saw_run_finished, "stream must end with RunFinished");
    }
}
