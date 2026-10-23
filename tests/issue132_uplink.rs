//! Goal 423 W2a — issue #132: the bounded-uplink contract primitive.
//!
//! Before this there was no uplink contract at all: the router's global
//! 1 MiB `DefaultBodyLimit` silently rejected oversized bodies with axum's
//! plain-text default — no `ApiError` envelope, no metric, no log line.
//! These tests pin the contract: an oversized body is a **typed 413** naming
//! `limit_bytes`, counted on `/metrics`, WARNed — and it fails *that one
//! request only*: the session, its runtime and a live SSE subscriber are
//! untouched. The budget is configurable (`RECURSIVE_UPLINK_MAX_BYTES`,
//! default = the global constant byte for byte) and genuinely applies: the
//! messages route opts out of the global layer, so a raised limit raises the
//! real ceiling.
//!
//! Run with: `cargo test --test issue132_uplink`
#![cfg(feature = "http")]

#[path = "http_common/mod.rs"]
mod common;

mod issue132_uplink_tests {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use recursive::config::DEFAULT_UPLINK_MAX_BYTES;
    use recursive::http::{build_router, AppState};
    use recursive::llm::{Completion, MockProvider};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio_stream::StreamExt;
    use tower::ServiceExt;

    use crate::common::sample_state_with_provider;

    fn state_answering(text: &str, turns: usize) -> AppState {
        sample_state_with_provider(Arc::new(MockProvider::new(
            (0..turns)
                .map(|_| Completion {
                    content: text.into(),
                    tool_calls: vec![],
                    finish_reason: Some("stop".into()),
                    usage: None,
                    reasoning_content: None,
                })
                .collect(),
        )))
    }

    fn post_message(uri: &str, body: String) -> axum::http::Request<Body> {
        axum::http::Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    }

