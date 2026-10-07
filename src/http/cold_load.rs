//! Goal 397 — HTTP session cold load: restore sessions from storage.
//!
//! Sessions are held in an in-memory table (`AppState.sessions`), so before
//! this module a server restart made every persisted session invisible to
//! the API (`GET /sessions/:id` → 404) even though Goal 396's write path had
//! flushed transcripts to the `StorageBackend`. This module adds the read
//! path: on a memory miss, `GET /sessions/:id` and
//! `POST /sessions/:id/messages` rebuild the session from
//! [`crate::storage::StorageBackend::load_transcript`] and insert it back
//! into the table (lazy load).
//!
//! # Transcript normalization (the two real traps)
//!
//! Storage holds kernel transcripts — messages exactly as the runtime saw
//! them, starting with the system prompt. They cannot be seeded verbatim:
//!
//! 1. **Double system prompt.** The runtime builds its own system prompt and
//!    [`AgentRuntimeBuilder::seed_transcript`] places the seed AFTER it.
//!    Seeding a stored system message would yield two. Rule: drop
//!    `msgs[0]` when its role is `System`.
//! 2. **Orphan tool results (invariant #8).** A transcript whose head is a
//!    run of `Role::Tool` messages has no preceding assistant `tool_calls`
//!    to answer to; providers reject it with HTTP 400. Rule: drop the
//!    leading run of `Role::Tool` messages.
//!
//! The empty check happens AFTER normalization: a stored transcript that
//! only contained a system message (or only orphan tool results) restores
//! to nothing and must stay 404 — never a ghost session.
//!
//! # Per-session metadata (issue #98)
//!
//! A restored runtime is built through the SAME frontend-neutral path as a
//! fresh one (`handlers::build_session_runtime`), so it gets the compactor /
//! microcompactor / transcript cap, token streaming and the storage backend.
//! Before #98 the restored path skipped all of that and additionally rebuilt
//! the system prompt from the server default — silently swapping a custom
//! persona and downgrading the permission mode.
//!
//! The per-session config echoes (`system_prompt`, `permission_mode`, `title`,
//! `max_steps`) are persisted as a JSON blob in the storage backends' generic
//! key/value space (`session-meta/<id>`, the same space as the delete
//! tombstone) at session creation. A transcript persisted by an older build
//! has no blob: it restores with the server defaults — the pre-#98 behaviour —
//! rather than failing. `title` is re-persisted on `PATCH /sessions/:id`.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use super::handlers::{ensure_access, format_timestamp};
use super::{ApiError, AppState, AuthIdentity, SessionState};
use crate::message::{Message, Role};
use crate::runtime::AgentRuntime;
use std::time::SystemTime;

/// Fetch a session from the in-memory table, cold-loading it from the
/// storage backend on a miss.
///
/// Lock discipline (the IO never runs under the table lock):
///
/// 1. short read lock — hit returns immediately;
/// 2. `load_transcript().await` with NO lock held;
/// 3. short write lock — a concurrent loader that won the race wins
///    ("first insert wins"), the loser discards its freshly built runtime.
///
/// Returns 404 when the storage has no restorable transcript for `id`
/// (missing, or empty after normalization) — identical to the pre-restart
/// semantics for unknown ids.
///
/// Issue #85: ownership is asserted here — before the runtime is built and
/// inserted — so a caller probing a foreign id cannot make the server
/// materialize someone else's session. Returns 403 for a session the identity
/// does not own (an unattributed one included: only an admin reaches those).
/// Storage key for the "this session was deleted" tombstone.
///
/// Goal 396 keeps the transcript snapshot when a session ends (DELETE included —
/// the CLI session-directory semantics), while Goal 397 restores any session
/// with a non-empty snapshot. Without a marker those two combine into "DELETE
/// then GET resurrects the session", which breaks the v050 lifecycle contract
/// (DELETE → GET must be 404). The tombstone lives in the storage backends'
/// generic key/value space so it works for every backend (local / S3).
pub(super) fn deleted_marker_key(id: &str) -> String {
    format!("session-deleted/{id}")
}

/// Per-session configuration that must survive a server restart (issue #98).
///
/// Persisted as one JSON blob through [`crate::storage::StorageBackend`]'s
/// generic key/value space, so it works for every backend (local / S3).
/// Every field is optional: `None` means "use the server default", which is
/// exactly how a blob from an older build (absent entirely) is treated.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub(super) struct SessionMeta {
    /// Channel-prepared BASE system prompt (before project-context / skill /
    /// environment assembly), exactly as `create_session` resolved it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// `permission_mode` string as supplied to `create_session`. Re-parsed on
    /// restore so the server's `allow_bypass_permissions` guard still applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// Human-readable title (create-time `session_name`, later `PATCH`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Explicit per-session step cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_steps: Option<usize>,
    /// Issue #127: the agent preset the session was assembled from. Unlike the
    /// other fields this one is persisted even when the session took the
    /// server default — a preset IS the session's runtime wiring (tools,
    /// compaction, re-injection), so restoring it with a different one would
    /// silently rewire a live session. A blob without the field (older build)
    /// restores with the server default, as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// Issue #94: per-session run overrides (USD ceiling, thinking budget).
    /// `default` keeps stored blobs written before this field existed valid.
    #[serde(
        default,
        skip_serializing_if = "crate::http::SessionOverrides::is_default"
    )]
    pub overrides: crate::http::SessionOverrides,
    /// Issue #85: subject that created the session. `None` (older build)
    /// restores as an unattributed session, reachable by admins only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Issue #85: the owner's tenant, part of the ownership key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
}

