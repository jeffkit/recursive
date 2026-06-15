//! Integration tests for Goal 155 — UUID per-message chain.
//!
//! Verifies that:
//! - Every `TranscriptEntry` has a non-empty UUID v4.
//! - `parent_uuid` of each entry (except root) equals the preceding entry's UUID.
//! - `SessionReader::load_transcript_indexed` builds a UUID → entry index.
//! - Old JSONL files without `uuid` fields load without error.

use recursive::message::Message;
use recursive::session::{SessionReader, SessionStatus, SessionWriter};
use recursive::test_util::IsolatedWorkspace;
use std::sync::{Arc, Mutex};

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn make_writer(ws: &IsolatedWorkspace) -> Arc<Mutex<SessionWriter>> {
    Arc::new(Mutex::new(
        SessionWriter::create(ws.path(), "test", "gpt-4o", "openai").unwrap(),
    ))
}

fn session_dir(sw: &Arc<Mutex<SessionWriter>>) -> std::path::PathBuf {
    sw.lock().unwrap().session_dir().to_path_buf()
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 1: every entry has a uuid; parent_uuid chain is correct
// ─────────────────────────────────────────────────────────────────────────────

/// Every newly written `TranscriptEntry` has a UUID v4 (36 chars). The
/// `parent_uuid` of each non-root entry equals the UUID of the entry
/// before it in the chain.
#[test]
fn uuid_chain_is_correct() {
    let ws = IsolatedWorkspace::new();
    let sw = make_writer(&ws);
    let dir = session_dir(&sw);

    {
        let mut w = sw.lock().unwrap();
        w.append(&Message::user("hello"), None, None).unwrap();
        w.append(&Message::assistant("hi"), None, None).unwrap();
        w.append(&Message::tool_result("call_1", "result"), None, None)
            .unwrap();
        w.finish(SessionStatus::Completed).unwrap();
    }

    let entries = SessionReader::load_transcript(&dir).unwrap();
    assert_eq!(entries.len(), 3);

    // All UUIDs must be non-empty and the right length.
    for (i, e) in entries.iter().enumerate() {
        assert_eq!(
            e.uuid.len(),
            36,
            "entry {i} uuid should be 36 chars: {:?}",
            e.uuid
        );
    }

    // Root has no parent.
    assert_eq!(
        entries[0].parent_uuid, None,
        "root must have no parent_uuid"
    );

    // Each subsequent entry's parent_uuid == previous entry's uuid.
    assert_eq!(
        entries[1].parent_uuid.as_deref(),
        Some(entries[0].uuid.as_str()),
        "entry[1].parent_uuid should be entry[0].uuid"
    );
    assert_eq!(
        entries[2].parent_uuid.as_deref(),
        Some(entries[1].uuid.as_str()),
        "entry[2].parent_uuid should be entry[1].uuid"
    );

    // All UUIDs are distinct.
    let uuids: Vec<&str> = entries.iter().map(|e| e.uuid.as_str()).collect();
    let mut unique = uuids.clone();
    unique.dedup();
    assert_eq!(uuids.len(), unique.len(), "all UUIDs should be distinct");
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 2: UUID index from load_transcript_indexed
// ─────────────────────────────────────────────────────────────────────────────

/// `SessionReader::load_transcript_indexed` returns both the ordered vec
/// and a `HashMap<uuid, TranscriptEntry>` for O(1) lookup.
#[test]
fn load_transcript_indexed_builds_uuid_index() {
    let ws = IsolatedWorkspace::new();
    let sw = make_writer(&ws);
    let dir = session_dir(&sw);

    {
        let mut w = sw.lock().unwrap();
        w.append(&Message::user("q"), None, None).unwrap();
        w.append(&Message::assistant("a"), None, None).unwrap();
        w.finish(SessionStatus::Completed).unwrap();
    }

    let (entries, index) = SessionReader::load_transcript_indexed(&dir).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(index.len(), 2);

    for entry in &entries {
        let looked_up = index.get(&entry.uuid).expect("uuid should be in index");
        assert_eq!(looked_up.uuid, entry.uuid);
        assert_eq!(looked_up.content, entry.content);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 3: backward compat — old JSONL without uuid loads cleanly
// ─────────────────────────────────────────────────────────────────────────────

/// A JSONL file whose entries lack `uuid` / `parent_uuid` fields (pre-g155)
/// loads without error. Missing UUID defaults to an empty string.
#[test]
fn old_jsonl_without_uuid_loads_without_error() {
    let ws = IsolatedWorkspace::new();
    let sw = make_writer(&ws);
    let dir = {
        let guard = sw.lock().unwrap();
        guard.session_dir().to_path_buf()
    };
    drop(sw);

    // Write a legacy-format JSONL manually (no uuid fields).
    let jsonl_path = dir.join("transcript.jsonl");
    let legacy = r#"{"id":"msg_001","role":"user","content":"old message","timestamp":"2024-01-01T00:00:00Z"}
{"id":"msg_002","parent_id":"msg_001","role":"assistant","content":"old reply","timestamp":"2024-01-01T00:00:01Z"}
"#;
    std::fs::write(&jsonl_path, legacy).unwrap();

    let entries = SessionReader::load_transcript(&dir).unwrap();
    assert_eq!(entries.len(), 2);
    // uuid defaults to empty string for pre-g155 entries.
    assert_eq!(
        entries[0].uuid, "",
        "missing uuid should default to empty string"
    );
    assert_eq!(entries[0].content, "old message");
    assert_eq!(entries[1].content, "old reply");
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 4: tool-result source_tool_assistant_uuid
// ─────────────────────────────────────────────────────────────────────────────

/// Tool-result messages have `source_tool_assistant_uuid` pointing to the
/// assistant that issued the tool call (g155).
#[test]
fn tool_result_source_uuid_points_to_assistant() {
    let ws = IsolatedWorkspace::new();
    let sw = make_writer(&ws);
    let dir = session_dir(&sw);

    {
        let mut w = sw.lock().unwrap();
        w.append(&Message::user("run it"), None, None).unwrap();
        w.append(&Message::assistant("calling tool"), None, None)
            .unwrap();
        w.append(&Message::tool_result("call_1", "output"), None, None)
            .unwrap();
        w.finish(SessionStatus::Completed).unwrap();
    }

    let entries = SessionReader::load_transcript(&dir).unwrap();
    assert_eq!(entries.len(), 3);

    let assistant_uuid = &entries[1].uuid;
    let tool_result = &entries[2];
    // source_tool_assistant_uuid on the tool result points to the assistant entry.
    assert_eq!(
        tool_result.source_tool_assistant_uuid.as_deref(),
        Some(assistant_uuid.as_str()),
        "tool result should reference the assistant that issued the call"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 5: resume continues UUID chain
// ─────────────────────────────────────────────────────────────────────────────

/// After `open_existing`, the UUID chain continues from the last entry written
/// in the previous session — not restarting from None.
#[test]
fn open_existing_continues_uuid_chain() {
    let ws = IsolatedWorkspace::new();
    let dir;

    // First session: write two messages, then fully drop the writer.
    {
        let mut w = SessionWriter::create(ws.path(), "test", "gpt-4o", "openai").unwrap();
        dir = w.session_dir().to_path_buf();
        w.append(&Message::user("first"), None, None).unwrap();
        w.append(&Message::assistant("answer"), None, None).unwrap();
        // Drop without finish to simulate crash — SessionLock is released on Drop.
    }

    let first_entries = SessionReader::load_transcript(&dir).unwrap();
    let last_uuid_before = first_entries.last().unwrap().uuid.clone();

    // Resume: open_existing should recover last_uuid.
    let mut w2 = SessionWriter::open_existing(&dir).unwrap();
    w2.append(&Message::user("resumed"), None, None).unwrap();
    w2.finish(SessionStatus::Completed).unwrap();
    drop(w2);

    let all_entries = SessionReader::load_transcript(&dir).unwrap();
    assert_eq!(all_entries.len(), 3);

    // The resumed message's parent_uuid == last uuid from before.
    assert_eq!(
        all_entries[2].parent_uuid.as_deref(),
        Some(last_uuid_before.as_str()),
        "resumed message parent_uuid should continue from previous session"
    );
}
