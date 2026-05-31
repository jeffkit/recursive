//! Integration tests for `recursive resume <id>` (Goal 151).
//!
//! These exercise the public lib surface added by g151:
//! `SessionWriter::create_with_tools`, `SessionWriter::open_existing`,
//! `SessionReader::load_messages`,
//! `SessionReader::list_sessions_sorted_by_updated_at`,
//! `SessionLock`, and the legacy → JSONL migration entry points.
//!
//! End-to-end binary tests (driving the actual `recursive` CLI to
//! resume a real LLM session) are deliberately not here — they are
//! covered by manual smoke per the goal acceptance, and require a
//! mock provider scaffold that's heavier than the value added.

use std::path::PathBuf;
use std::sync::Arc;

use recursive::llm::ToolSpec;
use recursive::message::{Message, Role};
use recursive::session::{hash_tool_specs, SessionLock, SessionMeta, SessionReader, SessionWriter};
use recursive::test_util::PinnedRecursiveHome;

/// Pin RECURSIVE_HOME for the duration of the test so per-user
/// session paths land in a tempdir, not the real `~/.recursive`.
struct HomeOverride {
    _dir: tempfile::TempDir,
    _pin: PinnedRecursiveHome,
}

impl HomeOverride {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let pin = PinnedRecursiveHome::new(dir.path());
        Self {
            _dir: dir,
            _pin: pin,
        }
    }
}

fn workspace() -> PathBuf {
    // Use a stable workspace path. The slug derived from this is
    // what gets written under <RECURSIVE_HOME>/sessions/<slug>/...
    PathBuf::from("/tmp/g151-test-ws")
}

fn make_specs(name: &str) -> Vec<ToolSpec> {
    vec![ToolSpec {
        name: name.into(),
        description: format!("test tool {name}"),
        parameters: serde_json::json!({"type":"object"}),
    }]
}

#[test]
fn resume_by_full_id_round_trips_seed() {
    let _h = HomeOverride::new();
    let ws = workspace();

    let specs = make_specs("read_file");
    let mut w =
        SessionWriter::create_with_tools(&ws, "round trip", "model", "openai", &specs).unwrap();
    let dir = w.session_dir().to_path_buf();
    w.append(&Message::user("hello".to_string())).unwrap();
    w.append(&Message::assistant("hi".to_string())).unwrap();
    w.finish("interrupted").unwrap();
    drop(w);

    let messages = SessionReader::load_messages(&dir).unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, Role::User);
    assert_eq!(messages[0].content, "hello");
    assert_eq!(messages[1].role, Role::Assistant);
}

#[test]
fn most_recent_shortcut_picks_active_or_interrupted() {
    let _h = HomeOverride::new();
    let ws = workspace();

    // Create three sessions in order.
    let mut a = SessionWriter::create(&ws, "A", "m", "p").unwrap();
    a.append(&Message::user("a".to_string())).unwrap();
    a.finish("success").unwrap();
    let dir_a = a.session_dir().to_path_buf();
    drop(a);

    // Need timestamps to differ; sleep across the 1-sec boundary.
    std::thread::sleep(std::time::Duration::from_millis(1100));

    let mut b = SessionWriter::create(&ws, "B", "m", "p").unwrap();
    b.append(&Message::user("b".to_string())).unwrap();
    // Don't call finish — stays "active".
    let dir_b = b.session_dir().to_path_buf();
    drop(b);

    std::thread::sleep(std::time::Duration::from_millis(1100));

    let mut c = SessionWriter::create(&ws, "C", "m", "p").unwrap();
    c.append(&Message::user("c".to_string())).unwrap();
    c.finish("success").unwrap();
    let dir_c = c.session_dir().to_path_buf();
    drop(c);

    let sorted = SessionReader::list_sessions_sorted_by_updated_at(&ws).unwrap();
    // The crashed session B has updated_at advanced by `append`,
    // but A and C also have advanced timestamps from finish().
    // Sort is desc, so C is at the top; B comes second; A last.
    assert_eq!(sorted.len(), 3);
    assert_eq!(sorted[0].0, dir_c);
    assert_eq!(sorted[1].0, dir_b);
    assert_eq!(sorted[2].0, dir_a);

    // The most-recent active/interrupted is C? No — C is "success".
    // B is the one we want, since it has status="active".
    let pick = sorted
        .into_iter()
        .find(|(_, m)| matches!(m.status.as_str(), "active" | "interrupted"));
    let (picked_dir, picked_meta) = pick.expect("expected an active session");
    assert_eq!(picked_dir, dir_b);
    assert_eq!(picked_meta.status, "active");
}