    async fn create_session(app: &axum::Router) -> String {
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 201);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        json["id"].as_str().expect("session id").to_string()
    }

    async fn send_message(
        app: &axum::Router,
        id: &str,
        body: String,
    ) -> axum::http::Response<Body> {
        app.clone()
            .oneshot(post_message(&format!("/sessions/{id}/messages"), body))
            .await
            .unwrap()
    }

    /// `{"content":"…"}` with exactly `total` bytes on the wire.
    fn message_body_of_total_bytes(total: usize) -> String {
        // `{"content":""}` is 14 bytes; pad the content to fill the budget.
        assert!(total > 14, "a message body must have room for content");
        format!(r#"{{"content":"{}"}}"#, "a".repeat(total - 14))
    }

    async fn get_session(app: &axum::Router, id: &str) -> axum::http::StatusCode {
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(format!("/sessions/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        response.status()
    }

    async fn subscribe(app: &axum::Router, id: &str) -> axum::response::Response {
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(format!("/sessions/{id}/events"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "SSE subscribe must succeed");
        response
    }

    /// Read from an open SSE body until it goes quiet.
    async fn read_quiet(chunks: &mut axum::body::BodyDataStream) -> String {
        let mut text = String::new();
        while let Ok(Some(Ok(chunk))) =
            tokio::time::timeout(Duration::from_millis(300), chunks.next()).await
        {
            text.push_str(&String::from_utf8_lossy(&chunk));
        }
        text
    }

    async fn metrics_text(app: &axum::Router) -> String {
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8_lossy(&body).into_owned()
    }

    // ── headline: typed 413, nothing else disturbed ─────────────────────

    /// A body over the budget is a typed 413 envelope naming `limit_bytes`
    /// (+1 on the /metrics counter) — and it fails that request alone: the
    /// session still answers GET, the live SSE subscription survives, and a
    /// legal message goes through right after.
    #[tokio::test]
    async fn messages_uplink_overflow_is_typed_413_and_leaves_session_intact() {
        let mut state = state_answering("all done", 2);
        state.config.uplink_max_bytes = 64;
        // Turn off the default 10-request burst limiter: this suite hammers.
        state.rate_limiter = recursive::http::RateLimiter::new(100_000, 100_000.0);
        let app = build_router(state.clone());
        let id = create_session(&app).await;

        // A subscriber is live *before* the overflow.
        let a = subscribe(&app, &id).await;
        let mut a_chunks = a.into_body().into_data_stream();

        // One byte over the budget.
        let response = send_message(&app, &id, "x".repeat(65)).await;
        assert_eq!(response.status(), 413, "the overflow must be a 413");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8_lossy(&body);
        let envelope: serde_json::Value = serde_json::from_str(&body)
            .expect("the overflow must be the ApiError JSON envelope, not plain text");
        assert!(
            envelope["error"]
                .as_str()
                .expect("error message")
                .contains("limit_bytes=64"),
            "the message must name limit_bytes: {body}"
        );

        // …and counted.
        let metrics = metrics_text(&app).await;
        assert!(
            metrics.contains("recursive_uplink_overflows 1\n"),
            "{metrics}"
        );

        // The session is untouched: GET works…
        assert_eq!(get_session(&app, &id).await, 200);
        // …a legal message goes through…
        let response = send_message(&app, &id, r#"{"content":"hi"}"#.into()).await;
        assert_eq!(response.status(), 200, "a legal message must succeed");
        // …and the pre-existing SSE subscription is alive to see its frames.
        let frames = read_quiet(&mut a_chunks).await;
        assert!(
            frames.contains("all done"),
            "the live subscriber must see the turn's frames: {frames}"
        );
    }

    // ── headline: configurable, 1 MiB default byte for byte ────────────

    /// The default is exactly the global body-limit constant: a body at the
    /// limit passes, the limit's first excess byte is refused with the typed
    /// 413. Raised budgets genuinely apply — the messages route is exempt
    /// from the global layer, so a 1.5 MiB body rides through a 2 MiB budget
    /// that the (still-present) global 1 MiB layer would have rejected.
    #[tokio::test]
    async fn uplink_limit_is_configurable_with_1mib_default() {
        // Default: the fixture config carries the constant, byte for byte.
        let state = state_answering("all done", 2);
        assert_eq!(state.config.uplink_max_bytes, DEFAULT_UPLINK_MAX_BYTES);
        assert_eq!(DEFAULT_UPLINK_MAX_BYTES, 1024 * 1024);
        let app = build_router(state.clone());
        let id = create_session(&app).await;

        // Exactly 1 MiB on the wire: accepted.
        let response = send_message(&app, &id, message_body_of_total_bytes(1024 * 1024)).await;
        let status = response.status();
        let response_body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            status,
            200,
            "a body exactly at the limit must pass: {}",
            String::from_utf8_lossy(&response_body)
        );

        // One byte over: the typed 413, naming the default limit.
        let response = send_message(&app, &id, message_body_of_total_bytes(1024 * 1024 + 1)).await;
        assert_eq!(response.status(), 413);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8_lossy(&bytes);
        assert!(body.contains("limit_bytes=1048576"), "{body}");

        // Raised budget: the same route now takes a 1.5 MiB body — the
        // global 1 MiB `DefaultBodyLimit` must NOT be the effective gate
        // (that is what the route-level disable is for).
        let mut raised = state_answering("all done", 1);
        raised.config.uplink_max_bytes = 2 * 1024 * 1024;
        raised.rate_limiter = recursive::http::RateLimiter::new(100_000, 100_000.0);
        let raised_app = build_router(raised.clone());
        let raised_id = create_session(&raised_app).await;
        let response = send_message(
            &raised_app,
            &raised_id,
            message_body_of_total_bytes(3 * 1024 * 1024 / 2),
        )
        .await;
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            status,
            200,
            "1.5 MiB must pass a 2 MiB budget: {}",
            String::from_utf8_lossy(&bytes)
        );

        // …while its own budget still bites at 2 MiB + 1.
        let response = send_message(
            &raised_app,
            &raised_id,
            message_body_of_total_bytes(2 * 1024 * 1024 + 1),
        )
        .await;
        assert_eq!(response.status(), 413);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8_lossy(&bytes);
        assert!(body.contains("limit_bytes=2097152"), "{body}");
    }
}
