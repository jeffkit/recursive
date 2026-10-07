//! Audit retrieval/export endpoints and the request → audit-event middleware
//! (issue #101).
//!
//! The append-only, hash-chained stream lives in [`crate::audit_log`]. This
//! module exposes it over HTTP and, crucially, attributes request-driven
//! events: the middleware sits **inside** the auth layer, so it sees the
//! resolved [`AuthIdentity`] and can record *who* approved a plan / deleted a
//! session — something the per-tool [`crate::tools::AuditMeta`] cannot say.
//!
//! The stream itself is process-global, so both read endpoints ([`list_audit`],
//! [`export_audit`]) hand a non-admin caller only its own records
//! ([`visible_to`]) — audited data is exactly as tenant-sensitive as the
//! sessions and triggers #85 already scopes per owner.

use axum::extract::{Extension, Query, Request};
use axum::http::Method;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;

use super::{ApiError, AuthIdentity};
use crate::audit_log::{AuditAction, AuditActor, AuditQuery, AuditRecord};

/// Records returned by `/audit` and `/audit/export` when the caller did not
/// ask for a `limit`.
///
/// The stream is append-only with no rotation, and every 401/503 appends a
/// line, so an unbounded response is a growth vector for any credential
/// holder. Callers that need more pass an explicit `limit` (or page by
/// `from`/`to`).
pub(super) const DEFAULT_AUDIT_LIMIT: usize = 1000;

/// Query parameters for `GET /audit` and `GET /audit/export`.
#[derive(serde::Deserialize, Debug, Default)]
pub(super) struct AuditQueryParams {
    /// Inclusive lower bound on record timestamp (epoch millis).
    from: Option<i64>,
    /// Inclusive upper bound on record timestamp (epoch millis).
    to: Option<i64>,
    /// Filter to a caller subject.
    actor: Option<String>,
    /// Filter to a caller tenant.
    tenant: Option<String>,
    /// Filter to a session id.
    session: Option<String>,
    /// Keep only the most recent N matching records
    /// (default [`DEFAULT_AUDIT_LIMIT`]).
    limit: Option<usize>,
    /// Recompute and report the hash chain when true.
    #[serde(default)]
    verify: bool,
}

impl AuditQueryParams {
    fn into_query(self) -> AuditQuery {
        AuditQuery {
            from_ts: self.from,
            to_ts: self.to,
            actor: self.actor,
            tenant: self.tenant,
            session: self.session,
            limit: Some(self.limit.unwrap_or(DEFAULT_AUDIT_LIMIT)),
        }
    }
}

/// The records `identity` may read: an admin sees the whole stream, everyone
/// else only their own events — same subject **and** tenant, the rule
/// [`AuthIdentity::may_access_session`] applies to sessions. Retrieval is the
/// only place the stream leaves the process, so it is the one place that has
/// to re-apply the isolation the per-owner handlers enforce (`#85`).
fn visible_to(records: Vec<AuditRecord>, identity: &AuthIdentity) -> Vec<AuditRecord> {
    if identity.admin {
        return records;
    }
    records
        .into_iter()
        .filter(|r| r.actor.subject == identity.subject && r.actor.tenant == identity.tenant)
        .collect()
}

/// Recompute the hash chain and render the verdict for the `/audit` response.
fn chain_report() -> serde_json::Value {
    match crate::audit_log::verify() {
        Ok(n) => serde_json::json!({ "ok": true, "verified_records": n }),
        Err(e) => serde_json::json!({ "ok": false, "error": e.message() }),
    }
}

/// GET /audit — retrieve the audit stream, filtered.
///
/// The enterprise audit trail is deliberately separate from the transcript:
/// append-only, hash-chained and attributed to a caller, so retrieval does
/// not depend on the mutable per-session files a filesystem writer can
/// rewrite.
///
/// The stream is process-global (it spans tenants), so retrieval is scoped to
/// the caller unless the caller is an admin.
pub(super) async fn list_audit(
    Query(params): Query<AuditQueryParams>,
    Extension(identity): Extension<AuthIdentity>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let verify = params.verify;
    let chain = verify.then(chain_report);
    let filtered = match crate::audit_log::records() {
        Ok(records) => params.into_query().apply(visible_to(records, &identity)),
        // A caller that asked for chain verification gets the verdict, not a
        // 500: an unparsable line is exactly what `chain.ok = false` reports.
        Err(_) if verify => Vec::new(),
        Err(e) => return Err(ApiError::internal(format!("audit log read failed: {e}"))),
    };
    Ok(Json(serde_json::json!({
        "enabled": crate::audit_log::is_enabled(),
        "count": filtered.len(),
        "chain": chain,
        "records": filtered,
    })))
}

