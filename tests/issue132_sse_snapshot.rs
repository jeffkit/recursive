//! Goal 422 W1 — issue #132: session stream contract, part 1.
//!
//! Issue #97 made `GET /sessions/:id/events` resumable *within one process*:
//! a cursor the in-memory ring still covers is replayed, an evicted one gets
//! an explicit `gap`. But the ring and the frame sequence were rebuilt from
//! zero by a restart or an idle eviction, so a client resuming across one got
//! neither history nor a gap — a silent empty stream, with nothing telling it
//! to reconcile. These tests pin the W1 upgrade:
//!
//! - the per-session event watermark is persisted at the end of each turn and
//!   seeds the cold-loaded session, so a *real* pre-restart cursor resolves to
//!   an explicit outcome (never a silent resume);
//! - that outcome is a `snapshot` frame — the session state as a cold load
//!   rebuilds it, with the sequence it is current up to — after which only
//!   newer frames follow (gap-free, duplicate-free);
//! - `gap` is reserved for when no snapshot can be derived (runtime busy with
//!   a turn, nothing restorable), and both outcomes are counted on `/metrics`;
//! - the snapshot sits behind the same owner gate as the frames.
//!
//! Run with: `cargo test --test issue132_sse_snapshot`
#![cfg(feature = "http")]

#[path = "http_common/mod.rs"]
mod common;

mod issue132_tests {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use recursive::http::{
        build_router, build_router_with_auth_and_rate_limit, AppState, AuthConfig, RateLimiter,
        SseEvent, SseFrame, SESSION_EVENT_LOG_CAPACITY,
    };
    use recursive::llm::{Completion, MockProvider};
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio_stream::StreamExt;
    use tower::ServiceExt;

    use crate::common::sample_state_with_provider;

    // ── fixtures ────────────────────────────────────────────────────────

    fn completion(text: &str) -> Completion {
        Completion {
            content: text.into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }
    }

