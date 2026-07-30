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
use recursive::session::{SessionReader, SessionStatus, SessionWriter};
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
        // Cross-turn compaction after turn 2 (Goal 289).
        simple_completion("compact summary"),
        simple_completion("reply3"),
        // Intra-turn compaction during turn 3 (kernel's run_core maybe_compact
        // fires before each step's LLM call when transcript ≥ keep_recent_n+2).
        simple_completion("compact summary"),
        // Cross-turn compaction after turn 3.
        simple_completion("compact summary"),
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
    sw.lock().unwrap().finish(SessionStatus::Completed).ok();

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
        // Cross-turn compaction after turn 2 (Goal 289).
        simple_completion("compact summary"),
        simple_completion("reply3"),
        // Intra-turn compaction during turn 3 (kernel's run_core maybe_compact
        // fires before each step's LLM call when transcript ≥ keep_recent_n+2).
        simple_completion("compact summary"),
        // Cross-turn compaction after turn 3.
        simple_completion("compact summary"),
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
    sw.lock().unwrap().finish(SessionStatus::Completed).ok();

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
    w.finish(SessionStatus::Completed).unwrap();
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
    w.finish(SessionStatus::Completed).unwrap();
    drop(w);

    let entries = SessionReader::load_transcript(&dir).unwrap();
    assert_eq!(
        entries.len(),
        3,
        "all 3 messages should load when no boundary is present"
    );
}

// ── Goal 336: cache telemetry on the CompactionBoundary event ─────────
//
// The cross-turn compaction path must forward the just-completed turn's
// cache_hit_tokens / cache_miss_tokens onto the emitted CompactionBoundary
// event, so the journal/TUI/HTTP sink can record the pre-compact cache state
// (the data that decides whether goal 341's cache-preserving work pays off).
// These call AgentRuntime::maybe_compact_cross_turn directly with a transcript
// seeded past the 100-char threshold so compaction always fires.

fn seed_runtime_for_boundary(
    provider: Arc<MockProvider>,
) -> (
    AgentRuntime,
    tokio::sync::mpsc::UnboundedReceiver<recursive::AgentEvent>,
) {
    let (sink, rx) = recursive::event::ChannelSink::new();
    let mut rt = AgentRuntime::builder()
        .llm(provider)
        .compactor(Compactor::new(100).keep_recent_n(1))
        .event_sink(Arc::new(sink))
        .build()
        .unwrap();
    // 8 user/assistant pairs, 40 chars each → ~640 chars, well over the 100 threshold.
    let msgs: Vec<Message> = (0..16)
        .map(|i| {
            if i % 2 == 0 {
                Message::user("x".repeat(40))
            } else {
                Message::assistant("y".repeat(40))
            }
        })
        .collect();
    rt.set_transcript(msgs);
    (rt, rx)
}

fn summary_completion() -> recursive::llm::Completion {
    recursive::llm::Completion {
        content: "summary".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }
}

#[tokio::test]
async fn compaction_boundary_emits_cache_metrics() {
    let (mut rt, mut rx) =
        seed_runtime_for_boundary(Arc::new(MockProvider::new(vec![summary_completion()])));
    while rx.try_recv().is_ok() {}
    rt.maybe_compact_cross_turn(&recursive::TokenUsage {
        cache_hit_tokens: 30,
        cache_miss_tokens: 70,
        ..Default::default()
    })
    .await
    .unwrap();
    let mut found = false;
    while let Ok(ev) = rx.try_recv() {
        if matches!(
            ev,
            recursive::AgentEvent::CompactionBoundary {
                cache_hit_tokens: 30,
                cache_miss_tokens: 70,
                ..
            }
        ) {
            found = true;
        }
    }
    assert!(
        found,
        "CompactionBoundary must carry the turn's cache_hit/miss_tokens"
    );
}

#[tokio::test]
async fn compaction_boundary_cache_zero_when_no_usage() {
    let (mut rt, mut rx) =
        seed_runtime_for_boundary(Arc::new(MockProvider::new(vec![summary_completion()])));
    while rx.try_recv().is_ok() {}
    rt.maybe_compact_cross_turn(&recursive::TokenUsage::default())
        .await
        .unwrap();
    let mut found = false;
    while let Ok(ev) = rx.try_recv() {
        if matches!(
            ev,
            recursive::AgentEvent::CompactionBoundary {
                cache_hit_tokens: 0,
                cache_miss_tokens: 0,
                ..
            }
        ) {
            found = true;
        }
    }
    assert!(
        found,
        "CompactionBoundary cache fields must be 0 when usage is default"
    );
}

// ── Goal 338: recompaction-in-chain telemetry ──────────────────────────
//
// The CompactionBoundary event carries three new fields:
//   is_recompaction_in_chain, turns_since_previous_compact,
//   previous_compact_turn.
// These are populated from AgentRuntime::last_compact_turn and distinguish
// first-in-session compaction from recompaction chains.