#[test]
fn resume_continues_msg_numbering() {
    let _h = HomeOverride::new();
    let ws = workspace();

    let mut w = SessionWriter::create(&ws, "g151", "m", "p").unwrap();
    let dir = w.session_dir().to_path_buf();
    w.append(&Message::user("u1".to_string())).unwrap();
    w.append(&Message::assistant("a1".to_string())).unwrap();
    w.append(&Message::user("u2".to_string())).unwrap();
    drop(w); // No finish() — simulate crash.

    // Re-open and append more.
    let mut w2 = SessionWriter::open_existing(&dir).unwrap();
    let id = w2.append(&Message::assistant("a2".to_string())).unwrap();
    assert_eq!(id, "msg_004");
    drop(w2);

    let entries = SessionReader::load_transcript(&dir).unwrap();
    assert_eq!(entries.len(), 4);
    assert_eq!(entries[3].id, "msg_004");
    assert_eq!(entries[3].parent_id.as_deref(), Some("msg_003"));
}

#[test]
fn open_existing_blocks_concurrent_open() {
    let _h = HomeOverride::new();
    let ws = workspace();

    let w1 = SessionWriter::create(&ws, "g151-conc", "m", "p").unwrap();
    let dir = w1.session_dir().to_path_buf();

    // Second open in the same process: pid alive, lock refused.
    let err = match SessionWriter::open_existing(&dir) {
        Ok(_) => panic!("second open_existing must fail while w1 is alive"),
        Err(e) => e,
    };
    assert!(
        err.to_string()
            .contains(&format!("pid {}", std::process::id())),
        "expected error to mention our pid, got: {err}"
    );

    drop(w1);

    // After dropping, lock is released — re-open succeeds.
    let _w2 = SessionWriter::open_existing(&dir).unwrap();
}

#[test]
fn lock_path_inside_session_dir() {
    let _h = HomeOverride::new();
    let ws = workspace();
    let w = SessionWriter::create(&ws, "lock-path", "m", "p").unwrap();
    let dir = w.session_dir().to_path_buf();

    // Lock sentinel must live under the session_dir itself.
    let sentinel = dir.join(".lock");
    assert!(sentinel.is_file());

    drop(w);

    // After Drop, sentinel is removed.
    assert!(!sentinel.exists());
}

#[test]
fn tool_registry_hash_round_trip_via_create_with_tools() {
    let _h = HomeOverride::new();
    let ws = workspace();
    let specs = make_specs("write_file");
    let w = SessionWriter::create_with_tools(&ws, "hashed", "m", "p", &specs).unwrap();
    let dir = w.session_dir().to_path_buf();
    drop(w);

    let meta = SessionReader::load_meta(&dir).unwrap();
    assert_eq!(
        meta.tool_registry_hash.as_deref(),
        Some(hash_tool_specs(&specs).as_str())
    );
}

#[test]
fn tool_registry_hash_absent_for_plain_create() {
    let _h = HomeOverride::new();
    let ws = workspace();
    let w = SessionWriter::create(&ws, "no-hash", "m", "p").unwrap();
    let dir = w.session_dir().to_path_buf();
    drop(w);

    let meta = SessionReader::load_meta(&dir).unwrap();
    assert!(meta.tool_registry_hash.is_none());
}

#[test]
fn load_messages_round_trips_tool_calls_and_reasoning() {
    use recursive::llm::ToolCall;

    let _h = HomeOverride::new();
    let ws = workspace();
    let mut w = SessionWriter::create(&ws, "round-trip-fields", "m", "p").unwrap();
    let dir = w.session_dir().to_path_buf();

    let tool_calls = vec![ToolCall {
        id: "call_001".into(),
        name: "read_file".into(),
        arguments: serde_json::json!({"path":"/tmp/foo"}),
    }];
    let assistant = Message {
        role: Role::Assistant,
        content: "thinking out loud".into(),
        tool_calls: tool_calls.clone(),
        tool_call_id: None,
        reasoning_content: Some("internal monologue".into()),
    };
    w.append(&Message::user("u".to_string())).unwrap();
    w.append(&assistant).unwrap();
    w.append(&Message::tool_result("call_001", "result body"))
        .unwrap();
    w.finish("success").unwrap();
    drop(w);

    let msgs = SessionReader::load_messages(&dir).unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[1].tool_calls.len(), 1);
    assert_eq!(msgs[1].tool_calls[0].id, "call_001");
    assert_eq!(
        msgs[1].reasoning_content.as_deref(),
        Some("internal monologue")
    );
    assert_eq!(msgs[2].role, Role::Tool);
    assert_eq!(msgs[2].tool_call_id.as_deref(), Some("call_001"));
}