/// Storage key for the per-session metadata blob (issue #98).
pub(super) fn session_meta_key(id: &str) -> String {
    format!("session-meta/{id}")
}

/// Best-effort persist of the per-session metadata: a storage failure is
/// logged, never fatal — the session is still usable, it just restores with
/// the server defaults (pre-#98 behaviour).
pub(super) async fn persist_session_meta(state: &AppState, id: &str, meta: &SessionMeta) {
    let Ok(json) = serde_json::to_string(meta) else {
        tracing::warn!(session_id = %id, "failed to serialize session metadata");
        return;
    };
    if let Err(e) = state
        .storage
        .save_memory(&session_meta_key(id), &json)
        .await
    {
        tracing::warn!(session_id = %id, error = %e, "failed to persist session metadata");
    }
}

/// Best-effort load of the persisted metadata. A read or parse failure
/// degrades to `None` (server defaults) — a corrupt blob must not make a
/// restorable session 500.
pub(super) async fn load_session_meta(state: &AppState, id: &str) -> Option<SessionMeta> {
    match state.storage.load_memory(&session_meta_key(id)).await {
        Ok(Some(json)) => match serde_json::from_str(&json) {
            Ok(meta) => Some(meta),
            Err(e) => {
                tracing::warn!(
                    session_id = %id,
                    error = %e,
                    "ignoring unparsable session metadata"
                );
                None
            }
        },
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(session_id = %id, error = %e, "failed to load session metadata");
            None
        }
    }
}

/// Mirror a new title into the persisted metadata, preserving every other
/// field. Used by `PATCH /sessions/:id`.
pub(super) async fn update_persisted_title(state: &AppState, id: &str, title: Option<String>) {
    let mut meta = load_session_meta(state, id).await.unwrap_or_default();
    meta.title = title;
    persist_session_meta(state, id, &meta).await;
}

pub(super) async fn get_or_load_session(
    state: &Arc<AppState>,
    id: &str,
    identity: &AuthIdentity,
) -> Result<Arc<SessionState>, ApiError> {
    // Phase 1: short read lock. All mutable session state lives in Arc
    // fields, so a struct-level clone shares it with the table value.
    {
        let host_sessions = state.host.sessions();
        let sessions = host_sessions.read().await;
        if let Some(existing) = sessions.get(id) {
            ensure_access(identity, existing)?;
            return Ok(Arc::new(existing.clone()));
        }
    }

    // Phase 2: IO outside any lock. A deleted session stays deleted: DELETE keeps
    // the snapshot (Goal 396) but leaves a tombstone, and cold load honours it.
    let tombstone = state
        .storage
        .load_memory(&deleted_marker_key(id))
        .await
        .map_err(|e| ApiError::internal(format!("load tombstone for session {id}: {e}")))?;
    if tombstone.is_some() {
        return Err(ApiError::not_found("session not found"));
    }
    let stored = state
        .storage
        .load_transcript(id)
        .await
        .map_err(|e| ApiError::internal(format!("load transcript for session {id}: {e}")))?;
    let seed = normalize_stored_transcript(stored)
        .ok_or_else(|| ApiError::not_found("session not found"))?;
    let non_system_count = seed.len();

    // Phase 3: build the restored runtime with no lock held. The persisted
    // per-session config (issue #98) drives the rebuild; absent, the server
    // defaults apply.
    let meta = load_session_meta(state, id).await;
    // Issue #85: the owner recorded at creation decides access. Checked
    // before the runtime is built so a foreign probe materializes nothing.
    if !identity.may_access_session(
        meta.as_ref().and_then(|m| m.owner.as_deref()),
        meta.as_ref().and_then(|m| m.tenant.as_deref()),
    ) {
        return Err(ApiError::forbidden("session belongs to another identity"));
    }
    // Issue #114: the persisted usage snapshot (token totals + the USD already
    // billed) rides the storage key/value space like the #98 blob.
    let persisted_usage = super::usage::load_persisted_usage(&state.storage, id).await;
    let runtime = build_restored_runtime(state, id, seed, meta.as_ref()).await?;
    let plan_approval_gate = runtime.plan_approval_gate();

    // Issue #114: restore the session's accumulated usage so a server restart
    // does not zero it (the pre-#114 behaviour). A transcript written before
    // this field existed — or a corrupt blob — restores an empty accumulator.
    //
    // The accumulator bills at the *current* server model: the restored cost is
    // frozen at the rates history actually cost, so a restart onto another
    // model prices neither the restored history nor the new turns wrongly.
    let restored_usage = {
        let usage = super::SessionUsage::new(state.config.model.clone());
        if let Some(persisted) = &persisted_usage {
            usage.restore(&persisted.usage);
            usage.restore_cost(persisted.cost_micro_usd);
        }
        Arc::new(usage)
    };

    // Phase 4: short write lock — first insert wins a concurrent race.
    let host_sessions = state.host.sessions();
    let mut sessions = host_sessions.write().await;
    if let Some(existing) = sessions.get(id) {
        return Ok(Arc::new(existing.clone()));
    }
    let session = SessionState {
        id: id.to_string(),
        // `created_at` stays synthesized: it is presentation metadata, not a
        // per-session override, and was never persisted.
        created_at: format_timestamp(SystemTime::now()),
        title: meta.as_ref().and_then(|m| m.title.clone()),
        owner: meta.as_ref().and_then(|m| m.owner.clone()),
        tenant: meta.as_ref().and_then(|m| m.tenant.clone()),
        runtime: Arc::new(tokio::sync::Mutex::new(runtime)),
        plan_approval_gate,
        interrupt_token: Arc::new(tokio::sync::Mutex::new(None)),
        non_system_message_count: Arc::new(std::sync::atomic::AtomicUsize::new(non_system_count)),
        last_active_ms: Arc::new(std::sync::atomic::AtomicU64::new(super::now_session_ms())),
        usage: restored_usage,
        event_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
    };
    // All mutable state lives in Arc fields, so this handle shares everything
    // that matters with the table value.
    let shared = Arc::new(session.clone());
    sessions.insert(id.to_string(), session);
    state
        .metrics
        .sessions_active
        .fetch_add(1, Ordering::Relaxed);
    tracing::info!(session_id = %id, "session cold-loaded from storage");
    Ok(shared)
}

