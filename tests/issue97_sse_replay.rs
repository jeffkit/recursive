//! Issue #97 — resumable session SSE.
//!
//! `GET /sessions/:id/events` used to be live-only: a subscriber that fell
//! behind the broadcast channel silently lost the frames it missed, and a
//! reconnect (mobile network switch, proxy timeout, the stream's own 1 h cap)
//! restarted from the subscription instant. These tests pin the replay
//! contract: `?since=` / `Last-Event-ID` resume, a fresh subscriber is *not*
//! handed history it never asked for, and a cursor older than the retained
//! window yields an explicit `gap` frame rather than a silent hole.
//!
//! Run with: `cargo test --test issue97_sse_replay`
#![cfg(feature = "http")]

#[path = "http_common/mod.rs"]
mod common;

mod issue97_tests {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use recursive::http::{build_router, AppState, SseEvent, SseFrame, SESSION_EVENT_LOG_CAPACITY};
    use recursive::llm::{Completion, MockProvider};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio_stream::StreamExt;
    use tower::ServiceExt;

    use crate::common::sample_state_with_provider;

    fn tool_call(id: &str) -> SseFrame {
        SseFrame {
            id: id.to_string(),
            event: SseEvent::ToolCall {
                name: "Read".into(),
                step: 0,
            },
        }
    }

    fn done(id: &str) -> SseFrame {
        SseFrame {
            id: id.to_string(),
            event: SseEvent::Done {
                finish_reason: "NoMoreToolCalls".into(),
                total_steps: 1,
            },
        }
    }

