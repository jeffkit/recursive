//! Integration-ish tests for `AguiClient`. We run a real `wiremock`
//! HTTP server on a loopback port and drive `AguiClient` against it,
//! plus one pure helper test that bypasses HTTP to exercise the
//! chunked-parse path with deterministic split points.

use crate::{drive_stream, AguiClient, ClientError, Event, RunAgentInput};
use agui_protocol::{ContextItem, Message, Tool};
use futures_util::stream;
use serde_json::json;
use std::time::Duration;
use tokio::sync::mpsc;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Convenience: build a minimal valid `RunAgentInput`.
fn dummy_input() -> RunAgentInput {
    RunAgentInput {
        thread_id: "t".into(),
        run_id: "r".into(),
        messages: vec![Message {
            id: "m1".into(),
            role: "user".into(),
            content: Some("hello".into()),
            ..Default::default()
        }],
        tools: vec![Tool {
            name: "noop".into(),
            description: "no-op".into(),
            parameters: json!({"type":"object"}),
        }],
        context: vec![ContextItem {
            description: "cwd".into(),
            value: "/tmp".into(),
        }],
        resume: None,
        state: None,
        interrupt_before: None,
        forwarded_props: None,
    }
}

/// Drain `rx` until it closes or the deadline fires.
async fn drain(mut rx: mpsc::UnboundedReceiver<Event>) -> Vec<Event> {
    let mut out = Vec::new();
    let deadline = tokio::time::sleep(Duration::from_secs(5));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Some(e) => out.push(e),
                None => return out,
            },
            _ = &mut deadline => panic!("timed out waiting for stream close, got {} events so far", out.len()),
        }
    }
}

#[tokio::test]
async fn client_streams_events_from_mock_server() {
    let server = MockServer::start().await;
    let body = "data: {\"type\":\"RunStarted\",\"threadId\":\"t\",\"runId\":\"r\"}\n\n\
                data: {\"type\":\"RunFinished\",\"threadId\":\"t\",\"runId\":\"r\"}\n\n";
    Mock::given(method("POST"))
        .and(path("/agui"))
        .and(header("accept", "text/event-stream"))
        .and(header("content-type", "application/json"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let endpoint = format!("{}/agui", server.uri()).parse().unwrap();
    let client = AguiClient::new(endpoint);
    let rx = client.run(dummy_input()).await.expect("run ok");
    let events = drain(rx).await;

    assert_eq!(events.len(), 2, "got {events:?}");
    assert!(matches!(events[0], Event::RunStarted(_)));
    assert!(matches!(events[1], Event::RunFinished(_)));
}

#[tokio::test]
async fn client_propagates_4xx_as_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/agui"))
        .respond_with(ResponseTemplate::new(401).set_body_string("auth required"))
        .mount(&server)
        .await;

    let endpoint = format!("{}/agui", server.uri()).parse().unwrap();
    let client = AguiClient::new(endpoint);
    let err = client.run(dummy_input()).await.expect_err("should fail");
    match err {
        ClientError::HttpStatus { status, body } => {
            assert_eq!(status, 401);
            assert!(body.contains("auth required"), "body was: {body}");
        }
        other => panic!("expected HttpStatus, got {other:?}"),
    }
}

#[tokio::test]
async fn client_handles_partial_chunks_across_reads() {
    // wiremock buffers responses, so we can't easily get true-chunked
    // wire delivery from it. The goal of *this* test is the parser
    // stitching, not the HTTP framing — so we feed `drive_stream`
    // directly with a hand-rolled stream that splits the body at
    // adversarial boundaries (mid-frame, mid-line, between `\n`s).
    let frame_a = "data: {\"type\":\"RunStarted\",\"threadId\":\"t\",\"runId\":\"r\"}\n\n";
    let frame_b = "data: {\"type\":\"RunFinished\",\"threadId\":\"t\",\"runId\":\"r\"}\n\n";
    let combined: Vec<u8> = frame_a.bytes().chain(frame_b.bytes()).collect();

    // 1-byte chunks, the most adversarial possible split.
    let chunks: Vec<Result<Vec<u8>, std::io::Error>> =
        combined.iter().map(|b| Ok(vec![*b])).collect();
    let s = stream::iter(chunks);

    let (tx, mut rx) = mpsc::unbounded_channel();
    drive_stream(s, tx).await;

    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    assert_eq!(events.len(), 2, "got {events:?}");
    assert!(matches!(events[0], Event::RunStarted(_)));
    assert!(matches!(events[1], Event::RunFinished(_)));
}

#[tokio::test]
async fn client_with_invalid_header_returns_error() {
    let endpoint: url::Url = "http://example.invalid/agui".parse().unwrap();
    let client = AguiClient::new(endpoint);

    // Header *name* with illegal char (space).
    let err = client
        .clone()
        .with_header("bad name", "value")
        .expect_err("space in header name is invalid");
    assert!(matches!(err, ClientError::InvalidHeader(_)));

    // Header *value* with illegal char (newline).
    let err = client
        .with_header("x-good", "bad\nvalue")
        .expect_err("newline in header value is invalid");
    assert!(matches!(err, ClientError::InvalidHeader(_)));
}

#[tokio::test]
async fn client_post_body_serialises_run_agent_input() {
    let server = MockServer::start().await;
    let expected_body = serde_json::to_value(dummy_input()).unwrap();

    Mock::given(method("POST"))
        .and(path("/agui"))
        .and(wiremock::matchers::body_json(expected_body))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "data: {\"type\":\"RunFinished\",\"threadId\":\"t\",\"runId\":\"r\"}\n\n",
                ),
        )
        .mount(&server)
        .await;

    let endpoint = format!("{}/agui", server.uri()).parse().unwrap();
    let client = AguiClient::new(endpoint);
    let rx = client.run(dummy_input()).await.expect("run ok");
    let events = drain(rx).await;
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], Event::RunFinished(_)));

    // If the body matcher missed, wiremock would 404 the request and
    // `run` would return `HttpStatus { status: 404, .. }`. The fact
    // that we got a 200 + the event proves the JSON body matched
    // exactly — i.e. it round-trips back into our `RunAgentInput`.
    // Belt-and-braces: parse one of the recorded requests and assert.
    let received = server.received_requests().await.expect("recorded");
    assert_eq!(received.len(), 1);
    let parsed: RunAgentInput = serde_json::from_slice(&received[0].body).expect("parse body");
    assert_eq!(parsed.run_id, "r");
    assert_eq!(parsed.thread_id, "t");
}

#[tokio::test]
async fn client_endpoint_returns_url() {
    let endpoint: url::Url = "http://example.com/agui".parse().unwrap();
    let client = AguiClient::new(endpoint.clone());
    assert_eq!(client.endpoint(), &endpoint);
}