/// Normalize a stored transcript for seeding (see module docs).
///
/// Returns `None` when nothing restorable remains — the caller must keep
/// the 404 semantics and never materialize a ghost session.
fn normalize_stored_transcript(msgs: Vec<Message>) -> Option<Vec<Message>> {
    let mut msgs = msgs;
    // 1. Stored system message → dropped (the runtime rebuilds its own).
    if msgs.first().is_some_and(|m| m.role == Role::System) {
        msgs.remove(0);
    }
    // 2. Leading orphan tool results → dropped until the first non-Tool
    //    message (invariant #8: a Tool result must answer an Assistant
    //    tool_call, which cannot exist before the first non-Tool message).
    match msgs.iter().position(|m| m.role != Role::Tool) {
        Some(0) => {}
        Some(n) => {
            msgs.drain(..n);
        }
        None => msgs.clear(),
    }
    if msgs.is_empty() {
        None
    } else {
        Some(msgs)
    }
}

/// Build a runtime for a cold-loaded session.
///
/// Issue #98: this goes through the very same [`build_session_runtime`] the
/// fresh-session and fork paths use, so a restored session gets the compactor
/// / microcompactor / transcript cap, token streaming and the storage backend
/// — previously it got none of them (unbounded context, one-shot streaming,
/// no persistence). The per-session overrides come from the persisted
/// [`SessionMeta`]; without one (transcript written by an older build) the
/// current server configuration applies, exactly as before.
///
/// [`build_session_runtime`]: super::handlers::build_session_runtime
async fn build_restored_runtime(
    state: &Arc<AppState>,
    id: &str,
    seed: Vec<Message>,
    meta: Option<&SessionMeta>,
) -> Result<AgentRuntime, ApiError> {
    // Base prompt: the persisted per-session prompt wins (issue #98 — a
    // custom persona must not be silently swapped for the server default).
    let base = meta
        .and_then(|m| m.system_prompt.clone())
        .unwrap_or_else(|| state.config.system_prompt.clone());
    // Same system-prompt assembly as every other channel (project context +
    // skill index + sub-agent note).
    let assembled = crate::assemble_system_prompt(
        &base,
        &state.config.workspace,
        &state.skills,
        state.config.subagent_enabled,
    );
    // Issue #31: a restored session gets its OWN environment (container
    // tier) like a fresh one, plus the environment prompt segment.
    let mut tool_registry = state
        .session_tool_registry()
        .await
        .map_err(ApiError::internal)?;
    if let Some(mode_str) = meta.and_then(|m| m.permission_mode.as_deref()) {
        let perm_mode =
            super::handlers::parse_permission_mode(mode_str, state.config.allow_bypass_permissions);
        tool_registry =
            tool_registry.with_permissions(crate::permissions::LayeredPermissionsConfig {
                mode: perm_mode,
                layers: Vec::new(),
            });
    }
    let (full, segments) = super::handlers::inject_environment_segment(
        assembled.full,
        assembled.segments,
        &tool_registry,
    );
    let max_steps = meta
        .and_then(|m| m.max_steps)
        .unwrap_or(state.config.max_steps);
    // Issue #127: restore the preset the session was created with; a session
    // persisted before presets existed (or with the field absent) falls back
    // to the server default.
    let preset = super::handlers::resolve_session_preset(
        meta.and_then(|m| m.preset.as_deref()),
        &state.config,
    )?;
    // Issue #94: restore the session's run overrides so a cold-loaded session
    // keeps the budget / thinking budget its creator asked for.
    let overrides = meta.map(|m| m.overrides).unwrap_or_default();
    let mut runtime = super::handlers::build_session_runtime(
        state,
        tool_registry,
        full,
        segments,
        max_steps,
        &preset,
        overrides,
    )
    .seed_transcript(seed)
    .build()
    .map_err(|e| ApiError::internal(format!("failed to build restored session runtime: {e}")))?;
    runtime.set_session_id(id);
    Ok(runtime)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::now_session_ms;
    use crate::llm::{Completion, MockProvider, ToolCall};
    use crate::runtime::AgentRuntimeBuilder;
    use crate::storage::{LocalStorageBackend, StorageBackend};
    use std::sync::atomic::AtomicU64;

    fn user(content: &str) -> Message {
        Message {
            role: Role::User,
            content: content.to_string(),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: None,
            is_compaction_summary: false,
        }
    }

    fn assistant(content: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: content.to_string(),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: None,
            is_compaction_summary: false,
        }
    }

    fn assistant_with_tool_call(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: id.to_string(),
                name: "Read".to_string(),
                arguments: serde_json::json!({"path": "a.txt"}),
            }],
            tool_call_id: None,
            reasoning_content: None,
            is_compaction_summary: false,
        }
    }

    fn tool_result(call_id: &str) -> Message {
        Message {
            role: Role::Tool,
            content: "file body".to_string(),
            tool_calls: vec![],
            tool_call_id: Some(call_id.to_string()),
            reasoning_content: None,
            is_compaction_summary: false,
        }
    }

    fn system(content: &str) -> Message {
        Message {
            role: Role::System,
            content: content.to_string(),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: None,
            is_compaction_summary: false,
        }
    }

    // ── normalization ──────────────────────────────────────────────────────

    #[test]
    fn normalize_strips_leading_system_message() {
        let out = normalize_stored_transcript(vec![
            system("stored prompt"),
            user("ping"),
            assistant("pong"),
        ])
        .expect("restorable");
        assert_eq!(out.len(), 2);
        assert!(!out.iter().any(|m| m.role == Role::System));
        assert_eq!(out[0].content, "ping");
    }

    #[test]
    fn normalize_strips_leading_orphan_tool_results() {
        let out = normalize_stored_transcript(vec![
            tool_result("call-1"),
            tool_result("call-2"),
            user("after orphans"),
        ])
        .expect("restorable");
        assert!(!out.iter().any(|m| m.role == Role::Tool));
        assert_eq!(out[0].content, "after orphans");
    }

    #[test]
    fn normalize_keeps_valid_tool_pairing_intact() {
        let paired = vec![
            user("list files"),
            assistant_with_tool_call("call-1"),
            tool_result("call-1"),
            assistant("done"),
        ];
        let out = normalize_stored_transcript(paired.clone()).expect("restorable");
        assert_eq!(out, paired);
    }

    #[test]
    fn normalize_system_then_orphans_is_empty_after_strip() {
        // Empty check happens AFTER normalization: a transcript of only a
        // system message plus orphan tool results restores to nothing.
        assert!(normalize_stored_transcript(vec![system("only prompt")]).is_none());
        assert!(normalize_stored_transcript(vec![tool_result("call-1")]).is_none());
        assert!(normalize_stored_transcript(vec![system("p"), tool_result("c")]).is_none());
        assert!(normalize_stored_transcript(vec![]).is_none());
    }

    // ── get_or_load_session ───────────────────────────────────────────────

    fn test_config() -> crate::config::Config {
        crate::config::Config {
            workspace: std::path::PathBuf::from("/tmp"),
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
            shell_timeout_secs: 5,
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

    fn test_state(dir: std::path::PathBuf, completions: Vec<Completion>) -> Arc<AppState> {
        let metrics = Arc::new(crate::http::Metrics::default());
        Arc::new(AppState {
            tools: vec![],
            tool_registry: crate::tools::ToolRegistry::default(),
            config: test_config(),
            provider: Arc::new(MockProvider::new(completions)),
            host: Arc::new(crate::session_host::SessionHost::new(
                std::time::Duration::from_secs(3600),
                crate::http::AdmissionGate::new(
                    8,
                    std::time::Duration::ZERO,
                    Arc::new(AtomicU64::new(0)),
                    Arc::new(AtomicU64::new(0)),
                ),
            )),
            event_channels: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            metrics,
            slash_commands: Arc::new(vec![]),
            rate_limiter: crate::http::RateLimiter::new(10, 1.0),
            skills: vec![],
            storage: Arc::new(LocalStorageBackend::new(dir)),
            agui_active_runs: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            session_mirror_root: None,
        })
    }

    async fn seed(dir: &std::path::Path, id: &str, msgs: Vec<Message>) {
        let backend = LocalStorageBackend::new(dir.to_path_buf());
        backend.save_transcript(id, &msgs).await.unwrap();
    }

    fn restore_env(name: &str, saved: Option<String>) {
        match saved {
            Some(v) => std::env::set_var(name, v),
            None => std::env::remove_var(name),
        }
    }

    #[tokio::test]
    async fn cold_load_restores_single_system_and_valid_pairing() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(
            dir.path().to_path_buf(),
            vec![Completion {
                content: "continued".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            }],
        );
        seed(
            dir.path(),
            "sess-cold-1",
            vec![
                system("stored prompt"),
                user("ping"),
                assistant_with_tool_call("call-1"),
                tool_result("call-1"),
                assistant("pong"),
            ],
        )
        .await;

        let session = get_or_load_session(&state, "sess-cold-1", &AuthIdentity::local())
            .await
            .expect("cold load succeeds");
        let rt = session.runtime.lock().await;
        let transcript = rt.transcript();
        // Exactly one system message — the runtime's own, not the stored one.
        assert_eq!(
            transcript.iter().filter(|m| m.role == Role::System).count(),
            1
        );
        // No persisted metadata → the server default prompt applies (the
        // pre-#98 fallback; a custom prompt instead comes from the meta blob).
        assert!(
            transcript[0].content.contains("test prompt"),
            "unseeded meta must fall back to the server default prompt"
        );
        // Stored content restored in order, pairing intact.
        assert_eq!(
            transcript
                .iter()
                .filter(|m| m.role != Role::System)
                .map(|m| m.role)
                .collect::<Vec<_>>(),
            vec![Role::User, Role::Assistant, Role::Tool, Role::Assistant]
        );
        // Session metadata: synthesized timestamp, no title (minimal semantics).
        assert!(session.title.is_none());
        assert_eq!(
            session
                .non_system_message_count
                .load(std::sync::atomic::Ordering::Relaxed),
            4
        );
        assert_eq!(
            state
                .metrics
                .sessions_active
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    // ── issue #98: persisted per-session metadata ─────────────────────────

    /// Pin the literal key namespace. Save and load share `session_meta_key`,
    /// so a persist/load roundtrip alone cannot detect a key that collapsed to
    /// an empty (or shared) string — the two calls would agree anyway.
    #[test]
    fn storage_keys_are_namespaced_per_session() {
        assert_eq!(session_meta_key("abc"), "session-meta/abc");
        assert_eq!(deleted_marker_key("abc"), "session-deleted/abc");
    }

    #[tokio::test]
    async fn session_meta_roundtrips_through_the_storage_kv() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path().to_path_buf(), vec![]);

        assert!(
            load_session_meta(&state, "absent").await.is_none(),
            "missing key → None (no ghost metadata)"
        );
        let meta = SessionMeta {
            system_prompt: Some("be a pirate".into()),
            permission_mode: Some("auto".into()),
            title: Some("ship it".into()),
            max_steps: Some(7),
            preset: Some("standard".into()),
            overrides: Default::default(),
            owner: Some("alice".into()),
            tenant: Some("acme".into()),
        };
        persist_session_meta(&state, "s1", &meta).await;
        let loaded = load_session_meta(&state, "s1")
            .await
            .expect("persisted meta");
        assert_eq!(loaded.system_prompt.as_deref(), Some("be a pirate"));
        assert_eq!(loaded.permission_mode.as_deref(), Some("auto"));
        assert_eq!(loaded.title.as_deref(), Some("ship it"));
        assert_eq!(loaded.max_steps, Some(7));
        assert_eq!(
            loaded.preset.as_deref(),
            Some("standard"),
            "issue #127: the session's preset rides the #98 metadata path"
        );
        // Issue #85: ownership must survive the round-trip — dropping it on
        // restart would hand the session to the wrong caller (or to nobody).
        assert_eq!(loaded.owner.as_deref(), Some("alice"));
        assert_eq!(loaded.tenant.as_deref(), Some("acme"));
    }

    #[tokio::test]
    async fn update_persisted_title_keeps_the_other_fields() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path().to_path_buf(), vec![]);
        persist_session_meta(
            &state,
            "s1",
            &SessionMeta {
                system_prompt: Some("keep me".into()),
                permission_mode: None,
                title: None,
                max_steps: Some(9),
                preset: None,
                overrides: Default::default(),
                owner: Some("alice".into()),
                tenant: None,
            },
        )
        .await;

        update_persisted_title(&state, "s1", Some("renamed".into())).await;
        let loaded = load_session_meta(&state, "s1").await.expect("meta");
        assert_eq!(loaded.title.as_deref(), Some("renamed"));
        assert_eq!(loaded.system_prompt.as_deref(), Some("keep me"));
        assert_eq!(loaded.max_steps, Some(9));
        assert_eq!(
            loaded.owner.as_deref(),
            Some("alice"),
            "a title rename must not drop the session's owner"
        );
    }

    /// Issue #98 acceptance: a session created with a custom `system_prompt`
    /// and a non-default `permission_mode` restores BOTH, and the restored
    /// runtime keeps context management — previously the cold-load path built
    /// a compactor-less runtime and swapped in the server default prompt.
    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)]
    async fn cold_load_restores_custom_prompt_mode_and_context_management() {
        // `apply_context_management` reads the compaction threshold from the
        // environment; hold the process-global env lock for the whole test so
        // a parallel test can neither disable the compactor nor leave a stale
        // threshold behind. (current_thread: the guard is held across awaits.)
        let _env = crate::test_util::env_lock();
        let saved_threshold = std::env::var("RECURSIVE_COMPACT_THRESHOLD").ok();
        let saved_cap = std::env::var("RECURSIVE_MAX_TRANSCRIPT_CHARS").ok();
        std::env::remove_var("RECURSIVE_COMPACT_THRESHOLD");
        std::env::remove_var("RECURSIVE_MAX_TRANSCRIPT_CHARS");

        let dir = tempfile::tempdir().unwrap();
        let mut state = test_state(
            dir.path().to_path_buf(),
            vec![Completion {
                content: "earlier conversation summary".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            }],
        );
        // A real tool surface carries the shared read state the preset's file
        // re-injector needs (issue #127); the bare fixture registry has none.
        Arc::get_mut(&mut state)
            .expect("freshly built state is uniquely owned")
            .tool_registry = crate::tools::build_standard_tools(dir.path(), &[], 60);
        let mut long = Vec::new();
        for i in 0..6 {
            long.push(user(&format!("q{i}")));
            long.push(assistant(&format!("a{i}")));
        }
        seed(dir.path(), "sess-98", long).await;
        persist_session_meta(
            &state,
            "sess-98",
            &SessionMeta {
                system_prompt: Some("You are a pirate.".into()),
                permission_mode: Some("auto".into()),
                title: Some("pirate chat".into()),
                max_steps: Some(7),
                preset: Some("standard".into()),
                overrides: Default::default(),
                owner: None,
                tenant: None,
            },
        )
        .await;

        let session = get_or_load_session(&state, "sess-98", &AuthIdentity::local())
            .await
            .expect("cold load");
        assert_eq!(
            session.title.as_deref(),
            Some("pirate chat"),
            "the persisted title must survive the restart"
        );
        let mut rt = session.runtime.lock().await;
        assert!(
            rt.transcript()[0].content.contains("You are a pirate."),
            "the custom system prompt must survive the restart"
        );
        assert!(
            matches!(
                rt.kernel().tools().permission_mode(),
                crate::permissions::PermissionMode::Auto
            ),
            "the persisted permission mode must be restored"
        );
        assert_eq!(
            rt.kernel().max_steps,
            7,
            "the persisted per-session step cap must be restored"
        );
        assert!(
            rt.has_compactor(),
            "a restored session must keep context management (issue #98)"
        );
        // Issue #127: the session's preset survives the restart and is what
        // the runtime reports — and a restored runtime gets the preset's full
        // context half, post-compaction re-injection included.
        assert_eq!(
            rt.preset_id(),
            Some("standard"),
            "the persisted preset must drive the restore"
        );
        assert!(
            rt.context_management_facts()
                .reinject_recent_files
                .is_some(),
            "a restored session must re-inject recently-read files after compaction"
        );
        // And it actually fires on the long restored transcript.
        rt.compact_now().await.expect("compaction runs");
        assert!(
            rt.transcript().iter().any(|m| m.is_compaction_summary),
            "the restored session must be able to compact a long conversation"
        );
        drop(rt);

        restore_env("RECURSIVE_COMPACT_THRESHOLD", saved_threshold);
        restore_env("RECURSIVE_MAX_TRANSCRIPT_CHARS", saved_cap);
    }

    /// The `allow_bypass_permissions` guard still applies to restored
    /// sessions: a persisted `bypass` mode must not outlive the server policy.
    #[tokio::test]
    async fn cold_load_ignores_persisted_bypass_when_server_disallows_it() {
        let dir = tempfile::tempdir().unwrap();
        // `test_config()` sets allow_bypass_permissions = false.
        let state = test_state(dir.path().to_path_buf(), vec![]);
        seed(dir.path(), "sess-bypass", vec![user("hi"), assistant("yo")]).await;
        persist_session_meta(
            &state,
            "sess-bypass",
            &SessionMeta {
                system_prompt: None,
                permission_mode: Some("bypass".into()),
                title: None,
                max_steps: None,
                preset: None,
                overrides: Default::default(),
                owner: None,
                tenant: None,
            },
        )
        .await;

        let session = get_or_load_session(&state, "sess-bypass", &AuthIdentity::local())
            .await
            .expect("cold load");
        let rt = session.runtime.lock().await;
        assert!(
            matches!(
                rt.kernel().tools().permission_mode(),
                crate::permissions::PermissionMode::Default
            ),
            "bypass must be re-parsed against the server's allow_bypass policy"
        );
    }

    #[tokio::test]
    async fn cold_load_empty_storage_keeps_404_and_no_ghost() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path().to_path_buf(), vec![]);

        let err = match get_or_load_session(&state, "no-such-session", &AuthIdentity::local()).await
        {
            Ok(_) => panic!("unknown id must stay 404"),
            Err(e) => e,
        };
        assert_eq!(err.status, axum::http::StatusCode::NOT_FOUND);
        // No ghost session was materialized.
        assert_eq!(state.host.sessions().read().await.len(), 0);
        assert_eq!(
            state
                .metrics
                .sessions_active
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }

    /// Issue #85: ownership comes from the persisted metadata, and a foreign
    /// probe is refused BEFORE the runtime is built — it must not materialize
    /// someone else's session (nor count it in the sessions gauge).
    #[tokio::test]
    async fn cold_load_refuses_a_session_owned_by_someone_else() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path().to_path_buf(), vec![]);
        seed(dir.path(), "sess-owned", vec![user("hi"), assistant("yo")]).await;
        persist_session_meta(
            &state,
            "sess-owned",
            &SessionMeta {
                owner: Some("alice".into()),
                tenant: Some("acme".into()),
                ..Default::default()
            },
        )
        .await;

        for (subject, tenant) in [("bob", Some("acme")), ("alice", Some("globex"))] {
            let foreign = AuthIdentity {
                subject: subject.into(),
                tenant: tenant.map(str::to_string),
                admin: false,
            };
            let err = match get_or_load_session(&state, "sess-owned", &foreign).await {
                Ok(_) => panic!("a foreign identity must be refused"),
                Err(e) => e,
            };
            assert_eq!(err.status, axum::http::StatusCode::FORBIDDEN);
        }
        assert_eq!(
            state.host.sessions().read().await.len(),
            0,
            "a refused probe must not materialize the session"
        );

        let owner = AuthIdentity {
            subject: "alice".into(),
            tenant: Some("acme".into()),
            admin: false,
        };
        let session = get_or_load_session(&state, "sess-owned", &owner)
            .await
            .expect("the owner cold-loads its own session");
        assert_eq!(session.owner.as_deref(), Some("alice"));
        assert_eq!(session.tenant.as_deref(), Some("acme"));
    }

    #[tokio::test]
    async fn cold_load_memory_hit_skips_storage() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path().to_path_buf(), vec![]);
        // Pre-insert a session directly (as create_session would).
        let runtime = AgentRuntimeBuilder::new()
            .llm(Arc::new(MockProvider::new(vec![])) as Arc<dyn crate::ChatProvider>)
            .build()
            .unwrap();
        let gate = runtime.plan_approval_gate();
        state.host.sessions().write().await.insert(
            "live-session".to_string(),
            SessionState {
                id: "live-session".to_string(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                title: Some("live".to_string()),
                owner: None,
                tenant: None,
                runtime: Arc::new(tokio::sync::Mutex::new(runtime)),
                plan_approval_gate: gate,
                interrupt_token: Arc::new(tokio::sync::Mutex::new(None)),
                non_system_message_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                last_active_ms: Arc::new(AtomicU64::new(now_session_ms())),
                usage: Arc::new(crate::http::SessionUsage::new("test-model")),
                event_seq: Arc::new(AtomicU64::new(0)),
            },
        );

        let session = get_or_load_session(&state, "live-session", &AuthIdentity::local())
            .await
            .expect("memory hit");
        // The live title proves it came from the table, not a rebuild.
        assert_eq!(session.title.as_deref(), Some("live"));
    }

    #[tokio::test]
    async fn cold_load_continues_conversation_with_paired_growth() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(
            dir.path().to_path_buf(),
            vec![Completion {
                content: "second reply".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            }],
        );
        seed(
            dir.path(),
            "sess-cold-2",
            vec![user("first"), assistant("first reply")],
        )
        .await;

        let session = get_or_load_session(&state, "sess-cold-2", &AuthIdentity::local())
            .await
            .expect("cold load");
        session
            .runtime
            .lock()
            .await
            .enqueue("second")
            .await
            .expect("turn runs on restored session");
        let transcript = session.runtime.lock().await.transcript().to_vec();
        // Seeded turn + new turn, in order, pairing still legal.
        let roles: Vec<Role> = transcript
            .iter()
            .filter(|m| m.role != Role::System)
            .map(|m| m.role)
            .collect();
        assert_eq!(
            roles,
            vec![Role::User, Role::Assistant, Role::User, Role::Assistant]
        );
        assert_eq!(
            transcript.last().map(|m| m.content.as_str()),
            Some("second reply")
        );
        // Invariant #8 walk: every Tool message answered by the preceding
        // assistant's tool_calls (trivially satisfied here — no Tool roles).
        for (i, m) in transcript.iter().enumerate() {
            if m.role == Role::Tool {
                let prev = &transcript[i - 1];
                assert!(prev
                    .tool_calls
                    .iter()
                    .any(|c| Some(&c.id) == m.tool_call_id.as_ref()));
            }
        }
    }

    // ── issue #114: usage survives a restart ──────────────────────────────

    /// Before #114 the restored token counters were hardcoded to 0 — a restart
    /// silently zeroed every session's usage. A cold load must now restore the
    /// persisted snapshot, billed USD included; the accumulator then prices
    /// *new* turns at the current server model.
    #[tokio::test]
    async fn cold_load_restores_persisted_usage() {
        // Pin RECURSIVE_HOME so the effective catalog collapses to the bundled
        // prices (a stray providers cache must not change the asserted USD).
        let home = tempfile::tempdir().unwrap();
        let _pin = crate::test_util::PinnedRecursiveHome::new(home.path());
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path().to_path_buf(), vec![]);
        seed(dir.path(), "sess-usage", vec![user("hi"), assistant("yo")]).await;

        let usage = super::super::SessionUsage::new("deepseek-chat");
        usage.record(
            crate::llm::TokenUsage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                cache_hit_tokens: 40,
                cache_miss_tokens: 60,
                reasoning_tokens: 0,
            },
            777,
        );
        crate::http::usage::persist_usage(&state.storage, "sess-usage", &usage).await;

        let session = get_or_load_session(&state, "sess-usage", &AuthIdentity::local())
            .await
            .expect("cold load");
        let totals = session.usage.snapshot();
        assert_eq!(totals.prompt_tokens, 100);
        assert_eq!(totals.completion_tokens, 50);
        assert_eq!(totals.total_tokens, 150);
        assert_eq!(totals.cache_hit_tokens, 40);
        assert_eq!(totals.cache_miss_tokens, 60);
        assert_eq!(totals.llm_latency_ms, 777);
        assert_eq!(
            session.usage.cost_usd(),
            Some(0.000_023),
            "the USD the turn was billed at must survive the restart"
        );
        assert_eq!(
            session.usage.model(),
            state.config.model,
            "a restored session bills its new turns at the current server model"
        );
    }

    /// Issue #114: USD is frozen at the rates history was billed at, so a
    /// restart onto a *different* model neither re-bills history nor prices
    /// new turns at the stale rate.
    #[tokio::test]
    async fn cold_load_does_not_reprice_history_at_the_new_model() {
        let home = tempfile::tempdir().unwrap();
        let _pin = crate::test_util::PinnedRecursiveHome::new(home.path());
        let dir = tempfile::tempdir().unwrap();
        let mut state = test_state(dir.path().to_path_buf(), vec![]);
        Arc::get_mut(&mut state)
            .expect("unique state handle")
            .config
            .model = "MiniMax-M3".into();
        seed(dir.path(), "sess-model", vec![user("hi"), assistant("yo")]).await;

        // 1M completion tokens at deepseek-chat's $0.28/M is $0.28.
        let usage = super::super::SessionUsage::new("deepseek-chat");
        usage.record(
            crate::llm::TokenUsage {
                prompt_tokens: 0,
                completion_tokens: 1_000_000,
                total_tokens: 1_000_000,
                ..Default::default()
            },
            0,
        );
        crate::http::usage::persist_usage(&state.storage, "sess-model", &usage).await;

        let session = get_or_load_session(&state, "sess-model", &AuthIdentity::local())
            .await
            .expect("cold load");
        assert_eq!(
            session.usage.cost_usd(),
            Some(0.28),
            "history keeps the rate it was billed at"
        );
        assert_eq!(session.usage.model(), "MiniMax-M3");
    }

    /// A transcript written before #114 has no usage blob — it must restore at
    /// zero (priced at the current server model), not fail.
    #[tokio::test]
    async fn cold_load_without_persisted_usage_starts_at_zero() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path().to_path_buf(), vec![]);
        seed(dir.path(), "sess-legacy", vec![user("hi"), assistant("yo")]).await;

        let session = get_or_load_session(&state, "sess-legacy", &AuthIdentity::local())
            .await
            .expect("cold load");
        assert!(session.usage.snapshot().is_zero());
        assert_eq!(session.usage.model(), state.config.model);
    }
}