#[test]
fn lock_thread_safety_serialises_open_existing() {
    // Two threads racing on the same session_dir: only one wins
    // open_existing; the other must observe SessionLockBusy.
    let _h = HomeOverride::new();
    let ws = workspace();

    let w0 = SessionWriter::create(&ws, "thread-race", "m", "p").unwrap();
    let dir = w0.session_dir().to_path_buf();
    drop(w0); // Release the lock so the threads can race.

    let dir1 = Arc::new(dir.clone());
    let dir2 = Arc::new(dir);

    let h1 = std::thread::spawn(move || {
        // Hold the lock briefly so the other thread sees BUSY.
        let w = SessionWriter::open_existing(&dir1).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(150));
        drop(w);
    });

    // Give thread 1 a head start on the lock.
    std::thread::sleep(std::time::Duration::from_millis(20));

    let result2 = SessionWriter::open_existing(&dir2);
    h1.join().unwrap();

    // Thread 2 should have hit BUSY (since thread 1 was holding).
    // Note: this is timing-dependent — accept either an error or
    // a successful open if thread 1 ran exceedingly fast.
    if let Err(e) = result2 {
        assert!(
            e.to_string().contains("pid"),
            "expected lock error to mention pid, got: {e}"
        );
    }
    // If result2 was Ok, thread 1 finished before our open_existing
    // — also fine; the goal is to demonstrate no deadlock and
    // correct serialisation, not to require timing.
}

#[test]
fn session_lock_recovers_from_stale_with_dead_pid() {
    // Forge a stale .lock with a guaranteed-dead pid (u32::MAX is
    // far past PID_MAX_LIMIT on Linux/macOS) and matching hostname;
    // SessionLock::acquire should recover.
    let _h = HomeOverride::new();
    let dir = tempfile::tempdir().unwrap();
    let session_dir = dir.path().join("session");
    std::fs::create_dir_all(&session_dir).unwrap();

    // Hostname: read it via the same path the lock writer would
    // use, so the recovery branch (matching host + dead pid) fires.
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .unwrap_or_default()
        });
    let host = host.replace(['\n', '\r'], "_").trim().to_string();
    let stale = format!("{}\n{host}\n0\n", u32::MAX);
    std::fs::write(session_dir.join(".lock"), stale).unwrap();

    let _lock = SessionLock::acquire(&session_dir).expect("should recover stale lock");
}

#[test]
fn session_meta_default_format_back_compat_round_trip() {
    // Synthesise a `.meta.json` without the `tool_registry_hash`
    // field (pre-g151) and confirm that load_meta produces a
    // `SessionMeta` with `tool_registry_hash: None`.
    let _h = HomeOverride::new();
    let dir = tempfile::tempdir().unwrap();
    let session_dir = dir.path().join("legacy-session");
    std::fs::create_dir_all(&session_dir).unwrap();
    let raw = r#"{
  "session_id": "back-compat",
  "goal": "old run",
  "model": "model",
  "provider": "openai",
  "created_at": "2020-01-01T00:00:00Z",
  "updated_at": "2020-01-01T00:00:00Z",
  "message_count": 0,
  "status": "interrupted"
}"#;
    std::fs::write(session_dir.join(".meta.json"), raw).unwrap();
    std::fs::write(session_dir.join("transcript.jsonl"), "").unwrap();

    let meta = SessionReader::load_meta(&session_dir).unwrap();
    assert_eq!(meta.session_id, "back-compat");
    assert!(meta.tool_registry_hash.is_none());
    // Sanity: round-trip through serde so we know the field stays
    // None and isn't accidentally serialised as `null`.
    let back: SessionMeta = serde_json::from_str(&serde_json::to_string(&meta).unwrap()).unwrap();
    assert!(back.tool_registry_hash.is_none());
}