/// GET /audit/export — the (filtered) audit stream as NDJSON.
///
/// Newline-delimited JSON is the shape SIEM collectors ingest without a
/// bespoke parser; each line is exactly one [`crate::audit_log::AuditRecord`].
/// Scoped like `/audit`: a non-admin exports only its own records.
pub(super) async fn export_audit(
    Query(params): Query<AuditQueryParams>,
    Extension(identity): Extension<AuthIdentity>,
) -> Result<Response, ApiError> {
    let records = crate::audit_log::records()
        .map_err(|e| ApiError::internal(format!("audit log read failed: {e}")))?;
    let filtered = params.into_query().apply(visible_to(records, &identity));
    let mut body = String::new();
    for record in filtered {
        let line = serde_json::to_string(&record)
            .map_err(|e| ApiError::internal(format!("audit serialize failed: {e}")))?;
        body.push_str(&line);
        body.push('\n');
    }
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/x-ndjson")],
        body,
    )
        .into_response())
}

/// The audit event a successful request represents, if any.
///
/// Only mutations with a clear security meaning are classified: plan
/// approval/rejection and administrative session operations. Reads are not
/// audited (they are high-volume and carry no state change).
fn classify(
    method: &Method,
    path: &str,
    query: Option<&str>,
) -> Option<(AuditAction, Option<String>)> {
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    match (method, segments.as_slice()) {
        (&Method::POST, ["sessions", id, "plan", "confirm"]) => Some((
            AuditAction::ApprovalDecision {
                decision: "approved".to_string(),
            },
            Some((*id).to_string()),
        )),
        (&Method::POST, ["sessions", id, "plan", "reject"]) => Some((
            AuditAction::ApprovalDecision {
                decision: "rejected".to_string(),
            },
            Some((*id).to_string()),
        )),
        (&Method::DELETE, ["sessions", id]) => {
            let purge = query
                .map(|q| q.split('&').any(|pair| pair == "purge=true"))
                .unwrap_or(false);
            Some((
                AuditAction::AdminAction {
                    action: if purge {
                        "purge_session"
                    } else {
                        "delete_session"
                    }
                    .to_string(),
                },
                Some((*id).to_string()),
            ))
        }
        (&Method::PATCH, ["sessions", id]) => Some((
            AuditAction::AdminAction {
                action: "patch_session".to_string(),
            },
            Some((*id).to_string()),
        )),
        (&Method::POST, ["sessions"]) => Some((
            AuditAction::AdminAction {
                action: "create_session".to_string(),
            },
            None,
        )),
        _ => None,
    }
}

/// The session id a request path addresses, if any (`/sessions/{id}…`).
fn session_id_from_path(path: &str) -> Option<String> {
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    match segments.as_slice() {
        ["sessions", id, ..] => Some((*id).to_string()),
        _ => None,
    }
}

/// Record the caller behind a session so in-process tool dispatch can be
/// attributed, and emit an audit event for the classified mutations.
///
/// Layered inside the auth middleware, so `AuthIdentity` is already in the
/// request extensions. An event — and a session attribution — survives only
/// when the operation actually succeeded: a rejected plan, a 404 or a 403
/// leaves no audit trail claiming otherwise, and the attribution it briefly
/// displaced is put back.
pub(super) async fn audit_middleware(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(str::to_string);
    let identity = req.extensions().get::<AuthIdentity>().cloned();

    // A *mutating* session request may execute tools whose audit events are
    // emitted while the turn is in flight, so the caller has to be registered
    // before the handler runs — otherwise those events fall back to the local
    // operator. Reads never register: they cannot dispatch tools under the
    // caller's identity, so a plain `GET /sessions/{id}` must not be able to
    // claim a session it may not even be allowed to see.
    //
    // Registration is last-writer-wins, and only the handler can tell whether
    // this caller owns the session — so the binding it displaces is restored
    // below when the handler refuses the request (403/404/409).
    let mutating = method != Method::GET && method != Method::HEAD;
    let displaced = match (&identity, mutating) {
        (Some(identity), true) => session_id_from_path(&path).map(|session| {
            let previous = crate::audit_log::session_actor(&session);
            crate::audit_log::register_session_actor(&session, actor_from_identity(identity));
            (session, previous)
        }),
        _ => None,
    };

    let response = next.run(req).await;

    if !response.status().is_success() {
        if let Some((session, previous)) = displaced {
            match previous {
                Some(actor) => crate::audit_log::register_session_actor(&session, actor),
                None => crate::audit_log::forget_session_actor(&session),
            }
        }
    } else if let Some((action, session)) = classify(&method, &path, query.as_deref()) {
        let actor = identity
            .as_ref()
            .map(actor_from_identity)
            .unwrap_or_else(AuditActor::anonymous);
        let is_delete = matches!(
            &action,
            AuditAction::AdminAction { action }
                if action == "delete_session" || action == "purge_session"
        );
        if let (true, Some(sid)) = (is_delete, session.as_ref()) {
            crate::audit_log::forget_session_actor(sid);
        }
        crate::audit_log::emit(actor, session, action);
    }

    response
}

