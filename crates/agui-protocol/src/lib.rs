//! AG-UI protocol types, serde definitions, and SSE parser.
//! Transport-independent — does not pull in HTTP libraries.

#![doc(html_root_url = "https://docs.rs/agui-protocol/0.1.0")]

pub mod events;
pub mod input;
pub mod sse;

pub use events::{
    BaseEvent, Custom, Event, MessagesSnapshot, Raw, RunError, RunFinished, RunStarted, StateDelta,
    StateSnapshot, StepFinished, StepStarted, TextMessageChunk, TextMessageContent, TextMessageEnd,
    TextMessageStart, ToolCallArgs, ToolCallEnd, ToolCallResult, ToolCallStart,
};
pub use input::{ContextItem, Message, Resume, RunAgentInput, Tool};
pub use sse::SseParser;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    /// Parse a JSON literal into our Event, then re-serialize, and
    /// assert the round-trip is value-equal (key order tolerant).
    fn assert_round_trip(literal: Value) -> Event {
        let ev: Event = serde_json::from_value(literal.clone())
            .unwrap_or_else(|e| panic!("deserialise failed for {literal}: {e}"));
        let back = serde_json::to_value(&ev).expect("serialise");
        assert_eq!(back, literal, "round-trip mismatch");
        ev
    }

    #[test]
    fn event_round_trip_camel_case() {
        let literal = json!({
            "type": "TextMessageContent",
            "messageId": "m1",
            "delta": "hi",
        });
        let ev = assert_round_trip(literal);
        match ev {
            Event::TextMessageContent(c) => {
                assert_eq!(c.message_id, "m1");
                assert_eq!(c.delta, "hi");
            }
            other => panic!("expected TextMessageContent, got {other:?}"),
        }
    }

    #[test]
    fn every_variant_round_trips() {
        // One literal per variant. Each variant's required fields are
        // present; optionals are omitted to keep the JSON minimal so
        // `assert_round_trip` (which compares the re-serialised output
        // to the literal) works without `null` noise.
        let cases = vec![
            json!({"type":"RunStarted","threadId":"t","runId":"r"}),
            json!({"type":"RunFinished","threadId":"t","runId":"r"}),
            json!({"type":"RunError","message":"boom"}),
            json!({"type":"StepStarted","stepName":"plan"}),
            json!({"type":"StepFinished","stepName":"plan"}),
            json!({"type":"TextMessageStart","messageId":"m"}),
            json!({"type":"TextMessageContent","messageId":"m","delta":"hi"}),
            json!({"type":"TextMessageEnd","messageId":"m"}),
            json!({"type":"TextMessageChunk","delta":"chunk"}),
            json!({"type":"ToolCallStart","toolCallId":"c","toolCallName":"shell"}),
            json!({"type":"ToolCallArgs","toolCallId":"c","delta":"{\"x\":1}"}),
            json!({"type":"ToolCallEnd","toolCallId":"c"}),
            json!({
                "type":"ToolCallResult",
                "toolCallId":"c",
                "messageId":"m",
                "content":"ok",
            }),
            json!({"type":"StateSnapshot","snapshot":{"k":"v"}}),
            json!({"type":"StateDelta","delta":[{"op":"add","path":"/k","value":"v"}]}),
            json!({"type":"MessagesSnapshot","messages":[]}),
            json!({
                "type":"Custom",
                "name":"agui-tui/permission_request",
                "value":{"tool":"shell","args":"ls"},
            }),
            json!({"type":"Raw","event":{"foo":"bar"}}),
        ];
        for literal in cases {
            assert_round_trip(literal);
        }
    }

    #[test]
    fn sse_parser_splits_events_at_blank_line() {
        let payload = b"data: {\"type\":\"RunStarted\",\"threadId\":\"t\",\"runId\":\"r\"}\n\n\
                        data: {\"type\":\"RunFinished\",\"threadId\":\"t\",\"runId\":\"r\"}\n\n";
        let mut p = SseParser::new();
        let events = p.feed(payload);
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], Event::RunStarted(_)));
        assert!(matches!(events[1], Event::RunFinished(_)));
    }

    #[test]
    fn sse_parser_handles_partial_chunks_across_reads() {
        // Same payload, but include a multi-byte UTF-8 char ("é" =
        // 0xC3 0xA9) inside one of the JSON strings, then split the
        // stream right between those two bytes plus mid-frame and
        // mid-line. We must not lose any bytes.
        let frame_a = "data: {\"type\":\"RunStarted\",\"threadId\":\"café\",\"runId\":\"r\"}\n\n";
        let frame_b = "data: {\"type\":\"RunFinished\",\"threadId\":\"t\",\"runId\":\"r\"}\n\n";
        let combined: Vec<u8> = frame_a.bytes().chain(frame_b.bytes()).collect();

        // Locate the index of the 0xA9 (continuation byte of `é`)
        // and split right before it so the first chunk ends mid-codepoint.
        let split_in_codepoint = combined.iter().position(|&b| b == 0xA9).unwrap();

        let chunk1 = &combined[..split_in_codepoint];
        let chunk2 = &combined[split_in_codepoint..split_in_codepoint + 5]; // mid-line
        let chunk3 = &combined[split_in_codepoint + 5..frame_a.len()]; // up through frame_a's `\n\n`
        let chunk4 = &combined[frame_a.len()..frame_a.len() + 10]; // mid frame_b
        let chunk5 = &combined[frame_a.len() + 10..]; // remainder

        let mut p = SseParser::new();
        let mut all = Vec::new();
        for chunk in [chunk1, chunk2, chunk3, chunk4, chunk5] {
            all.extend(p.feed(chunk));
        }
        assert_eq!(all.len(), 2, "got {all:?}");
        match &all[0] {
            Event::RunStarted(rs) => assert_eq!(rs.thread_id, "café"),
            other => panic!("first event wrong: {other:?}"),
        }
        assert!(matches!(all[1], Event::RunFinished(_)));
    }

    #[test]
    fn sse_parser_skips_comments_and_keepalives() {
        // Comment-only frame, blank-only frame, then a real event.
        let payload =
            b": this is a comment\n: another\n\n\n\ndata: {\"type\":\"RunStarted\",\"threadId\":\"t\",\"runId\":\"r\"}\n\n";
        let mut p = SseParser::new();
        let events = p.feed(payload);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], Event::RunStarted(_)));
    }

    #[test]
    fn sse_parser_supports_multi_line_data() {
        // A single frame whose payload is split across two `data:` lines.
        // Per SSE, those are joined with `\n` between them, so the
        // resulting JSON parses as one Custom event with a multi-line
        // string value.
        let frame = "data: {\"type\":\"Custom\",\"name\":\"x\",\"value\":\"line1\\nline2\"}\n\n";
        // Now split the JSON across two `data:` lines (still one frame,
        // i.e. one blank-line terminator).
        let split = "data: {\"type\":\"Custom\",\"name\":\"x\",\n\
                     data: \"value\":\"line1\\nline2\"}\n\n";
        let mut p = SseParser::new();
        let events = p.feed(split.as_bytes());
        assert_eq!(events.len(), 1);
        match &events[0] {
            Event::Custom(c) => {
                assert_eq!(c.name, "x");
                assert_eq!(c.value, Value::String("line1\nline2".to_string()));
            }
            other => panic!("expected Custom, got {other:?}"),
        }
        // Sanity: the single-line equivalent parses to the same thing.
        let mut p2 = SseParser::new();
        let single = p2.feed(frame.as_bytes());
        assert_eq!(single, events);
    }

    #[test]
    fn sse_parser_recovers_from_bad_json() {
        let payload = b"data: {not valid json\n\n\
                        data: {\"type\":\"RunStarted\",\"threadId\":\"t\",\"runId\":\"r\"}\n\n";
        let mut p = SseParser::new();
        let events = p.feed(payload);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], Event::RunStarted(_)));
    }

    #[test]
    fn custom_event_preserves_unknown_fields() {
        // A Custom value with nested unknown keys must round-trip
        // verbatim, otherwise our `agui-tui/*` extensions lose data.
        let literal = json!({
            "type": "Custom",
            "name": "agui-tui/permission_request",
            "value": {
                "tool": "run_shell",
                "args": "cargo test",
                "extra": {"deeply": {"nested": [1, 2, 3]}}
            }
        });
        let ev: Event = serde_json::from_value(literal.clone()).unwrap();
        let back = serde_json::to_value(&ev).unwrap();
        assert_eq!(back, literal);
        match ev {
            Event::Custom(c) => {
                assert_eq!(c.name, "agui-tui/permission_request");
                assert_eq!(c.value["extra"]["deeply"]["nested"][2], json!(3));
            }
            other => panic!("expected Custom, got {other:?}"),
        }
    }

    #[test]
    fn run_agent_input_serializes_camel_case() {
        let input = RunAgentInput {
            thread_id: "t".into(),
            run_id: "r".into(),
            messages: vec![Message {
                id: "msg-1".into(),
                role: "user".into(),
                content: Some("hello".into()),
                ..Default::default()
            }],
            tools: vec![Tool {
                name: "read_file".into(),
                description: "Read a file".into(),
                parameters: json!({"type": "object"}),
            }],
            context: vec![ContextItem {
                description: "cwd".into(),
                value: "/tmp".into(),
            }],
            resume: None,
            state: None,
            forwarded_props: None,
        };
        let v = serde_json::to_value(&input).unwrap();
        assert_eq!(v["threadId"], "t");
        assert_eq!(v["runId"], "r");
        // Optional `None` fields must be omitted, not serialised as null.
        assert!(v.get("resume").is_none(), "resume should be omitted: {v}");
        assert!(v.get("state").is_none(), "state should be omitted: {v}");
        assert!(
            v.get("forwardedProps").is_none(),
            "forwardedProps should be omitted: {v}"
        );
        // And nested message keeps its optional fields tidy too.
        assert_eq!(v["messages"][0]["id"], "msg-1");
        assert!(v["messages"][0].get("toolCallId").is_none());

        // Round-trip: deserialising the serialised form yields the same value.
        let back: RunAgentInput = serde_json::from_value(v).unwrap();
        assert_eq!(back, input);
    }
}
