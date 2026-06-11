//! Integration tests for Goal 156 — token usage & cost tracking.
//!
//! Verifies that:
//! - After a run, assistant messages in JSONL have non-null `usage` fields.
//! - `SessionMeta.cost` accumulates total tokens across messages.
//! - Old JSONL files without `usage` load without error.

use recursive::event::{CompositeSink, EventSink, NullSink};
use recursive::llm::{Completion, MockProvider, TokenUsage};
use recursive::session::SessionPersistenceSink;
use recursive::session::{SessionReader, SessionWriter, UsageMeta};
use recursive::test_util::IsolatedWorkspace;
use recursive::AgentRuntime;
use std::sync::{Arc, Mutex};

fn completion_with_usage(text: &str, input: u32, output: u32) -> Completion {
    Completion {
        content: text.to_string(),
        tool_calls: vec![],
        finish_reason: Some("stop".to_string()),
        usage: Some(TokenUsage {
            reasoning_tokens: 0,
            prompt_tokens: input,
            completion_tokens: output,
            total_tokens: input + output,
            cache_hit_tokens: 0,
            cache_miss_tokens: 0,
        }),
        reasoning_content: None,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 1: assistant messages have usage in JSONL
// ─────────────────────────────────────────────────────────────────────────────

/// After a run, the assistant messages in `transcript.jsonl` have a `usage`
/// field with non-zero `input_tokens` and `output_tokens`.
#[tokio::test]
async fn assistant_messages_have_usage_in_jsonl() {
    let ws = IsolatedWorkspace::new();
    let sw = Arc::new(Mutex::new(
        SessionWriter::create(ws.path(), "g156 usage", "m", "p").unwrap(),
    ));
    let dir = sw.lock().unwrap().session_dir().to_path_buf();

    let llm = Arc::new(MockProvider::with_usage(vec![
        completion_with_usage("hi", 10, 5),
        completion_with_usage("done", 20, 8),
    ]));

    let sink = Arc::new(CompositeSink::new(vec![
        Box::new(NullSink) as Box<dyn EventSink>,
        Box::new(SessionPersistenceSink::new(sw.clone())) as Box<dyn EventSink>,
    ]));

    let mut rt = AgentRuntime::builder()
        .llm(llm)
        .event_sink(sink)
        .build()
        .unwrap();

    rt.run("hello").await.unwrap();
    rt.run("bye").await.unwrap();
    drop(rt);
    sw.lock().unwrap().finish("done").ok();

    let entries = SessionReader::load_transcript(&dir).unwrap();
    let assistant_entries: Vec<_> = entries.iter().filter(|e| e.role == "assistant").collect();
    assert!(!assistant_entries.is_empty(), "no assistant entries found");

    for entry in &assistant_entries {
        let usage = entry
            .usage
            .as_ref()
            .expect("assistant entry should have usage");
        assert!(
            usage.input_tokens > 0 || usage.output_tokens > 0,
            "usage should have non-zero tokens for assistant message: {:?}",
            entry.content
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 2: cost accumulates in .meta.json after finish
// ─────────────────────────────────────────────────────────────────────────────

/// After `finish()`, `SessionMeta.cost` contains the cumulative token totals.
#[tokio::test]
async fn cost_accumulated_in_meta_after_finish() {
    let ws = IsolatedWorkspace::new();
    let sw = Arc::new(Mutex::new(
        SessionWriter::create(ws.path(), "g156 cost", "m", "p").unwrap(),
    ));
    let dir = sw.lock().unwrap().session_dir().to_path_buf();

    let llm = Arc::new(MockProvider::with_usage(vec![
        completion_with_usage("reply1", 10, 5),
        completion_with_usage("reply2", 20, 8),
    ]));

    let sink = Arc::new(CompositeSink::new(vec![
        Box::new(NullSink) as Box<dyn EventSink>,
        Box::new(SessionPersistenceSink::new(sw.clone())) as Box<dyn EventSink>,
    ]));

    let mut rt = AgentRuntime::builder()
        .llm(llm)
        .event_sink(sink)
        .build()
        .unwrap();

    rt.run("turn1").await.unwrap();
    rt.run("turn2").await.unwrap();
    drop(rt);
    sw.lock().unwrap().finish("done").unwrap();

    let meta = SessionReader::load_meta(&dir).unwrap();
    let cost = meta.cost.expect("cost should be in meta after finish");
    assert!(
        cost.total_input_tokens > 0,
        "total_input_tokens should be > 0, got {}",
        cost.total_input_tokens
    );
    assert!(
        cost.total_output_tokens > 0,
        "total_output_tokens should be > 0, got {}",
        cost.total_output_tokens
    );
    // With 2 turns at 10+20=30 input and 5+8=13 output:
    assert_eq!(
        cost.total_input_tokens, 30,
        "expected 30 total input tokens"
    );
    assert_eq!(
        cost.total_output_tokens, 13,
        "expected 13 total output tokens"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 3: old JSONL without usage loads cleanly
// ─────────────────────────────────────────────────────────────────────────────

/// JSONL entries without a `usage` field (pre-g156) load without error;
/// `entry.usage` is `None`.
#[test]
fn old_jsonl_without_usage_loads_without_error() {
    let ws = IsolatedWorkspace::new();
    let mut w = SessionWriter::create(ws.path(), "legacy", "m", "p").unwrap();
    let dir = w.session_dir().to_path_buf();
    // Write something to create the directory structure.
    w.append(&recursive::message::Message::user("x"), None, None)
        .unwrap();
    drop(w);

    // Overwrite the JSONL with a legacy entry lacking `usage`.
    let jsonl_path = dir.join("transcript.jsonl");
    let legacy = r#"{"uuid":"","id":"msg_001","role":"user","content":"hello","timestamp":"2024-01-01T00:00:00Z"}
"#;
    std::fs::write(&jsonl_path, legacy).unwrap();

    let entries = SessionReader::load_transcript(&dir).unwrap();
    assert_eq!(entries.len(), 1);
    assert!(
        entries[0].usage.is_none(),
        "missing usage field should default to None"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 4: UsageMeta::from_token_usage maps fields correctly
// ─────────────────────────────────────────────────────────────────────────────

/// `UsageMeta::from_token_usage` correctly maps `prompt_tokens` →
/// `input_tokens` and `completion_tokens` → `output_tokens`.
#[test]
fn usage_meta_from_token_usage_maps_correctly() {
    let tu = TokenUsage {
        reasoning_tokens: 0,
        prompt_tokens: 100,
        completion_tokens: 50,
        total_tokens: 150,
        cache_hit_tokens: 20,
        cache_miss_tokens: 10,
    };
    let um = UsageMeta::from_token_usage(&tu);
    assert_eq!(um.input_tokens, 100);
    assert_eq!(um.output_tokens, 50);
    assert_eq!(um.cache_read_tokens, Some(20));
    assert_eq!(um.cache_creation_tokens, Some(10));
}