    async fn create_session(app: &axum::Router) -> String {
        let app = app.clone();
        let response = app
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

    /// Seed the session's replay buffer exactly the way the turn forwarder
    /// does before it broadcasts each frame.
    async fn seed(state: &AppState, id: &str, frames: Vec<SseFrame>) {
        let sessions = state.host.sessions();
        let guard = sessions.read().await;
        let session = guard.get(id).expect("session present");
        for frame in frames {
            session.event_log.push(frame);
        }
    }

    /// Subscribe to `uri`, returning the SSE response.
    ///
    /// The response resolves as soon as the handler has subscribed and
    /// snapshotted the replay log, so a test can keep writing frames into the
    /// session afterwards to exercise the *live* pump.
    async fn subscribe(
        app: &axum::Router,
        uri: &str,
        last_event_id: Option<&str>,
    ) -> axum::response::Response {
        let mut request = axum::http::Request::builder().uri(uri);
        if let Some(id) = last_event_id {
            request = request.header("last-event-id", id);
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "SSE subscribe must succeed");
        assert!(response.headers()["content-type"]
            .to_str()
            .unwrap()
            .contains("text/event-stream"));
        response
    }

    /// Accumulate SSE frames until the stream goes quiet.
    async fn drain(response: axum::response::Response) -> String {
        let mut chunks = response.into_body().into_data_stream();
        let mut text = String::new();
        while let Ok(Some(Ok(chunk))) =
            tokio::time::timeout(Duration::from_millis(300), chunks.next()).await
        {
            text.push_str(&String::from_utf8_lossy(&chunk));
        }
        text
    }

    /// Subscribe and read in one step.
    async fn read_sse(app: &axum::Router, uri: &str, last_event_id: Option<&str>) -> String {
        drain(subscribe(app, uri, last_event_id).await).await
    }

    /// Publish a frame the way the turn forwarder does: into the replay log
    /// first, then a wake-up on the session's broadcast channel.
    async fn publish(state: &AppState, id: &str, frame: SseFrame) {
        let tx = {
            let channels = state.event_channels.read().await;
            channels.get(id).expect("session channel").clone()
        };
        let sessions = state.host.sessions();
        let guard = sessions.read().await;
        guard
            .get(id)
            .expect("session present")
            .event_log
            .push(frame.clone());
        drop(guard);
        let _ = tx.send(frame);
    }

    fn partial(id: &str, text: &str, step: usize) -> SseFrame {
        SseFrame {
            id: id.to_string(),
            event: SseEvent::PartialMessage {
                text: text.to_string(),
                step,
            },
        }
    }

    #[tokio::test]
    async fn since_cursor_replays_the_frames_it_missed() {
        let state = sample_state_with_provider(Arc::new(MockProvider::new(vec![])));
        let app = build_router(state.clone());
        let id = create_session(&app).await;
        seed(&state, &id, vec![tool_call("1-0-0"), done("1-0-1")]).await;

        let body = read_sse(&app, &format!("/sessions/{id}/events?since=1-0-0"), None).await;
        assert!(
            body.contains("event: done"),
            "the frame after the cursor must be replayed: {body}"
        );
        assert!(
            !body.contains("event: tool_call"),
            "frames the client already saw must not repeat: {body}"
        );
    }

    #[tokio::test]
    async fn last_event_id_header_resumes_like_since() {
        let state = sample_state_with_provider(Arc::new(MockProvider::new(vec![])));
        let app = build_router(state.clone());
        let id = create_session(&app).await;
        seed(&state, &id, vec![tool_call("1-0-0"), done("1-0-1")]).await;

        let body = read_sse(&app, &format!("/sessions/{id}/events"), Some("1-0-0")).await;
        assert!(body.contains("event: done"), "{body}");
        assert!(!body.contains("event: tool_call"), "{body}");
    }

    #[tokio::test]
    async fn without_a_cursor_the_stream_starts_from_now() {
        let state = sample_state_with_provider(Arc::new(MockProvider::new(vec![])));
        let app = build_router(state.clone());
        let id = create_session(&app).await;
        seed(&state, &id, vec![tool_call("1-0-0"), done("1-0-1")]).await;

        let body = read_sse(&app, &format!("/sessions/{id}/events"), None).await;
        assert!(
            !body.contains("event: done"),
            "a fresh subscriber must not be replayed history: {body}"
        );
    }

    #[tokio::test]
    async fn cursor_older_than_the_retained_window_emits_a_gap() {
        let state = sample_state_with_provider(Arc::new(MockProvider::new(vec![])));
        let app = build_router(state.clone());
        let id = create_session(&app).await;
        // One frame more than the ring holds: the first frame is evicted.
        let frames: Vec<SseFrame> = (0..=SESSION_EVENT_LOG_CAPACITY)
            .map(|i| partial(&format!("1-0-{i}"), "x", i))
            .collect();
        seed(&state, &id, frames).await;

        let body = read_sse(&app, &format!("/sessions/{id}/events?since=1-0-0"), None).await;
        assert!(
            body.contains("event: gap"),
            "an evicted cursor must be reported instead of skipped: {body}"
        );
        assert!(
            body.contains("\"resume_id\":\"1-0-1\""),
            "the gap must name where the stream resumes: {body}"
        );
        assert!(
            body.contains("event: partial_message"),
            "the retained frames still follow the gap: {body}"
        );
    }

    #[tokio::test]
    async fn an_empty_last_event_id_is_ignored() {
        let state = sample_state_with_provider(Arc::new(MockProvider::new(vec![])));
        let app = build_router(state.clone());
        let id = create_session(&app).await;
        seed(&state, &id, vec![tool_call("1-0-0"), done("1-0-1")]).await;

        // An empty header is "no cursor", not a cursor that matches nothing.
        let body = read_sse(&app, &format!("/sessions/{id}/events"), Some("")).await;
        assert!(
            !body.contains("event: done"),
            "an empty Last-Event-ID must not trigger a replay: {body}"
        );
    }

    #[tokio::test]
    async fn frames_published_after_connect_arrive_exactly_once() {
        let state = sample_state_with_provider(Arc::new(MockProvider::new(vec![])));
        let app = build_router(state.clone());
        let id = create_session(&app).await;

        // Subscribe first: everything published below is a live frame that the
        // pump has to pick up (and must not re-send on the next wake-up).
        let response = subscribe(&app, &format!("/sessions/{id}/events"), None).await;
        publish(&state, &id, partial("1-0-0", "live-a", 0)).await;
        publish(&state, &id, partial("1-0-1", "live-b", 1)).await;

        let body = drain(response).await;
        assert_eq!(
            body.matches("\"live-a\"").count(),
            1,
            "a live frame must be delivered exactly once: {body}"
        );
        assert_eq!(body.matches("\"live-b\"").count(), 1, "{body}");
    }

    #[tokio::test]
    async fn a_completed_turn_lands_in_the_replay_log() {
        let provider = Arc::new(MockProvider::new(vec![Completion {
            content: "all done".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }]));
        let state = sample_state_with_provider(provider);
        let app = build_router(state.clone());
        let id = create_session(&app).await;

        // Drive a real turn through the message endpoint: the forwarder must
        // log its frames on the way to the broadcast channel.
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/sessions/{id}/messages"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"content":"hi"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);

        let replay = {
            let sessions = state.host.sessions();
            let guard = sessions.read().await;
            guard
                .get(&id)
                .expect("session present")
                .event_log
                .drain_from(0)
        };
        assert!(
            !replay.truncated,
            "a turn this short must fit the replay ring"
        );
        assert!(
            replay
                .frames
                .iter()
                .any(|f| matches!(f.event, SseEvent::Done { .. })),
            "the turn's frames must be replayable: {:?}",
            replay.frames
        );

        // Resuming from the first frame replays the rest of the turn.
        let first = replay.frames.first().expect("frames").id.clone();
        let last = replay.frames.last().expect("frames").id.clone();
        let body = read_sse(&app, &format!("/sessions/{id}/events?since={first}"), None).await;
        assert!(
            body.contains(&format!("id: {last}\n")),
            "resume from {first} must replay {last}: {body}"
        );
    }
}