/// Drive one compaction (via maybe_compact_cross_turn) and collect the
/// emitted CompactionBoundary event from the receiver.
async fn capture_compaction_boundary(
    rt: &mut AgentRuntime,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<recursive::AgentEvent>,
    usage: recursive::llm::TokenUsage,
) -> Option<recursive::AgentEvent> {
    while rx.try_recv().is_ok() {} // drain old events
    rt.maybe_compact_cross_turn(&usage).await.unwrap();
    while let Ok(ev) = rx.try_recv() {
        if matches!(ev, recursive::AgentEvent::CompactionBoundary { .. }) {
            return Some(ev);
        }
    }
    None
}

#[tokio::test]
async fn first_compaction_is_not_recompaction() {
    let (mut rt, mut rx) = seed_runtime_for_boundary(Arc::new(MockProvider::new(vec![
        summary_completion(),
        summary_completion(),
    ])));

    let ev = capture_compaction_boundary(&mut rt, &mut rx, recursive::llm::TokenUsage::default())
        .await
        .expect("first compaction must emit CompactionBoundary");

    match ev {
        recursive::AgentEvent::CompactionBoundary {
            is_recompaction_in_chain,
            turns_since_previous_compact,
            previous_compact_turn,
            ..
        } => {
            assert!(
                !is_recompaction_in_chain,
                "first compaction must NOT be a recompaction"
            );
            assert_eq!(
                turns_since_previous_compact, 0,
                "first compaction must have turns_since=0"
            );
            assert!(
                previous_compact_turn.is_none(),
                "first compaction must have previous_compact_turn=None"
            );
        }
        other => panic!("expected CompactionBoundary, got {other:?}"),
    }
}

#[tokio::test]
async fn recompaction_marks_chain_when_within_session() {
    let (mut rt, mut rx) = seed_runtime_for_boundary(Arc::new(MockProvider::new(vec![
        summary_completion(),
        summary_completion(),
    ])));

    // First compaction — not a recompaction.
    capture_compaction_boundary(&mut rt, &mut rx, recursive::llm::TokenUsage::default())
        .await
        .expect("first compact");

    // Second compaction — reseed transcript past threshold.
    let msgs: Vec<Message> = (0..16)
        .map(|i| {
            if i % 2 == 0 {
                Message::user("x".repeat(40))
            } else {
                Message::assistant("y".repeat(40))
            }
        })
        .collect();
    rt.set_transcript(msgs);

    let ev = capture_compaction_boundary(&mut rt, &mut rx, recursive::llm::TokenUsage::default())
        .await
        .expect("second compaction must emit CompactionBoundary");

    match ev {
        recursive::AgentEvent::CompactionBoundary {
            is_recompaction_in_chain,
            previous_compact_turn,
            ..
        } => {
            assert!(
                is_recompaction_in_chain,
                "second compaction must be marked as recompaction"
            );
            assert_eq!(
                previous_compact_turn,
                Some(0),
                "previous compact turn should match first compaction turn"
            );
        }
        other => panic!("expected CompactionBoundary, got {other:?}"),
    }
}

#[tokio::test]
async fn manual_then_auto_detected_as_chain() {
    // Provider: 2 summary completions (one for compact_now, one for auto)
    let summary = summary_completion();
    let llm = Arc::new(MockProvider::new(vec![summary.clone(), summary.clone()]));
    let (sink, mut rx) = recursive::event::ChannelSink::new();
    let mut rt = AgentRuntime::builder()
        .llm(llm)
        .compactor(Compactor::new(100).keep_recent_n(1))
        .event_sink(Arc::new(sink))
        .build()
        .unwrap();

    // Seed a transcript with enough messages for compact_now
    // (keep_recent_n=1 means any 3+ messages get compacted).
    let msgs: Vec<Message> = (0..8)
        .map(|i| {
            if i % 2 == 0 {
                Message::user("x".repeat(40))
            } else {
                Message::assistant("y".repeat(40))
            }
        })
        .collect();
    rt.set_transcript(msgs.clone());

    // Step 1: manual compact via compact_now
    while rx.try_recv().is_ok() {}
    rt.compact_now().await.unwrap();

    // Step 2: reseed so cross-turn compaction will fire.
    rt.set_transcript(msgs);
    while rx.try_recv().is_ok() {}
    rt.maybe_compact_cross_turn(&recursive::llm::TokenUsage::default())
        .await
        .unwrap();

    // Find the CompactionBoundary from the auto-compact.
    let mut auto_boundary: Option<recursive::AgentEvent> = None;
    while let Ok(ev) = rx.try_recv() {
        if matches!(ev, recursive::AgentEvent::CompactionBoundary { .. }) {
            auto_boundary = Some(ev);
        }
    }

    let ev = auto_boundary.expect("auto-compact must emit CompactionBoundary");
    match ev {
        recursive::AgentEvent::CompactionBoundary {
            is_recompaction_in_chain,
            previous_compact_turn,
            ..
        } => {
            assert!(
                is_recompaction_in_chain,
                "auto compact after manual compact must be a recompaction chain"
            );
            assert_eq!(
                previous_compact_turn,
                Some(0),
                "previous_compact_turn should be the turn of the manual compact (turn 0)"
            );
        }
        other => panic!("expected CompactionBoundary, got {other:?}"),
    }
}
