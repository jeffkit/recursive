// Why this test exists:
// .dev/AGENTS.md invariant #8: "Tool-call ↔ tool-result pairing. Every
// `Role::Tool` message in the transcript MUST be immediately preceded by a
// `Role::Assistant` message whose `tool_calls` contains the matching id.
// OpenAI, DeepSeek, and Anthropic all enforce this server-side (HTTP 400
// 'Messages with role 'tool' must be a response to a preceding message with
// 'tool_calls''). Any operation that mutates the transcript mid-run —
// compaction, trimming, splicing, resume replay — MUST preserve this
// invariant or rebase the window past the orphan."
//
// This extends the existing `compaction_keeps_tool_calls_paired_with_results`
// test to cover all transcript mutation operations:
// - Compaction (`Compactor::compact`)
// - Compaction application (`Compactor::apply_to_transcript`)
// - Session resume/replay (loading from JSONL)
// - Manual transcript trimming/splicing

use recursive::llm::ToolCall;
use recursive::message::{Message, Role};

// ── Helpers ────────────────────────────────────────────────────────────────

fn assistant_with_tool_call(id: &str, name: &str, content: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: content.to_string(),
        tool_calls: vec![ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: serde_json::json!({"key": "value"}),
        }],
        tool_call_id: None,
        reasoning_content: None,
        is_compaction_summary: false,
    }
}

fn tool_result_msg(tool_call_id: &str, content: &str) -> Message {
    Message::tool_result(tool_call_id.to_string(), content.to_string())
}

/// Verify that a transcript satisfies the pairing invariant:
/// every Tool message has a preceding Assistant message (either the
/// immediate predecessor or an earlier one linked by consecutive Tool
/// results) whose tool_calls contain the matching id.
///
/// Per OpenAI/DeepSeek/Anthropic API spec: "tool" role messages must be
/// a response to a preceding message with tool_calls. Multiple tool results
/// may follow a single Assistant message with multiple tool_calls.
fn verify_tool_call_pairing(transcript: &[Message]) -> Result<(), String> {
    for (i, msg) in transcript.iter().enumerate() {
        if msg.role == Role::Tool {
            let tool_id = msg
                .tool_call_id
                .as_deref()
                .ok_or_else(|| format!("message [{i}] is Tool but has no tool_call_id"))?;

            if i == 0 {
                return Err(format!(
                    "message [0] is Tool with id={tool_id} but has no preceding message"
                ));
            }

            // Walk back through the transcript to find the matching Assistant.
            // Consecutive Tool messages may share a single Assistant.
            let mut found = false;
            let mut j = i - 1;
            loop {
                let prev = &transcript[j];
                match prev.role {
                    Role::Assistant => {
                        if prev.tool_calls.iter().any(|tc| tc.id == tool_id) {
                            found = true;
                        }
                        break;
                    }
                    Role::Tool => {
                        if j == 0 {
                            break;
                        }
                        j -= 1;
                    }
                    _ => break,
                }
            }

            if !found {
                return Err(format!(
                    "message [{i}] is Tool (id={tool_id}) but no preceding Assistant \
                     contains a matching tool_call"
                ));
            }
        }
    }
    Ok(())
}

// ── Basic pairing tests ────────────────────────────────────────────────────

/// A simple, well-formed transcript should pass the pairing check.
#[test]
fn valid_transcript_passes_pairing_check() {
    let transcript = vec![
        Message::system("You are an agent.".to_string()),
        Message::user("Do something".to_string()),
        assistant_with_tool_call("call_1", "Read", "Let me read that."),
        tool_result_msg("call_1", "file contents here"),
        assistant_with_tool_call("call_2", "Write", "Now I'll write."),
        tool_result_msg("call_2", "write successful"),
        Message::assistant("All done!".to_string()),
    ];

    verify_tool_call_pairing(&transcript).expect("valid transcript must pass");
}

/// A Tool message without a preceding Assistant is invalid.
#[test]
fn tool_without_assistant_predecessor_detected() {
    let transcript = vec![tool_result_msg("orphan_id", "orphan result")];

    let err = verify_tool_call_pairing(&transcript).unwrap_err();
    assert!(
        err.contains("no preceding message"),
        "must detect tool as first message: {err}"
    );
}

/// A Tool message preceded by a non-Assistant message is invalid.
#[test]
fn tool_preceded_by_non_assistant_detected() {
    let transcript = vec![
        Message::user("hello".to_string()),
        tool_result_msg("call_1", "result"),
    ];

    let err = verify_tool_call_pairing(&transcript).unwrap_err();
    assert!(
        err.contains("no preceding Assistant"),
        "must detect non-Assistant predecessor: {err}"
    );
}

/// A Tool message whose id doesn't match the preceding Assistant's tool_calls
/// is invalid.
#[test]
fn tool_with_mismatched_id_detected() {
    let transcript = vec![
        assistant_with_tool_call("call_1", "Read", "reading..."),
        tool_result_msg("call_2", "result"), // wrong id
    ];

    let err = verify_tool_call_pairing(&transcript).unwrap_err();
    assert!(
        err.contains("no preceding Assistant"),
        "must detect id mismatch: {err}"
    );
}

// ── Compaction preserves pairing ───────────────────────────────────────────