fn actor_from_identity(identity: &AuthIdentity) -> AuditActor {
    AuditActor::new(identity.subject.clone(), identity.tenant.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit_log::test_lock;
    use tower::ServiceExt;

    fn identity(subject: &str, tenant: Option<&str>) -> AuthIdentity {
        AuthIdentity {
            subject: subject.to_string(),
            tenant: tenant.map(str::to_string),
            admin: false,
        }
    }

    fn admin() -> AuthIdentity {
        AuthIdentity {
            subject: "root".to_string(),
            tenant: None,
            admin: true,
        }
    }

    /// App whose session routes answer with `status`, behind the audit layer.
    /// The identity is injected into the request instead of being resolved by
    /// the auth layer (which the real router stacks outside this one).
    fn audit_app(status: axum::http::StatusCode) -> axum::Router {
        axum::Router::new()
            .route(
                "/sessions/{id}",
                axum::routing::get(move || async move { status })
                    .delete(move || async move { status }),
            )
            .route(
                "/sessions/{id}/messages",
                axum::routing::post(move || async move { status }),
            )
            .layer(axum::middleware::from_fn(audit_middleware))
    }

    fn session_request(method: &str, uri: &str, identity: &AuthIdentity) -> Request {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .body(axum::body::Body::empty())
            .expect("request");
        req.extensions_mut().insert(identity.clone());
        req
    }

    #[test]
    fn classify_recognises_plan_decisions() {
        let (action, session) = classify(&Method::POST, "/sessions/abc/plan/confirm", None)
            .expect("confirm classified");
        assert_eq!(session.as_deref(), Some("abc"));
        assert_eq!(
            action,
            AuditAction::ApprovalDecision {
                decision: "approved".into()
            }
        );
        let (action, _) =
            classify(&Method::POST, "/sessions/abc/plan/reject", None).expect("reject classified");
        assert_eq!(
            action,
            AuditAction::ApprovalDecision {
                decision: "rejected".into()
            }
        );
    }

    #[test]
    fn classify_recognises_admin_mutations_and_query() {
        let (action, session) =
            classify(&Method::DELETE, "/sessions/abc", Some("purge=true")).expect("delete");
        assert_eq!(session.as_deref(), Some("abc"));
        assert_eq!(
            action,
            AuditAction::AdminAction {
                action: "purge_session".into()
            }
        );
        let (action, _) = classify(&Method::DELETE, "/sessions/abc", None).expect("delete");
        assert_eq!(
            action,
            AuditAction::AdminAction {
                action: "delete_session".into()
            }
        );
        assert!(matches!(
            classify(&Method::PATCH, "/sessions/abc", None),
            Some((AuditAction::AdminAction { .. }, _))
        ));
        assert!(matches!(
            classify(&Method::POST, "/sessions", None),
            Some((AuditAction::AdminAction { .. }, None))
        ));
    }

    #[test]
    fn classify_ignores_reads_and_unrelated_routes() {
        assert!(classify(&Method::GET, "/sessions/abc", None).is_none());
        assert!(classify(&Method::GET, "/audit", None).is_none());
        assert!(classify(&Method::POST, "/sessions/abc/messages", None).is_none());
        assert!(classify(&Method::DELETE, "/tools", None).is_none());
    }

    #[test]
    fn session_id_from_path_extracts_only_session_routes() {
        assert_eq!(session_id_from_path("/sessions/xyz"), Some("xyz".into()));
        assert_eq!(
            session_id_from_path("/sessions/xyz/messages"),
            Some("xyz".into())
        );
        assert_eq!(session_id_from_path("/sessions"), None);
        assert_eq!(session_id_from_path("/tools"), None);
    }

    #[test]
    fn query_params_map_to_audit_query() {
        let params = AuditQueryParams {
            from: Some(10),
            to: Some(20),
            actor: Some("alice".into()),
            tenant: Some("acme".into()),
            session: Some("s1".into()),
            limit: Some(5),
            verify: true,
        };
        let q = params.into_query();
        assert_eq!(q.from_ts, Some(10));
        assert_eq!(q.to_ts, Some(20));
        assert_eq!(q.actor.as_deref(), Some("alice"));
        assert_eq!(q.tenant.as_deref(), Some("acme"));
        assert_eq!(q.session.as_deref(), Some("s1"));
        assert_eq!(q.limit, Some(5));
    }

    #[test]
    fn query_params_default_to_the_response_cap() {
        let q = AuditQueryParams::default().into_query();
        assert_eq!(q.limit, Some(DEFAULT_AUDIT_LIMIT));
        let q = AuditQueryParams {
            limit: Some(5),
            ..Default::default()
        }
        .into_query();
        assert_eq!(q.limit, Some(5), "an explicit limit wins");
    }

    #[test]
    fn visible_to_hands_a_non_admin_only_its_own_records() {
        let record = |subject: &str, tenant: Option<&str>| AuditRecord {
            seq: 0,
            ts: 0,
            actor: AuditActor::new(subject, tenant.map(str::to_string)),
            session: None,
            action: AuditAction::AuthFailure { reason: "x".into() },
            prev_hash: crate::audit_log::GENESIS_HASH.to_string(),
            hash: "h".into(),
        };
        let records = vec![
            record("alice", Some("acme")),
            // Same subject, different tenant: a different owner (#85).
            record("alice", Some("other")),
            record("bob", None),
        ];

        let mine = visible_to(records.clone(), &identity("alice", Some("acme")));
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].actor.tenant.as_deref(), Some("acme"));
        assert_eq!(visible_to(records.clone(), &identity("bob", None)).len(), 1);
        assert_eq!(
            visible_to(records.clone(), &admin()).len(),
            3,
            "an admin sees the whole cross-tenant stream"
        );
    }

    #[tokio::test]
    // The process-wide audit lock is a std Mutex held for the whole test (to
    // serialize against the other audit tests); it deliberately spans the
    // awaited handler call, so the lint's guidance does not apply here.
    #[allow(clippy::await_holding_lock)]
    async fn list_audit_filters_and_scopes_to_the_caller() {
        let _guard = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        crate::audit_log::set_log_path(&path).expect("bind");
        crate::audit_log::emit(
            AuditActor::new("alice", Some("acme".into())),
            Some("http-audit-test".into()),
            AuditAction::AdminAction {
                action: "delete_session".into(),
            },
        );
        // Same session, another tenant's actor: visible to an admin only.
        crate::audit_log::emit(
            AuditActor::new("bob", Some("globex".into())),
            Some("http-audit-test".into()),
            AuditAction::ToolCall {
                tool: "Write".into(),
                side_effect: "mutating".into(),
                ok: true,
            },
        );

        let params = || AuditQueryParams {
            session: Some("http-audit-test".into()),
            verify: true,
            ..Default::default()
        };
        let Json(body) = list_audit(Query(params()), Extension(identity("alice", Some("acme"))))
            .await
            .expect("list ok");
        assert_eq!(body["count"], 1, "a non-admin reads only its own records");
        assert_eq!(body["records"][0]["actor"]["subject"], "alice");
        assert_eq!(body["chain"]["ok"], true);

        let Json(all) = list_audit(Query(params()), Extension(admin()))
            .await
            .expect("list ok");
        assert_eq!(all["count"], 2, "an admin reads the cross-tenant stream");
        crate::audit_log::clear();
    }

    #[tokio::test]
    // Same rationale as `list_audit_filters_and_scopes_to_the_caller`.
    #[allow(clippy::await_holding_lock)]
    async fn list_audit_answers_a_broken_chain_with_a_verdict() {
        let _guard = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        crate::audit_log::set_log_path(&path).expect("bind");
        crate::audit_log::emit(
            AuditActor::new("alice", Some("acme".into())),
            None,
            AuditAction::AuthFailure {
                reason: "bad key".into(),
            },
        );
        let mut text = std::fs::read_to_string(&path).expect("read");
        text.push_str("{ truncated\n");
        std::fs::write(&path, text).expect("write");

        let Json(body) = list_audit(
            Query(AuditQueryParams {
                verify: true,
                ..Default::default()
            }),
            Extension(admin()),
        )
        .await
        .expect("a verification request answers with a verdict, not a 500");

        assert_eq!(body["chain"]["ok"], false);
        assert!(body["chain"]["error"]
            .as_str()
            .expect("error message")
            .contains("unparsable"));
        crate::audit_log::clear();
    }

    #[tokio::test]
    // Same rationale as `list_audit_filters_and_scopes_to_the_caller`.
    #[allow(clippy::await_holding_lock)]
    async fn export_audit_returns_ndjson_scoped_to_the_caller() {
        let _guard = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        crate::audit_log::set_log_path(&path).expect("bind");
        crate::audit_log::emit(
            AuditActor::new("bob", None),
            Some("http-export-test".into()),
            AuditAction::ToolCall {
                tool: "Write".into(),
                side_effect: "mutating".into(),
                ok: true,
            },
        );
        crate::audit_log::emit(
            AuditActor::new("alice", Some("acme".into())),
            Some("http-export-test".into()),
            AuditAction::ToolCall {
                tool: "Bash".into(),
                side_effect: "external".into(),
                ok: false,
            },
        );

        let response = export_audit(
            Query(AuditQueryParams {
                session: Some("http-export-test".into()),
                ..Default::default()
            }),
            Extension(identity("bob", None)),
        )
        .await
        .expect("export ok");

        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/x-ndjson")
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("\"tool\":\"Write\""));
        assert!(
            !text.contains("Bash"),
            "another caller's records must not be exported"
        );
        crate::audit_log::clear();
    }

    #[tokio::test]
    async fn middleware_attributes_a_successful_mutation_to_the_caller() {
        let app = audit_app(axum::http::StatusCode::OK);

        let response = app
            .oneshot(session_request(
                "POST",
                "/sessions/reg-ok/messages",
                &identity("alice", Some("acme")),
            ))
            .await
            .expect("response");

        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(
            crate::audit_log::session_actor("reg-ok"),
            Some(AuditActor::new("alice", Some("acme".into()))),
            "tools dispatched by a successful turn must find the caller"
        );
        crate::audit_log::forget_session_actor("reg-ok");
    }

    #[tokio::test]
    async fn middleware_never_attributes_a_read() {
        let app = audit_app(axum::http::StatusCode::OK);

        let response = app
            .oneshot(session_request(
                "GET",
                "/sessions/victim-read",
                &identity("mallory", None),
            ))
            .await
            .expect("response");

        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert!(
            crate::audit_log::session_actor("victim-read").is_none(),
            "a read cannot dispatch tools, so it must not claim a session"
        );
    }

    #[tokio::test]
    async fn middleware_drops_the_attribution_when_the_handler_refuses() {
        let app = audit_app(axum::http::StatusCode::FORBIDDEN);

        let response = app
            .oneshot(session_request(
                "POST",
                "/sessions/refused/messages",
                &identity("mallory", None),
            ))
            .await
            .expect("response");

        assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
        assert!(
            crate::audit_log::session_actor("refused").is_none(),
            "a 403 must leave the stream unable to blame the rejected caller"
        );
    }

    #[tokio::test]
    async fn middleware_restores_the_displaced_attribution_on_refusal() {
        crate::audit_log::register_session_actor(
            "displaced",
            AuditActor::new("bob", Some("acme".into())),
        );
        let app = audit_app(axum::http::StatusCode::FORBIDDEN);

        let response = app
            .oneshot(session_request(
                "DELETE",
                "/sessions/displaced",
                &identity("mallory", None),
            ))
            .await
            .expect("response");

        assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
        assert_eq!(
            crate::audit_log::session_actor("displaced"),
            Some(AuditActor::new("bob", Some("acme".into()))),
            "the true owner keeps its attribution"
        );
        crate::audit_log::forget_session_actor("displaced");
    }

    #[tokio::test]
    // The audit log lock spans the awaited handler call on purpose (see
    // `list_audit_filters_and_scopes_to_the_caller`).
    #[allow(clippy::await_holding_lock)]
    async fn middleware_records_a_session_deletion_and_forgets_it() {
        let _guard = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        crate::audit_log::set_log_path(dir.path().join("audit.jsonl")).expect("bind");
        crate::audit_log::register_session_actor(
            "gone",
            AuditActor::new("alice", Some("acme".into())),
        );
        let app = audit_app(axum::http::StatusCode::OK);

        let response = app
            .oneshot(session_request(
                "DELETE",
                "/sessions/gone",
                &identity("alice", Some("acme")),
            ))
            .await
            .expect("response");

        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert!(crate::audit_log::session_actor("gone").is_none());
        let records = crate::audit_log::records().expect("records");
        assert!(
            records.iter().any(|r| {
                r.session.as_deref() == Some("gone")
                    && r.actor.subject == "alice"
                    && matches!(&r.action, AuditAction::AdminAction { action } if action == "delete_session")
            }),
            "the deletion must be attributed to the deleting caller"
        );
        crate::audit_log::clear();
    }
}
