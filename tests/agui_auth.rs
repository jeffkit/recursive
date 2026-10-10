//! Issue #152: AG-UI thread ownership (feature = "http").
//!
//! `/sessions` gained an ownership model in #85, but the AG-UI channel kept
//! accepting any credential and deriving a thread's directory from a
//! client-chosen id — so one tenant could resume, read or cancel another
//! tenant's thread by naming it. These tests drive the real router with two
//! callers to pin the boundary: a foreign identity is refused (and the
//! owner's transcript is left untouched), an admin still reaches every
//! thread, and the owner recorded on disk survives a fresh server state.

#[cfg(feature = "http")]
#[path = "http_common/mod.rs"]
mod common;

#[cfg(feature = "http")]
mod agui_auth {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use recursive::http::{
        build_router, build_router_with_auth_and_rate_limit, AppState, AuthConfig, RateLimiter,
    };
    use recursive::llm::{Completion, MockProvider};
    use std::path::Path;
    use std::sync::Arc;
    use tower::ServiceExt;

    use crate::common::{sample_state_with_provider, SET_INSECURE_OK};

    /// Two tenants (alice/bob) plus an admin (root). Keys are distinct so the
    /// middleware resolves a different [`recursive::http::AuthIdentity`] per
    /// caller.
    fn two_caller_auth_with_admin() -> AuthConfig {
        AuthConfig::new(vec!["key-a".into(), "key-b".into(), "key-root".into()])
            .with_key_subject("key-a", "alice")
            .with_key_subject("key-b", "bob")
            .with_key_subject("key-root", "root")
            .with_admin("root")
    }

    /// Keep `/agui` out of the developer's real sessions store: the thread
    /// directory resolves through `paths::user_sessions_dir`, which honours
    /// this hard override. Set once per process, like the `tests/http.rs`
    /// fixture.
    fn pin_sessions_root() {
        static ROOT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        ROOT.get_or_init(|| {
            let dir: &'static tempfile::TempDir =
                Box::leak(Box::new(tempfile::tempdir().expect("agui sessions root")));
            // SAFETY: written exactly once per test process.
            unsafe { std::env::set_var("RECURSIVE_SESSIONS_DIR", dir.path()) };
        });
    }

    /// An `AppState` whose workspace is the test's own tempdir, so distinct
    /// tests never share a workspace slug (and therefore never share a
    /// thread directory). One mock reply is enough — every run here completes
    /// in a single turn.
    fn state_at(ws: &Path) -> AppState {
        SET_INSECURE_OK.call_once(|| {
            unsafe { std::env::set_var("RECURSIVE_HTTP_AUTH_INSECURE_OK", "1") };
        });
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "ok".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let mut state = sample_state_with_provider(provider);
        state.config.workspace = ws.to_path_buf();
        state
    }

    fn authed_router(state: AppState) -> axum::Router {
        build_router_with_auth_and_rate_limit(
            state,
            two_caller_auth_with_admin(),
            RateLimiter::new(100, 1.0),
        )
    }

    fn run_request(thread: &str, key: &str) -> axum::http::Request<Body> {
        let body = serde_json::json!({
            "threadId": thread,
            "runId": "r1",
            "messages": [{"id": "m1", "role": "user", "content": "hello"}],
        });
        axum::http::Request::builder()
            .method("POST")
            .uri("/agui")
            .header("content-type", "application/json")
            .header("x-api-key", key)
            .body(Body::from(body.to_string()))
            .expect("request")
    }

    fn cancel_request(thread: &str, key: &str) -> axum::http::Request<Body> {
        axum::http::Request::builder()
            .method("POST")
            .uri(format!("/agui/{thread}/cancel"))
            .header("x-api-key", key)
            .body(Body::empty())
            .expect("request")
    }

    async fn drain(resp: axum::response::Response) -> Vec<u8> {
        resp.into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes()
            .to_vec()
    }

    fn transcript_path(ws: &Path, thread: &str) -> std::path::PathBuf {
        recursive::agui_session::session_dir(ws, thread)
            .expect("session dir")
            .join("transcript.jsonl")
    }