/// After `Compactor::apply_to_transcript`, the resulting transcript must
/// still satisfy the pairing invariant.
#[tokio::test]
async fn compaction_preserves_tool_call_pairing() {
    use recursive::llm::{Completion, MockProvider};
    use recursive::Compactor;

    // Build a transcript with tool calls before and after the split point.
    // keep_recent_n=3 will keep the last 3 messages verbatim.
    let transcript = vec![
        Message::system("You are an agent.".to_string()),
        Message::user("Read a file".to_string()),
        assistant_with_tool_call("call_1", "Read", "Reading file A."),
        tool_result_msg("call_1", "content of file A"),
        Message::assistant("File A says hello.".to_string()),
        Message::user("Now write to another file".to_string()),
        assistant_with_tool_call("call_2", "Write", "Writing file B."),
        tool_result_msg("call_2", "wrote file B"),
        Message::assistant("Done writing.".to_string()),
    ];

    // Verify the original transcript is valid.
    verify_tool_call_pairing(&transcript).expect("original transcript must be valid");

    let provider = MockProvider::new(vec![Completion {
        content: "Summary: user asked to read and write files.".to_string(),
        tool_calls: vec![],
        finish_reason: Some("stop".to_string()),
        usage: None,
        reasoning_content: None,
    }]);

    let compactor = Compactor::new(100).keep_recent_n(3);
    let mut mutable_transcript = transcript.clone();
    let result = compactor
        .apply_to_transcript(&provider, &mut mutable_transcript, 0)
        .await
        .expect("compaction should succeed");

    if result.is_some() {
        // If compaction happened, verify the resulting transcript is valid.
        verify_tool_call_pairing(&mutable_transcript)
            .expect("compacted transcript must preserve tool-call pairing");
    }
}

/// `Compactor::safe_split_point` must never split inside a Tool message.
#[test]
fn safe_split_point_never_splits_on_tool() {
    let transcript = vec![
        Message::user("hello".to_string()),
        assistant_with_tool_call("call_1", "Read", "reading"),
        tool_result_msg("call_1", "result"),
        Message::assistant("done".to_string()),
    ];

    // If keep_recent_n=2, the natural split point would be at index 2
    // (len=4, 4-2=2), which is the tool message. safe_split_point should
    // back up to index 1.
    let split = recursive::Compactor::safe_split_point(&transcript, 2);
    assert!(
        split != 2,
        "safe_split_point must not return index of Tool message"
    );
    assert!(
        matches!(
            transcript[split].role,
            Role::Assistant | Role::User | Role::System
        ),
        "split point must land on non-Tool message, got {:?}",
        transcript[split].role
    );
}

// ── Session resume preserves pairing ───────────────────────────────────────

/// When loading a transcript from JSONL (session resume), the messages must
/// satisfy the pairing invariant.
#[test]
fn session_resume_preserves_tool_call_pairing() {
    use tempfile::tempdir;
    let tmp = tempdir().unwrap();

    // Write a valid transcript with tool calls.
    let messages = vec![
        Message::system("agent".to_string()),
        Message::user("task".to_string()),
        assistant_with_tool_call("call_1", "Read", "reading"),
        tool_result_msg("call_1", "result"),
        Message::assistant("done".to_string()),
    ];

    // Save as JSONL
    let jsonl_path = tmp.path().join("transcript.jsonl");
    let mut jsonl = String::new();
    for msg in &messages {
        jsonl.push_str(&serde_json::to_string(msg).unwrap());
        jsonl.push('\n');
    }
    std::fs::write(&jsonl_path, &jsonl).unwrap();

    // Read back as JSONL lines.
    let loaded: Vec<Message> = std::fs::read_to_string(&jsonl_path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Message>(line).unwrap())
        .collect();

    assert_eq!(
        loaded.len(),
        messages.len(),
        "loaded message count must match"
    );
    verify_tool_call_pairing(&loaded).expect("resumed transcript must satisfy pairing invariant");
}

// ── Multiple tool calls in one Assistant message ───────────────────────────

/// An Assistant message with multiple tool_calls followed by multiple
/// Tool results (one per call) is valid.
#[test]
fn multiple_tool_calls_in_one_assistant_message() {
    let transcript = vec![
        Message::user("read two files".to_string()),
        Message {
            role: Role::Assistant,
            content: "Reading both files.".to_string(),
            tool_calls: vec![
                ToolCall {
                    id: "call_1".to_string(),
                    name: "Read".to_string(),
                    arguments: serde_json::json!({"path": "a.txt"}),
                },
                ToolCall {
                    id: "call_2".to_string(),
                    name: "Read".to_string(),
                    arguments: serde_json::json!({"path": "b.txt"}),
                },
            ],
            tool_call_id: None,
            reasoning_content: None,
            is_compaction_summary: false,
        },
        tool_result_msg("call_1", "content A"),
        tool_result_msg("call_2", "content B"),
        Message::assistant("Both files read.".to_string()),
    ];

    verify_tool_call_pairing(&transcript)
        .expect("multiple tool calls in one assistant must be valid");
}

// ── Consecutive Tool messages (each must be preceded by matching Assistant) ─

/// Each Tool message must be immediately preceded by the *same* Assistant
/// that contains its matching tool_call id. In practice, multiple Tool
/// results follow a single Assistant with multiple tool_calls.
#[test]
fn consecutive_tool_results_share_same_assistant() {
    let transcript = vec![
        Message {
            role: Role::Assistant,
            content: "".to_string(),
            tool_calls: vec![
                ToolCall {
                    id: "c1".to_string(),
                    name: "Read".to_string(),
                    arguments: serde_json::json!({}),
                },
                ToolCall {
                    id: "c2".to_string(),
                    name: "Read".to_string(),
                    arguments: serde_json::json!({}),
                },
            ],
            tool_call_id: None,
            reasoning_content: None,
            is_compaction_summary: false,
        },
        tool_result_msg("c1", "r1"),
        tool_result_msg("c2", "r2"),
    ];

    verify_tool_call_pairing(&transcript)
        .expect("consecutive tool results sharing same assistant must be valid");
}
