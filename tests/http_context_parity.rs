//! Goal-393 integration test: HTTP sessions get the same cross-turn context
//! management as the CLI (compactor / microcompactor / transcript cap).
//!
//! This lives in its own test **binary** on purpose: the context-management
//! assembly reads `RECURSIVE_COMPACT_THRESHOLD` / `RECURSIVE_MICROCOMPACT_*`
//! at runtime-build time, which is process-global state. Sharing a process
//! with the ~100 other `tests/http.rs` tests would leak a tiny compaction
//! threshold into every fixture runtime there, so this binary runs solo and
//! keeps its env manipulation inside one serialised test.

#![cfg(feature = "http")]

#[path = "http_common/mod.rs"]
mod common;

use common::{sample_state_with_provider, SET_INSECURE_OK};
use http_body_util::BodyExt;
use recursive::http::build_router;
use recursive::llm::{Completion, MockProvider};
use recursive::test_util::env_lock;
use std::sync::Arc;
use tower::ServiceExt;

fn completion(content: &str) -> Completion {
    Completion {
        content: content.to_string(),
        tool_calls: vec![],
        finish_reason: Some("stop".to_string()),
        usage: None,
        reasoning_content: None,
    }
}

/// Drive an HTTP session across the compaction threshold and verify it
/// compacts cross-turn and keeps conversing — the behaviour a CLI session
/// always had and an HTTP session lacked before Goal 393.
///
/// Single merged test: the env vars below are process-global, so the whole
/// scenario (set → run → assert → restore) must stay in one body. The env
/// lock is taken only inside the sync set/restore blocks — clippy forbids
/// holding a std MutexGuard across `.await`, and this binary runs exactly
/// one test, so no sibling can mutate the vars mid-run anyway.
#[tokio::test]
async fn http_session_compacts_cross_turn_and_continues() {
    let saved: Vec<(&str, Option<String>)>;
    {
        let _guard = env_lock();
        let names = [
            "RECURSIVE_COMPACT_THRESHOLD",
            "RECURSIVE_MICROCOMPACT_TRIGGER",
            "RECURSIVE_MICROCOMPACT_KEEP",
            "RECURSIVE_MAX_TRANSCRIPT_CHARS",
        ];
        saved = names.iter().map(|&n| (n, std::env::var(n).ok())).collect();

        // Tiny char threshold so cross-turn compaction fires as soon as the
        // transcript is long enough to split (Compactor::keep_recent_n = 8).
        // Microcompactor off so the only transcript mutation is the summary.
        unsafe { std::env::set_var("RECURSIVE_COMPACT_THRESHOLD", "10") };
        unsafe { std::env::set_var("RECURSIVE_MICROCOMPACT_TRIGGER", "0") };
        unsafe { std::env::remove_var("RECURSIVE_MICROCOMPACT_KEEP") };
        unsafe { std::env::remove_var("RECURSIVE_MAX_TRANSCRIPT_CHARS") };
    }

    let marker_a = "GOAL393-SUMMARY-A";
    let marker_b = "GOAL393-SUMMARY-B";
    // Turn completions t1..t6 interleave with the two summarisation calls:
    // c1..c5 = turn replies, c6 = summary A (after t5), c7 = reply,
    // c8 = summary B (after t6). Spares stay in the queue unused.
    let scripted = vec![
        completion("reply one"),
        completion("reply two"),
        completion("reply three"),
        completion("reply four"),
        completion("reply five"),
        completion(marker_a),
        completion("reply six"),
        completion(marker_b),
        completion("spare reply"),
        completion("spare summary"),
    ];
    let provider = Arc::new(MockProvider::new(scripted));
    SET_INSECURE_OK.call_once(|| unsafe {
        std::env::set_var("RECURSIVE_HTTP_AUTH_INSECURE_OK", "1");
    });
    let mut state = sample_state_with_provider(provider);
    // Headroom against the fixture's 10 req/s rate limiter: this test alone
    // issues ~8 requests back to back.
    state.rate_limiter = recursive::http::RateLimiter::new(1000, 1.0);
    let app = build_router(state);

    async fn post_json(
        app: &axum::Router,
        uri: String,
        body: String,
    ) -> axum::http::Response<axum::body::Body> {
        app.clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    // 1) Create the session — its runtime must carry the compactor.
    let response = post_json(&app, "/sessions".to_string(), "{}".to_string()).await;
    assert_eq!(response.status(), 201, "create session must succeed");
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let session_id = created["id"].as_str().unwrap().to_string();

    // 2) Five turns: the transcript grows past the split floor and the
    //    cross-turn compactor summarises the older slice after turn 5.
    for n in 1..=5usize {
        let response = post_json(
            &app,
            format!("/sessions/{session_id}/messages"),
            serde_json::json!({ "content": format!("user message number {n} with some padding text") })
                .to_string(),
        )
        .await;
        assert_eq!(
            response.status(),
            200,
            "turn {n} must succeed (compactor installed, not a context error)"
        );
    }

    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri(format!("/sessions/{session_id}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let detail: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let messages = detail["messages"].as_array().expect("messages array");
    assert!(
        !messages.is_empty(),
        "compacted transcript must lead with the summary message"
    );
    let head = messages[0]["content"].as_str().unwrap_or_default();
    assert!(
        head.contains(marker_a),
        "after turn 5 the transcript must start with the compaction summary, got: {head:?}"
    );

    // 3) Conversation continues past compaction — the pre-393 failure mode
    //    was the context error propagating and killing the session.
    let response = post_json(
        &app,
        format!("/sessions/{session_id}/messages"),
        serde_json::json!({ "content": "sixth user message, still conversing after compaction" })
            .to_string(),
    )
    .await;
    assert_eq!(
        response.status(),
        200,
        "turn 6 must succeed post-compaction"
    );

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .uri(format!("/sessions/{session_id}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let detail: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let messages = detail["messages"].as_array().expect("messages array");
    let head = messages[0]["content"].as_str().unwrap_or_default();
    assert!(
        head.contains(marker_b),
        "turn 6 must trigger a second compaction pass, transcript head: {head:?}"
    );
    assert!(
        messages.len() <= 10,
        "transcript must stay bounded by the compactor window, got {} messages",
        messages.len()
    );

    // 4) Restore the process env (sync scope again — no awaits inside).
    {
        let _guard = env_lock();
        for (name, value) in saved {
            if let Some(v) = value {
                unsafe { std::env::set_var(name, v) };
            } else {
                unsafe { std::env::remove_var(name) };
            }
        }
    }
}