    /// Acceptance: tenant B names tenant A's thread and is refused, and A's
    /// transcript is not appended to.
    #[tokio::test]
    async fn agui_run_rejects_other_tenant_thread() {
        pin_sessions_root();
        let ws = tempfile::tempdir().expect("workspace");
        let app = authed_router(state_at(ws.path()));

        // A creates the thread.
        let resp = app
            .clone()
            .oneshot(run_request("t-own", "key-a"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "alice's own run must succeed");
        let _ = drain(resp).await;

        let path = transcript_path(ws.path(), "t-own");
        let before = std::fs::read(&path).expect("alice's transcript exists");

        // B tries to continue it with valid B credentials.
        let resp = app
            .clone()
            .oneshot(run_request("t-own", "key-b"))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            403,
            "a foreign tenant must not resume another tenant's thread"
        );

        let after = std::fs::read(&path).expect("transcript still there");
        assert_eq!(
            before, after,
            "a refused run must not append to the owner's transcript"
        );
    }

    /// Acceptance: cancelling is refused for a foreign tenant and does not
    /// touch the owner's in-flight run; an admin still cancels.
    #[tokio::test]
    async fn agui_cancel_rejects_other_tenant() {
        pin_sessions_root();
        let ws = tempfile::tempdir().expect("workspace");
        let state = state_at(ws.path());
        // Share the cancel registry so the test can register a token the way
        // `spawn_agui_run` does, without racing a real in-flight run.
        let active = Arc::clone(&state.agui_active_runs);
        let app = authed_router(state);

        // A owns the thread (its `.meta.json` records alice).
        let resp = app
            .clone()
            .oneshot(run_request("t-cancel", "key-a"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let _ = drain(resp).await;

        let token = tokio_util::sync::CancellationToken::new();
        active.lock().unwrap_or_else(|e| e.into_inner()).insert(
            recursive::agui_session::thread_session_key("t-cancel"),
            token.clone(),
        );

        let resp = app
            .clone()
            .oneshot(cancel_request("t-cancel", "key-b"))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            403,
            "a foreign tenant must not cancel another tenant's run"
        );
        assert!(
            !token.is_cancelled(),
            "a refused cancel must leave the owner's run running"
        );

        let resp = app
            .clone()
            .oneshot(cancel_request("t-cancel", "key-root"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "an admin may cancel any thread");
        assert!(token.is_cancelled(), "the admin's cancel must take effect");
    }

    /// Acceptance: an admin identity reaches a thread it does not own,
    /// preserving the #85 `may_access_session` semantics.
    #[tokio::test]
    async fn agui_admin_can_access_any_thread() {
        pin_sessions_root();
        let ws = tempfile::tempdir().expect("workspace");
        let app = authed_router(state_at(ws.path()));

        let resp = app
            .clone()
            .oneshot(run_request("t-admin", "key-a"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let _ = drain(resp).await;

        let resp = app
            .clone()
            .oneshot(run_request("t-admin", "key-root"))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "an admin must be able to continue any thread"
        );
    }

    /// Acceptance: the ownership lives on disk, so a fresh server state over
    /// the same workspace still lets the owner in and still refuses strangers.
    #[tokio::test]
    async fn agui_ownership_survives_restart() {
        pin_sessions_root();
        let ws = tempfile::tempdir().expect("workspace");

        let first = authed_router(state_at(ws.path()));
        let resp = first
            .clone()
            .oneshot(run_request("t-restart", "key-a"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let _ = drain(resp).await;
        drop(first);

        // A brand-new AppState (fresh in-memory tables) over the same
        // workspace — nothing about the ownership is carried in memory.
        let second = authed_router(state_at(ws.path()));
        let resp = second
            .clone()
            .oneshot(run_request("t-restart", "key-a"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "the owner must still reach its thread");
        let _ = drain(resp).await;
        let resp = second
            .clone()
            .oneshot(run_request("t-restart", "key-b"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 403, "a stranger must still be refused");
    }

    /// Regression: with auth not configured (single-user deployment) the
    /// endpoint keeps working — the middleware attaches the implicit local
    /// identity and the run proceeds, exactly as before #152.
    #[tokio::test]
    async fn agui_single_user_auth_disabled_still_runs() {
        pin_sessions_root();
        let ws = tempfile::tempdir().expect("workspace");
        let app = build_router(state_at(ws.path()));

        let resp = app
            .clone()
            .oneshot(run_request("t-single", "ignored"))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "a single-user deployment must keep serving /agui"
        );
    }
}
