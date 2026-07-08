//! AG-UI protocol types, serde definitions, and SSE parser.
//! Transport-independent — does not pull in HTTP libraries.

#![doc(html_root_url = "https://docs.rs/agui-protocol/0.1.0")]

pub mod events;
pub mod input;
pub mod sse;

pub use events::{
    BaseEvent, Custom, Event, Interrupt, MessagesSnapshot, Raw, RunError, RunFinished,
    RunFinishedOutcome, RunStarted, StateDelta, StateSnapshot, StepFinished, StepStarted,
    TextMessageChunk, TextMessageContent, TextMessageEnd, TextMessageStart, ToolCallArgs,
    ToolCallEnd, ToolCallResult, ToolCallStart,
};
pub use input::{ContextItem, Message, Resume, ResumeStatus, RunAgentInput, Tool};
pub use sse::SseParser;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

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
        let cases = vec![
            json!({"type":"RunStarted","threadId":"t","runId":"r"}),
            json!({"type":"RunFinished","threadId":"t","runId":"r"}),
            json!({
                "type":"RunFinished",
                "threadId":"t","runId":"r",
                "outcome":{"type":"interrupt","interrupts":[{"id":"i1","reason":"tool_call","toolCallId":"tc-1"}]},
            }),
            json!({
                "type":"RunFinished",
                "threadId":"t","runId":"r",
                "outcome":{"type":"success"},
            }),
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
            json!({"type":"ToolCallResult","toolCallId":"c","messageId":"m","content":"ok"}),
            json!({"type":"StateSnapshot","snapshot":{"k":"v"}}),
            json!({"type":"StateDelta","delta":[{"op":"add","path":"/k","value":"v"}]}),
            json!({"type":"MessagesSnapshot","messages":[]}),
            json!({"type":"Custom","name":"agui-tui/permission_request","value":{"tool":"shell","args":"ls"}}),
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
        let frame_a =
            "data: {\"type\":\"RunStarted\",\"threadId\":\"caf\u{00e9}\",\"runId\":\"r\"}\n\n";
        let frame_b = "data: {\"type\":\"RunFinished\",\"threadId\":\"t\",\"runId\":\"r\"}\n\n";
        let combined: Vec<u8> = frame_a.bytes().chain(frame_b.bytes()).collect();
        let split_in_codepoint = combined.iter().position(|&b| b == 0xA9).unwrap();
        let chunk1 = &combined[..split_in_codepoint];
        let chunk2 = &combined[split_in_codepoint..split_in_codepoint + 5];
        let chunk3 = &combined[split_in_codepoint + 5..frame_a.len()];
        let chunk4 = &combined[frame_a.len()..frame_a.len() + 10];
        let chunk5 = &combined[frame_a.len() + 10..];
        let mut p = SseParser::new();
        let mut all = Vec::new();
        for chunk in [chunk1, chunk2, chunk3, chunk4, chunk5] {
            all.extend(p.feed(chunk));
        }
        assert_eq!(all.len(), 2, "got {all:?}");
        match &all[0] {
            Event::RunStarted(rs) => assert_eq!(rs.thread_id, "caf\u{00e9}"),
            other => panic!("first event wrong: {other:?}"),
        }
        assert!(matches!(all[1], Event::RunFinished(_)));
    }

    #[test]
    fn sse_parser_skips_comments_and_keepalives() {
        let payload =
            b": this is a comment\n: another\n\n\n\ndata: {\"type\":\"RunStarted\",\"threadId\":\"t\",\"runId\":\"r\"}\n\n";
        let mut p = SseParser::new();
        let events = p.feed(payload);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], Event::RunStarted(_)));
    }

    #[test]
    fn sse_parser_supports_multi_line_data() {
        let frame = "data: {\"type\":\"Custom\",\"name\":\"x\",\"value\":\"line1\\nline2\"}\n\n";
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

    // ── Resume v2 round-trip tests ───────────────────────────────────

    #[test]
    fn resume_resolved_with_payload_round_trips() {
        let resume = Resume {
            interrupt_id: "i-1".into(),
            status: ResumeStatus::Resolved,
            payload: Some(json!({"approved": true})),
        };
        let v = serde_json::to_value(&resume).unwrap();
        assert_eq!(v["interruptId"], "i-1");
        assert_eq!(v["status"], "resolved");
        assert_eq!(v["payload"]["approved"], true);
        let back: Resume = serde_json::from_value(v).unwrap();
        assert_eq!(back.interrupt_id, "i-1");
        assert_eq!(back.status, ResumeStatus::Resolved);
        assert_eq!(back.payload, Some(json!({"approved": true})));
    }

    #[test]
    fn resume_cancelled_round_trips() {
        let resume = Resume {
            interrupt_id: "i-2".into(),
            status: ResumeStatus::Cancelled,
            payload: None,
        };
        let v = serde_json::to_value(&resume).unwrap();
        assert_eq!(v["status"], "cancelled");
        let back: Resume = serde_json::from_value(v).unwrap();
        assert_eq!(back.status, ResumeStatus::Cancelled);
        assert!(back.payload.is_none());
    }

    // ── RunFinishedOutcome round-trip tests ──────────────────────────

    #[test]
    fn run_finished_outcome_success_round_trips() {
        let ev = Event::RunFinished(RunFinished {
            thread_id: "t".into(),
            run_id: "r".into(),
            outcome: Some(RunFinishedOutcome::Success),
            result: None,
            base: BaseEvent::default(),
        });
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["outcome"]["type"], "success");
        let back: Event = serde_json::from_value(v).unwrap();
        match back {
            Event::RunFinished(rf) => {
                assert_eq!(rf.outcome, Some(RunFinishedOutcome::Success));
            }
            other => panic!("expected RunFinished, got {other:?}"),
        }
    }

    #[test]
    fn run_finished_outcome_interrupt_with_tool_call_id_round_trips() {
        let ev = Event::RunFinished(RunFinished {
            thread_id: "t".into(),
            run_id: "r".into(),
            outcome: Some(RunFinishedOutcome::Interrupt {
                interrupts: vec![Interrupt {
                    id: "i-1".into(),
                    reason: "tool_call".into(),
                    message: Some("Approve this tool call?".into()),
                    tool_call_id: Some("tc-001".into()),
                    response_schema: Some(
                        json!({"type":"object","properties":{"approved":{"type":"boolean"}}}),
                    ),
                    expires_at: Some("2026-07-08T12:00:00Z".into()),
                    metadata: Some(json!({"toolName": "Bash"})),
                }],
            }),
            result: None,
            base: BaseEvent::default(),
        });
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["outcome"]["type"], "interrupt");
        assert_eq!(v["outcome"]["interrupts"][0]["toolCallId"], "tc-001");
        let back: Event = serde_json::from_value(v).unwrap();
        match back {
            Event::RunFinished(rf) => {
                let outcome = rf.outcome.expect("outcome must be present");
                match outcome {
                    RunFinishedOutcome::Interrupt { interrupts } => {
                        assert_eq!(interrupts[0].tool_call_id.as_deref(), Some("tc-001"));
                    }
                    other => panic!("expected Interrupt, got {other:?}"),
                }
            }
            other => panic!("expected RunFinished, got {other:?}"),
        }
    }

    #[test]
    fn run_finished_with_legacy_result_still_parses() {
        let v = json!({"type":"RunFinished","threadId":"t","runId":"r","result":{"legacy":"data"}});
        let ev: Event = serde_json::from_value(v).unwrap();
        match ev {
            Event::RunFinished(rf) => {
                assert!(rf.outcome.is_none());
                assert_eq!(rf.result, Some(json!({"legacy": "data"})));
            }
            other => panic!("expected RunFinished, got {other:?}"),
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
        };
        let v = serde_json::to_value(&input).unwrap();
        assert_eq!(v["threadId"], "t");
        assert_eq!(v["runId"], "r");
        assert!(v.get("resume").is_none(), "resume should be omitted: {v}");
        assert!(
            v.get("interruptBefore").is_none(),
            "interruptBefore should be omitted: {v}"
        );
        let back: RunAgentInput = serde_json::from_value(v).unwrap();
        assert_eq!(back, input);
    }

    #[test]
    fn run_agent_input_with_resume_v2_and_interrupt_before_round_trips() {
        let input = RunAgentInput {
            thread_id: "t-1".into(),
            run_id: "r-1".into(),
            messages: vec![],
            tools: vec![],
            context: vec![],
            resume: Some(vec![
                Resume {
                    interrupt_id: "i-1".into(),
                    status: ResumeStatus::Resolved,
                    payload: Some(json!({"approved": true})),
                },
                Resume {
                    interrupt_id: "i-2".into(),
                    status: ResumeStatus::Cancelled,
                    payload: None,
                },
            ]),
            state: None,
            interrupt_before: Some(vec!["Bash".into(), "Write".into()]),
            forwarded_props: None,
        };
        let v = serde_json::to_value(&input).unwrap();
        assert_eq!(v["resume"][0]["interruptId"], "i-1");
        assert_eq!(v["resume"][0]["status"], "resolved");
        assert_eq!(v["interruptBefore"][0], "Bash");

        // Old payload without resume/interruptBefore still parses
        let old = json!({"threadId":"t-old","runId":"r-old","messages":[],"tools":[],"context":[]});
        let parsed: RunAgentInput = serde_json::from_value(old).unwrap();
        assert!(parsed.resume.is_none());
        assert!(parsed.interrupt_before.is_none());
    }
}
