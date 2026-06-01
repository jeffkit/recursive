//! Integration tests for Goal 157 — compact_boundary markers + session meta.
//!
//! Verifies that:
//! - After compaction, JSONL contains a compact_boundary system entry.
//! - `SessionReader::load_transcript` with default behavior returns only
//!   post-boundary messages.
//! - A session's `.meta.json` contains `first_prompt` and `last_prompt`
//!   after at least one user turn.
//! - These fields survive even if the session crashes before `finish()`.
//! - Old JSONL files without compact_boundary entries load all messages.

use recursive::event::{CompositeSink, EventSink, NullSink};
use recursive::llm::MockProvider;
use recursive::message::Message;
use recursive::session::SessionPersistenceSink;
use recursive::session::{SessionReader, SessionWriter};
use recursive::test_util::IsolatedWorkspace;
use recursive::{AgentRuntime, Compactor};
use std::sync::{Arc, Mutex};

fn simple_completion(text: &str) -> recursive::llm::Completion {
    recursive::llm::Completion {
        content: text.to_string(),
        tool_calls: vec![],
        finish_reason: Some("stop".to_string()),
        usage: None,
        reasoning_content: None,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 1: compact_boundary in JSONL after compaction
// ─────────────────────────────────────────────────────────────────────────────

/// After cross-turn compaction fires, the JSONL contains a system entry with
/// `"type":"system","subtype":"compact_boundary"`.
#[tokio::test]
async fn compact_boundary_written_to_jsonl() {
    let ws = IsolatedWorkspace::new();
    let sw = Arc::new(Mutex::new(
        SessionWriter::create(ws.path(), "g157 test", "m", "p").unwrap(),
    ));
    let dir = sw.lock().unwrap().session_dir().to_path_buf();

    let llm = Arc::new(MockProvider::new(vec![
        simple_completion("reply1"),
        simple_completion("reply2"),
        simple_completion("compact summary"),
        simple_completion("reply3"),
    ]));

    let sink = Arc::new(CompositeSink::new(vec![
        Box::new(NullSink) as Box<dyn EventSink>,
        Box::new(SessionPersistenceSink::new(sw.clone())) as Box<dyn EventSink>,
    ]));

    let compactor = Compactor::new(1).keep_recent_n(2);
    let mut rt = AgentRuntime::builder()
        .llm(llm)
        .event_sink(sink)
        .compactor(compactor)
        .build()
        .unwrap();

    rt.run("turn1").await.unwrap();
    rt.run("turn2").await.unwrap();
    rt.run("turn3").await.unwrap();
    drop(rt);
    sw.lock().unwrap().finish("done").ok();

    // Read the raw JSONL and look for the compact_boundary entry.
    let raw = std::fs::read_to_string(dir.join("transcript.jsonl")).unwrap();
    let has_boundary = raw
        .lines()
        .any(|line| line.contains("\"compact_boundary\"") && line.contains("\"type\":\"system\""));
    assert!(
        has_boundary,
        "compact_boundary entry not found in JSONL; file:\n{raw}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 2: load_transcript skips pre-boundary messages
// ─────────────────────────────────────────────────────────────────────────────

/// `SessionReader::load_transcript` discards entries before the last
/// compact_boundary, returning only post-boundary messages.
#[tokio::test]
async fn load_transcript_skips_pre_boundary_messages() {
    let ws = IsolatedWorkspace::new();
    let sw = Arc::new(Mutex::new(
        SessionWriter::create(ws.path(), "g157 boundary", "m", "p").unwrap(),
    ));
    let dir = sw.lock().unwrap().session_dir().to_path_buf();

    let llm = Arc::new(MockProvider::new(vec![
        simple_completion("reply1"),
        simple_completion("reply2"),
        simple_completion("compact summary"),
        simple_completion("reply3"),
    ]));

    let sink = Arc::new(CompositeSink::new(vec![
        Box::new(NullSink) as Box<dyn EventSink>,
        Box::new(SessionPersistenceSink::new(sw.clone())) as Box<dyn EventSink>,
    ]));

    let compactor = Compactor::new(1).keep_recent_n(2);
    let mut rt = AgentRuntime::builder()
        .llm(llm)
        .event_sink(sink)
        .compactor(compactor)
        .build()
        .unwrap();

    rt.run("turn1").await.unwrap();
    rt.run("turn2").await.unwrap();
    rt.run("turn3").await.unwrap();
    drop(rt);
    sw.lock().unwrap().finish("done").ok();

    let entries = SessionReader::load_transcript(&dir).unwrap();

    // Pre-compaction entries (turn1/reply1/turn2/reply2) should be gone.
    let has_reply1 = entries.iter().any(|e| e.content == "reply1");
    assert!(
        !has_reply1,
        "reply1 should be before the boundary and thus skipped"
    );

    // The compaction summary and post-boundary content should be present.
    let has_summary = entries
        .iter()
        .any(|e| e.content.contains("compact summary"));
    assert!(
        has_summary,
        "compaction summary should be after boundary; entries: {entries:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 3: first_prompt and last_prompt in .meta.json
// ─────────────────────────────────────────────────────────────────────────────

/// After user turns, `.meta.json` contains `first_prompt` (first user message)
/// and `last_prompt` (most recent user message).
#[test]
fn first_and_last_prompt_written_to_meta() {
    let ws = IsolatedWorkspace::new();
    let mut w = SessionWriter::create(ws.path(), "prompt meta", "m", "p").unwrap();
    let dir = w.session_dir().to_path_buf();

    w.append(&Message::user("first question"), None, None)
        .unwrap();
    w.append(&Message::assistant("first answer"), None, None)
        .unwrap();
    w.append(&Message::user("second question"), None, None)
        .unwrap();
    w.append(&Message::assistant("second answer"), None, None)
        .unwrap();
    w.finish("done").unwrap();
    drop(w);

    let meta = SessionReader::load_meta(&dir).unwrap();
    assert_eq!(
        meta.first_prompt.as_deref(),
        Some("first question"),
        "first_prompt should be the first user message"
    );
    assert_eq!(
        meta.last_prompt.as_deref(),
        Some("second question"),
        "last_prompt should be the most recent user message"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 4: first/last prompt survive crash (written on bump, not just finish)
// ─────────────────────────────────────────────────────────────────────────────

/// `first_prompt` / `last_prompt` are written to `.meta.json` on every user
/// message append (via `bump_updated_at`), so they survive a crash.
#[test]
fn first_last_prompt_survive_crash_before_finish() {
    let ws = IsolatedWorkspace::new();
    let mut w = SessionWriter::create(ws.path(), "crash prompt", "m", "p").unwrap();
    let dir = w.session_dir().to_path_buf();

    w.append(&Message::user("only question"), None, None)
        .unwrap();
    // Simulate crash: drop WITHOUT calling finish().
    drop(w);

    let meta = SessionReader::load_meta(&dir).unwrap();
    assert_eq!(
        meta.first_prompt.as_deref(),
        Some("only question"),
        "first_prompt should be written even if finish() is never called"
    );
    assert_eq!(
        meta.last_prompt.as_deref(),
        Some("only question"),
        "last_prompt should be written even if finish() is never called"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 5: old JSONL without compact_boundary loads all messages
// ─────────────────────────────────────────────────────────────────────────────

/// A JSONL file with no compact_boundary entry (pre-g157) loads all messages.
#[test]
fn no_boundary_loads_all_messages() {
    let ws = IsolatedWorkspace::new();
    let mut w = SessionWriter::create(ws.path(), "no boundary", "m", "p").unwrap();
    let dir = w.session_dir().to_path_buf();

    // Append messages normally (no compaction event).
    w.append(&Message::user("q1"), None, None).unwrap();
    w.append(&Message::assistant("a1"), None, None).unwrap();
    w.append(&Message::user("q2"), None, None).unwrap();
    w.finish("done").unwrap();
    drop(w);

    let entries = SessionReader::load_transcript(&dir).unwrap();
    assert_eq!(
        entries.len(),
        3,
        "all 3 messages should load when no boundary is present"
    );
}