    /// A state whose provider answers every turn with `text`.
    fn state_answering(text: &str, turns: usize) -> AppState {
        sample_state_with_provider(Arc::new(MockProvider::new(
            (0..turns).map(|_| completion(text)).collect(),
        )))
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

    fn json_request(method: &str, uri: &str, body: &str) -> axum::http::Request<Body> {
        axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    async fn create_session(app: &axum::Router) -> String {
        let response = app
            .clone()
            .oneshot(json_request("POST", "/sessions", "{}"))
            .await
            .unwrap();
        assert_eq!(response.status(), 201);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        json["id"].as_str().expect("session id").to_string()
    }

    /// Drive one real turn through the message endpoint: frames land in the
    /// ring, the transcript is persisted, and the watermark is written.
    async fn run_turn(app: &axum::Router, id: &str, text: &str) {
        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                &format!("/sessions/{id}/messages"),
                &format!(r#"{{"content":"{text}"}}"#),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "the turn must succeed");
    }

    async fn ring_frames(state: &AppState, id: &str) -> Vec<SseFrame> {
        let sessions = state.host.sessions();
        let guard = sessions.read().await;
        guard
            .get(id)
            .expect("session present")
            .event_log
            .drain_from(0)
            .frames
    }

    async fn ring_write_pos(state: &AppState, id: &str) -> u64 {
        let sessions = state.host.sessions();
        let guard = sessions.read().await;
        guard
            .get(id)
            .expect("session present")
            .event_log
            .replay(None)
            .next_pos
    }

    async fn event_seq(state: &AppState, id: &str) -> u64 {
        let sessions = state.host.sessions();
        let guard = sessions.read().await;
        guard
            .get(id)
            .expect("session present")
            .event_seq
            .load(Ordering::Relaxed)
    }

    /// Seed the ring directly, the way the forwarder does.
    async fn seed(state: &AppState, id: &str, frames: Vec<SseFrame>) {
        let sessions = state.host.sessions();
        let guard = sessions.read().await;
        let session = guard.get(id).expect("session present");
        for frame in frames {
            session.event_log.push(frame);
        }
    }

    /// What a restart (or the idle reaper) does to a session's in-memory
    /// state: the table entry — ring, sequence counter, runtime — and its
    /// broadcast channel are gone. Storage keeps the transcript, the
    /// metadata blob and (since goal 422) the watermark.
    async fn simulate_restart(state: &AppState, id: &str) {
        state.host.sessions().write().await.remove(id);
        state.event_channels.write().await.remove(id);
    }

    /// The first REST touch after a restart cold-loads the session.
    async fn cold_load(app: &axum::Router, id: &str) {
        let response = app
            .clone()
            .oneshot(json_request("GET", &format!("/sessions/{id}"), ""))
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "cold load must restore the session");
    }

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
        response
    }

    /// Accumulate SSE text until the stream goes quiet.
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

    async fn read_sse(app: &axum::Router, uri: &str, last_event_id: Option<&str>) -> String {
        drain(subscribe(app, uri, last_event_id).await).await
    }

    /// Publish a live frame the way the forwarder does: ring first, then the
    /// broadcast wake-up.
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

    async fn metrics_text(app: &axum::Router) -> String {
        let response = app
            .clone()
            .oneshot(json_request("GET", "/metrics", ""))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8_lossy(&body).into_owned()
    }

    /// The SSE blocks (`\n\n`-separated) of a stream body.
    fn blocks(body: &str) -> Vec<&str> {
        body.split("\n\n")
            .filter(|b| !b.trim().is_empty())
            .collect()
    }

    /// The `data:` payload of the one `snapshot` block in `body`.
    fn snapshot_payload(body: &str) -> serde_json::Value {
        let snapshots: Vec<&str> = blocks(body)
            .into_iter()
            .filter(|b| b.contains("event: snapshot"))
            .collect();
        assert_eq!(
            snapshots.len(),
            1,
            "exactly one snapshot must open the stream: {body}"
        );
        let data = snapshots[0]
            .lines()
            .find_map(|l| l.strip_prefix("data: "))
            .expect("snapshot data line");
        serde_json::from_str(data).expect("snapshot payload is JSON")
    }

    /// Every `id:` a stream body carried, in order.
    fn frame_ids(body: &str) -> Vec<String> {
        body.lines()
            .filter_map(|l| l.strip_prefix("id: "))
            .map(str::to_string)
            .collect()
    }

    // ── headline: stale cursor across a cold load ───────────────────────

    /// Acceptance #1 of issue #132 in CI form. Before goal 422 the restored
    /// ring knew nothing about the pre-restart cursor and `anything_evicted`
    /// was false, so the client got an empty stream and no hint to reconcile.
    #[tokio::test]
    async fn stale_cursor_across_cold_load_gets_snapshot_not_silent_resume() {
        let state = state_answering("all done", 1);
        let app = build_router(state.clone());
        let id = create_session(&app).await;
        run_turn(&app, &id, "hi").await;

        // The client's cursor: a frame of the turn that just ran.
        let frames = ring_frames(&state, &id).await;
        assert!(frames.len() >= 2, "a turn emits several frames");
        let stale_cursor = frames[0].id.clone();
        let watermark = event_seq(&state, &id).await;
        assert!(watermark > 0, "the turn advanced the sequence");

        simulate_restart(&state, &id).await;

        // Reconnect with the pre-restart cursor — no warm-up REST call first
        // (issue #227): the events endpoint itself cold-loads, so this
        // subscribe is the session's first touch after the restart.
        let response = subscribe(
            &app,
            &format!("/sessions/{id}/events?since={stale_cursor}"),
            None,
        )
        .await;
        assert_eq!(
            event_seq(&state, &id).await,
            watermark,
            "the subscribe's cold load must seed the sequence from the persisted watermark"
        );
        assert!(
            ring_frames(&state, &id).await.is_empty(),
            "the rebuilt ring holds nothing from before the restart"
        );
        // … and then the stream goes on: a live frame continuing the sequence
        // space must arrive on the same connection.
        publish(
            &state,
            &id,
            partial(&format!("9-1-{watermark}"), "after-restart", 0),
        )
        .await;
        let body = drain(response).await;

        assert!(
            body.contains("event: snapshot"),
            "a real pre-restart cursor must open with a snapshot, not an empty stream: {body}"
        );
        assert!(
            !body.contains("event: gap"),
            "the snapshot supersedes the gap: {body}"
        );
        let snapshot = snapshot_payload(&body);
        assert_eq!(snapshot["type"], "snapshot");
        assert_eq!(
            snapshot["up_to_seq"], watermark,
            "the snapshot is current up to the restored sequence"
        );
        let messages = snapshot["messages"].as_array().expect("messages");
        let roles: Vec<&str> = messages
            .iter()
            .map(|m| m["role"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(
            roles,
            vec!["user", "assistant"],
            "the snapshot is the cold-load state: prompt stripped, turn kept: {messages:?}"
        );
        assert!(
            body.contains("\"after-restart\""),
            "live increments must follow the snapshot seamlessly: {body}"
        );
        assert_eq!(
            body.matches("\"after-restart\"").count(),
            1,
            "and arrive exactly once: {body}"
        );
    }

    /// The headline's negative image: without a cursor the restored session
    /// behaves like any fresh subscription — nothing is replayed, nothing is
    /// announced. The snapshot is a *resume* mechanism, not a greeting.
    #[tokio::test]
    async fn restored_session_without_a_cursor_still_starts_from_now() {
        let state = state_answering("all done", 1);
        let app = build_router(state.clone());
        let id = create_session(&app).await;
        run_turn(&app, &id, "hi").await;
        simulate_restart(&state, &id).await;
        cold_load(&app, &id).await;

        let body = read_sse(&app, &format!("/sessions/{id}/events"), None).await;
        assert!(!body.contains("event: snapshot"), "{body}");
        assert!(!body.contains("event: gap"), "{body}");
    }

    // ── headline: snapshot wire format ──────────────────────────────────

    /// Same shape rules as the `gap` frame (issue #97): typed `event:`, JSON
    /// `data:`, and deliberately no `id:` — a per-connection notice that is
    /// not on the session timeline and never enters the ring.
    #[tokio::test]
    async fn snapshot_frame_is_a_connection_notice_outside_the_ring() {
        let state = state_answering("all done", 1);
        let app = build_router(state.clone());
        let id = create_session(&app).await;
        run_turn(&app, &id, "hi").await;
        let stale_cursor = ring_frames(&state, &id).await[0].id.clone();
        simulate_restart(&state, &id).await;
        cold_load(&app, &id).await;
        let write_pos_before = ring_write_pos(&state, &id).await;

        let body = read_sse(
            &app,
            &format!("/sessions/{id}/events?since={stale_cursor}"),
            None,
        )
        .await;

        let snapshot_block = blocks(&body)
            .into_iter()
            .find(|b| b.contains("event: snapshot"))
            .expect("snapshot block");
        assert!(
            !snapshot_block.lines().any(|l| l.starts_with("id:")),
            "a snapshot carries no id: — it is not a timeline frame: {snapshot_block}"
        );
        assert!(
            snapshot_block.lines().any(|l| l.starts_with("data: {")),
            "{snapshot_block}"
        );
        let payload = snapshot_payload(&body);
        assert!(payload["messages"].is_array());
        assert!(payload["up_to_seq"].is_u64());

        // The ring is untouched: the snapshot took no position and left no
        // frame behind.
        assert_eq!(
            ring_write_pos(&state, &id).await,
            write_pos_before,
            "a snapshot must not consume a ring position"
        );
        assert!(
            ring_frames(&state, &id)
                .await
                .iter()
                .all(|f| !matches!(f.event, SseEvent::Snapshot { .. })),
            "a snapshot is never stored in the replay ring"
        );
        // Nor is it broadcast: a second subscriber resuming from now sees no
        // snapshot.
        let other = read_sse(&app, &format!("/sessions/{id}/events"), None).await;
        assert!(!other.contains("event: snapshot"), "{other}");
    }

    #[test]
    fn snapshot_variant_serializes_as_a_tagged_payload() {
        let event = SseEvent::Snapshot {
            messages: vec![serde_json::json!({"role": "user", "content": "hi"})],
            up_to_seq: 7,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            json.starts_with(r#"{"type":"snapshot","messages":[{"#),
            "{json}"
        );
        assert!(json.ends_with(r#"}],"up_to_seq":7}"#), "{json}");
        let back: SseEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, event);
    }

    // ── snapshot ⊕ frames: no duplicate, no hole ────────────────────────

    /// Frames the snapshot already folded in (sequence below `up_to_seq`) are
    /// dropped; frames newer than it still follow — during the initial replay
    /// and on the live pump alike.
    #[tokio::test]
    async fn frames_below_up_to_seq_are_dropped_and_newer_ones_follow() {
        let state = state_answering("all done", 1);
        let app = build_router(state.clone());
        let id = create_session(&app).await;
        run_turn(&app, &id, "hi").await;
        let stale_cursor = ring_frames(&state, &id).await[0].id.clone();
        let watermark = event_seq(&state, &id).await;
        simulate_restart(&state, &id).await;
        cold_load(&app, &id).await;

        // After the restart the ring already holds two kinds of frame: one a
        // late forwarder could still push for a pre-watermark event (below
        // `up_to_seq`), and one from a new event (at/above it).
        seed(
            &state,
            &id,
            vec![
                partial(&format!("9-0-{}", watermark - 1), "folded-in", 0),
                partial(&format!("9-1-{watermark}"), "newer", 0),
            ],
        )
        .await;

        let response = subscribe(
            &app,
            &format!("/sessions/{id}/events?since={stale_cursor}"),
            None,
        )
        .await;
        publish(
            &state,
            &id,
            partial(&format!("9-0-{}", watermark - 1), "late-and-folded-in", 1),
        )
        .await;
        publish(
            &state,
            &id,
            partial(&format!("9-1-{}", watermark + 1), "live-newer", 1),
        )
        .await;
        let body = drain(response).await;

        assert!(body.contains("event: snapshot"), "{body}");
        assert!(
            !body.contains("\"folded-in\"") && !body.contains("\"late-and-folded-in\""),
            "frames the snapshot already covers must not be re-sent: {body}"
        );
        assert_eq!(body.matches("\"newer\"").count(), 1, "{body}");
        assert_eq!(body.matches("\"live-newer\"").count(), 1, "{body}");
        let ids = frame_ids(&body);
        assert_eq!(
            ids,
            vec![format!("9-1-{watermark}"), format!("9-1-{}", watermark + 1)],
            "only frames at or above up_to_seq are on the wire, in order"
        );
    }

    // ── gap stays for what a snapshot cannot cover ──────────────────────

    /// A session with nothing restorable (ring seeded directly, no turn ever
    /// ran) still gets the issue-#97 gap — the snapshot path adds an option,
    /// it does not remove the explicit notice.
    #[tokio::test]
    async fn gap_remains_when_nothing_restorable_backs_a_snapshot() {
        let state = state_answering("unused", 0);
        let app = build_router(state.clone());
        let id = create_session(&app).await;
        let frames: Vec<SseFrame> = (0..=SESSION_EVENT_LOG_CAPACITY)
            .map(|i| partial(&format!("1-0-{i}"), "x", i))
            .collect();
        seed(&state, &id, frames).await;

        let body = read_sse(&app, &format!("/sessions/{id}/events?since=1-0-0"), None).await;
        assert!(body.contains("event: gap"), "{body}");
        assert!(!body.contains("event: snapshot"), "{body}");
        assert!(
            body.contains("event: partial_message"),
            "retained frames still follow the gap: {body}"
        );
    }

    /// A turn holds the runtime lock for its whole duration. A reconnect must
    /// not stall behind it: the snapshot is only *tried*, and a busy runtime
    /// degrades to the explicit gap — promptly.
    #[tokio::test]
    async fn busy_runtime_falls_back_to_an_explicit_gap_without_stalling() {
        let state = state_answering("all done", 1);
        let app = build_router(state.clone());
        let id = create_session(&app).await;
        run_turn(&app, &id, "hi").await;
        let stale_cursor = ring_frames(&state, &id).await[0].id.clone();
        simulate_restart(&state, &id).await;
        cold_load(&app, &id).await;

        // Hold the runtime the way an in-flight turn does.
        let runtime = {
            let sessions = state.host.sessions();
            let guard = sessions.read().await;
            guard.get(&id).expect("session present").runtime.clone()
        };
        let _turn_in_flight = runtime.lock().await;

        let body = tokio::time::timeout(
            Duration::from_secs(5),
            read_sse(
                &app,
                &format!("/sessions/{id}/events?since={stale_cursor}"),
                None,
            ),
        )
        .await
        .expect("the subscribe must not wait for the turn to finish");
        assert!(
            body.contains("event: gap"),
            "a busy runtime must still announce the lost history: {body}"
        );
        assert!(!body.contains("event: snapshot"), "{body}");
    }

    // ── /metrics ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn snapshot_and_gap_outcomes_are_counted_on_metrics() {
        let state = state_answering("all done", 1);
        let app = build_router(state.clone());
        let before = metrics_text(&app).await;
        assert!(
            before.contains("recursive_event_stream_snapshots 0\n"),
            "{before}"
        );
        assert!(
            before.contains("recursive_event_stream_gaps 0\n"),
            "{before}"
        );

        // One snapshot: stale cursor across a cold load.
        let id = create_session(&app).await;
        run_turn(&app, &id, "hi").await;
        let stale_cursor = ring_frames(&state, &id).await[0].id.clone();
        simulate_restart(&state, &id).await;
        cold_load(&app, &id).await;
        let body = read_sse(
            &app,
            &format!("/sessions/{id}/events?since={stale_cursor}"),
            None,
        )
        .await;
        assert!(body.contains("event: snapshot"), "{body}");

        // One gap: a bare session's evicted cursor has no snapshot behind it.
        let bare = create_session(&app).await;
        let frames: Vec<SseFrame> = (0..=SESSION_EVENT_LOG_CAPACITY)
            .map(|i| partial(&format!("1-0-{i}"), "x", i))
            .collect();
        seed(&state, &bare, frames).await;
        let body = read_sse(&app, &format!("/sessions/{bare}/events?since=1-0-0"), None).await;
        assert!(body.contains("event: gap"), "{body}");

        let after = metrics_text(&app).await;
        assert!(
            after.contains("recursive_event_stream_snapshots 1\n"),
            "{after}"
        );
        assert!(after.contains("recursive_event_stream_gaps 1\n"), "{after}");
        assert!(after.contains("# TYPE recursive_event_stream_snapshots counter\n"));
        assert!(after.contains("# TYPE recursive_event_stream_gaps counter\n"));
    }

    // ── headline: capacity is configurable, default unchanged ───────────

    /// A state whose replay rings hold `capacity` frames.
    fn state_with_capacity(text: &str, turns: usize, capacity: usize) -> AppState {
        let mut state = state_answering(text, turns);
        state.config.sse_replay_capacity = capacity;
        state
    }

    /// The ring is sized from `Config::sse_replay_capacity` (env
    /// `RECURSIVE_SSE_REPLAY_CAPACITY`; the `from_env` default is pinned to the
    /// constant in `config.rs`). A server on the default retains exactly what
    /// the pre-config constant retained; a custom value is honoured by every
    /// session — fresh or cold-loaded — and the frames it drops are counted.
    #[tokio::test]
    async fn event_log_capacity_is_configurable_with_512_default() {
        // Default: the fixture config carries the constant, and a ring built
        // from it behaves exactly like the hard-coded one did — one frame
        // past the constant evicts exactly one.
        let state = state_answering("unused", 0);
        assert_eq!(state.config.sse_replay_capacity, SESSION_EVENT_LOG_CAPACITY);
        assert_eq!(SESSION_EVENT_LOG_CAPACITY, 512);
        let app = build_router(state.clone());
        let id = create_session(&app).await;
        let frames: Vec<SseFrame> = (0..=SESSION_EVENT_LOG_CAPACITY)
            .map(|i| partial(&format!("1-0-{i}"), "x", i))
            .collect();
        seed(&state, &id, frames).await;
        let retained = ring_frames(&state, &id).await;
        assert_eq!(retained.len(), SESSION_EVENT_LOG_CAPACITY);
        assert_eq!(
            retained[0].id, "1-0-1",
            "exactly the oldest frame was evicted"
        );
        assert!(metrics_text(&app)
            .await
            .contains("recursive_event_log_evictions 1\n"));

        // Custom: a 4-frame ring keeps the last 4 of 6 and reports 2 evicted.
        let state = state_with_capacity("all done", 1, 4);
        let app = build_router(state.clone());
        let id = create_session(&app).await;
        let frames: Vec<SseFrame> = (0..6)
            .map(|i| partial(&format!("1-0-{i}"), "x", i))
            .collect();
        seed(&state, &id, frames).await;
        let retained = ring_frames(&state, &id).await;
        assert_eq!(
            retained.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(),
            vec!["1-0-2", "1-0-3", "1-0-4", "1-0-5"],
            "the configured capacity, not the constant, bounds the ring"
        );
        let metrics = metrics_text(&app).await;
        assert!(
            metrics.contains("recursive_event_log_evictions 2\n"),
            "{metrics}"
        );
        assert!(metrics.contains("# TYPE recursive_event_log_evictions counter\n"));

        // … and a cold-loaded session is sized the same way.
        run_turn(&app, &id, "hi").await;
        simulate_restart(&state, &id).await;
        cold_load(&app, &id).await;
        let frames: Vec<SseFrame> = (0..6)
            .map(|i| partial(&format!("9-0-{}", 100 + i), "y", i))
            .collect();
        seed(&state, &id, frames).await;
        assert_eq!(
            ring_frames(&state, &id).await.len(),
            4,
            "a restored ring honours the configured capacity too"
        );
    }

    // ── headline: reconnect after eviction, state restored, no gaps ─────

    /// Read from an open SSE body until it goes quiet, keeping it open.
    async fn read_quiet(chunks: &mut axum::body::BodyDataStream) -> String {
        let mut text = String::new();
        while let Ok(Some(Ok(chunk))) =
            tokio::time::timeout(Duration::from_millis(300), chunks.next()).await
        {
            text.push_str(&String::from_utf8_lossy(&chunk));
        }
        text
    }

    /// Acceptance #1 of issue #132 as a controlled comparison: subscriber A
    /// stays connected across several turns and records every frame live;
    /// the ring is too small to keep them all, so A's first frame is evicted;
    /// subscriber B then resumes from that evicted cursor. B must end up with
    /// A's state — delivered as a snapshot — with no gap frame and none of
    /// A's frames repeated.
    ///
    /// A reads between turns (the turn endpoint returns only after every
    /// frame is in the ring), so with a ring at least one turn deep A is never
    /// a slow consumer — the eviction is the *history's* length, not A's lag.
    #[tokio::test]
    async fn reconnect_after_eviction_restores_state_without_gaps() {
        let capacity = 16;
        let max_turns = 8;
        let state = state_with_capacity("all done", max_turns, capacity);
        let app = build_router(state.clone());
        let id = create_session(&app).await;

        // A subscribes before the first turn.
        let a = subscribe(&app, &format!("/sessions/{id}/events"), None).await;
        let mut a_chunks = a.into_body().into_data_stream();
        let mut a_body = String::new();

        // Turns run until the ring has dropped A's first frame.
        let mut turns = 0;
        let a_first = loop {
            turns += 1;
            assert!(
                turns <= max_turns,
                "the ring never overflowed after {max_turns} turns — frames per turn \
                 shrank below what this test assumes"
            );
            run_turn(&app, &id, "hi").await;
            a_body.push_str(&read_quiet(&mut a_chunks).await);
            let a_ids = frame_ids(&a_body);
            let first = a_ids.first().expect("A saw the first turn").clone();
            let retained = ring_frames(&state, &id).await;
            assert!(
                a_ids.len() / turns <= capacity,
                "precondition: one turn must fit the ring, or A itself would lag: {a_ids:?}"
            );
            if retained.iter().all(|f| f.id != first) {
                break first;
            }
        };
        drop(a_chunks);
        let a_ids = frame_ids(&a_body);
        assert!(
            a_ids.len() > capacity,
            "the history must overflow the ring for the comparison to mean anything: {a_ids:?}"
        );
        assert!(
            !a_body.contains("event: gap"),
            "A was live throughout and must never have lagged: {a_body}"
        );
        assert_eq!(ring_frames(&state, &id).await.len(), capacity);

        // A's state after the turns, as the REST surface reports it (system
        // prompt excluded — the snapshot is the cold-load seed).
        let response = app
            .clone()
            .oneshot(json_request("GET", &format!("/sessions/{id}"), ""))
            .await
            .unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let detail: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let a_state: Vec<serde_json::Value> = detail["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .filter(|m| m["role"] != "system")
            .cloned()
            .collect();
        assert_eq!(a_state.len(), 2 * turns, "{a_state:?}");

        // B resumes from A's evicted cursor.
        let b_body = read_sse(
            &app,
            &format!("/sessions/{id}/events?since={a_first}"),
            None,
        )
        .await;

        assert!(
            !b_body.contains("event: gap"),
            "no hole: the snapshot heals the evicted cursor: {b_body}"
        );
        let snapshot = snapshot_payload(&b_body);
        assert_eq!(
            snapshot["messages"].as_array().expect("messages"),
            &a_state,
            "B's rebuilt state must equal A's"
        );
        assert_eq!(
            snapshot["up_to_seq"],
            event_seq(&state, &id).await,
            "the snapshot is current up to the turn's last event"
        );
        let b_ids = frame_ids(&b_body);
        assert!(
            b_ids.iter().all(|id| !a_ids.contains(id)),
            "no duplicate: nothing A already saw is re-sent to B: {b_ids:?}"
        );
        assert!(
            b_ids.is_empty(),
            "every frame of the turn is folded into the snapshot: {b_ids:?}"
        );

        // And the loss that made the snapshot necessary is on /metrics.
        let metrics = metrics_text(&app).await;
        assert!(
            metrics.contains("recursive_event_stream_snapshots 1\n"),
            "{metrics}"
        );
        assert!(
            metrics.contains("recursive_event_stream_gaps 0\n"),
            "{metrics}"
        );
        let evicted: u64 = metrics
            .lines()
            .find_map(|l| l.strip_prefix("recursive_event_log_evictions "))
            .and_then(|v| v.trim().parse().ok())
            .expect("eviction counter line");
        assert_eq!(
            evicted as usize,
            a_ids.len() - capacity,
            "every frame the ring dropped is counted"
        );
    }

    // ── owner gate (issue #85) ──────────────────────────────────────────

    fn api_request(method: &str, uri: &str, key: &str, body: &str) -> axum::http::Request<Body> {
        axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("X-API-Key", key)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    /// The snapshot is session state. It must sit behind exactly the gate the
    /// frames sit behind: a foreign credential is refused before any state is
    /// derived, across a restart included (ownership is persisted, #85).
    #[tokio::test]
    async fn snapshot_stays_behind_the_owner_gate() {
        let state = state_answering("all done", 1);
        let auth = AuthConfig::new(vec!["key-a".into(), "key-b".into()])
            .with_key_subject("key-a", "alice")
            .with_key_subject("key-b", "bob");
        let app = build_router_with_auth_and_rate_limit(
            state.clone(),
            auth,
            RateLimiter::new(10_000, 10_000.0),
        );

        // Alice owns the session and runs a turn.
        let response = app
            .clone()
            .oneshot(api_request("POST", "/sessions", "key-a", "{}"))
            .await
            .unwrap();
        assert_eq!(response.status(), 201);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let id = serde_json::from_slice::<serde_json::Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let response = app
            .clone()
            .oneshot(api_request(
                "POST",
                &format!("/sessions/{id}/messages"),
                "key-a",
                r#"{"content":"hi"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let stale_cursor = ring_frames(&state, &id).await[0].id.clone();

        simulate_restart(&state, &id).await;
        let response = app
            .clone()
            .oneshot(api_request("GET", &format!("/sessions/{id}"), "key-a", ""))
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "alice cold-loads her own session");

        // Bob resumes with alice's cursor: refused, and no state leaves.
        let response = app
            .clone()
            .oneshot(api_request(
                "GET",
                &format!("/sessions/{id}/events?since={stale_cursor}"),
                "key-b",
                "",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), 403, "a foreign identity must be refused");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8_lossy(&body);
        assert!(!body.contains("snapshot"), "{body}");
        assert!(
            !body.contains("all done"),
            "no session state may leak: {body}"
        );

        // Alice herself gets the snapshot.
        let response = app
            .clone()
            .oneshot(api_request(
                "GET",
                &format!("/sessions/{id}/events?since={stale_cursor}"),
                "key-a",
                "",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = drain(response).await;
        assert!(body.contains("event: snapshot"), "{body}");
        assert!(body.contains("all done"), "{body}");
    }
}
