//! Integration tests for Goal 152 — Incremental transcript writes.
//!
//! Verifies that `SessionPersistenceSink` + `AgentRuntime` persist each
//! completed message to `transcript.jsonl` as it is committed, rather than
//! in a single batch after the run.

use std::sync::Arc;

use recursive::test_util::PinnedRecursiveHome;
use recursive::{
    compact::Compactor,
    event::{CompositeSink, EventSink, NullSink},
    llm::{Completion, MockProvider, ToolCall},
    message::{Message, Role},
    session::{SessionReader, SessionWriter},
    AgentRuntime, ChannelSink, SessionPersistenceSink,
};
use serde_json::json;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn workspace() -> std::path::PathBuf {
    std::path::PathBuf::from("/tmp/g152-test-ws")
}

struct HomePin {
    _dir: tempfile::TempDir,
    _pin: PinnedRecursiveHome,
}

impl HomePin {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let pin = PinnedRecursiveHome::new(dir.path());
        Self {
            _dir: dir,
            _pin: pin,
        }
    }
}

fn make_session(_home: &HomePin) -> Arc<std::sync::Mutex<SessionWriter>> {
    let ws = workspace();
    let w = SessionWriter::create(&ws, "test goal", "mock-model", "mock")
        .expect("create session writer");
    Arc::new(std::sync::Mutex::new(w))
}

fn get_session_dir(sw: &Arc<std::sync::Mutex<SessionWriter>>) -> std::path::PathBuf {
    sw.lock().unwrap().session_dir().to_path_buf()
}

fn load_messages_from(sw: &Arc<std::sync::Mutex<SessionWriter>>) -> Vec<Message> {
    let dir = get_session_dir(sw);
    SessionReader::load_messages(&dir).expect("load messages")
}

fn simple_completion(text: &str) -> Completion {
    Completion {
        content: text.into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }
}

fn make_persistence_composite(sw: &Arc<std::sync::Mutex<SessionWriter>>) -> Arc<dyn EventSink> {
    let (channel_sink, _rx) = ChannelSink::new();
    Arc::new(CompositeSink::new(vec![
        Box::new(channel_sink) as Box<dyn EventSink>,
        Box::new(SessionPersistenceSink::new(sw.clone())) as Box<dyn EventSink>,
    ]))
}

// ---------------------------------------------------------------------------
// Test 1: messages written to disk BEFORE finalize_session_writer is called
// ---------------------------------------------------------------------------

/// After `runtime.run()` returns, `transcript.jsonl` already contains the
/// committed messages — even though `finalize_session_writer` (which sets
/// the final meta-status) has not been called yet.
///
/// This is the core property of Goal 152: "completed-or-bust" persistence is
/// replaced by per-message incremental writes.
#[tokio::test]
async fn messages_persisted_before_finalize() {
    let home = HomePin::new();
    let sw = make_session(&home);

    let llm = Arc::new(MockProvider::new(vec![simple_completion("ok")]));
    let sink = make_persistence_composite(&sw);

    let mut rt = AgentRuntime::builder()
        .llm(llm)
        .event_sink(sink)
        .build()
        .unwrap();

    rt.run("do something").await.unwrap();
    drop(rt); // runtime dropped; finalize_session_writer NOT called yet

    let transcript = load_messages_from(&sw);
    // Expect at minimum: user message + assistant message.
    assert!(
        transcript.len() >= 2,
        "expected ≥2 messages before finalize, got {}",
        transcript.len()
    );
    assert_eq!(transcript[0].role, Role::User);
    assert_eq!(transcript[0].content, "do something");
    assert_eq!(transcript[1].role, Role::Assistant);
    assert_eq!(transcript[1].content, "ok");
}

// ---------------------------------------------------------------------------
// Test 2: message with all three fields written as a single jsonl line
// ---------------------------------------------------------------------------

/// An assistant message that carries `content`, `tool_calls`, **and**
/// `reasoning_content` round-trips through the jsonl without data loss.
#[tokio::test]
async fn assistant_with_reasoning_and_tool_calls_one_line() {
    use recursive::llm::ToolSpec;
    use recursive::tools::ToolRegistry;
    use recursive::Tool;

    let home = HomePin::new();
    let sw = make_session(&home);

    let llm = Arc::new(MockProvider::new(vec![
        Completion {
            content: "thinking out loud".into(),
            tool_calls: vec![ToolCall {
                id: "call_42".into(),
                name: "echo_tool".into(),
                arguments: json!({"x": 1}),
            }],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: Some("my deep reasoning".into()),
        },
        simple_completion("done"),
    ]));

    // Minimal tool so the runtime can execute the tool call.
    struct EchoTool;
    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "echo_tool".into(),
                description: "echo".into(),
                parameters: json!({"type":"object","properties":{}}),
            }
        }
        async fn execute(&self, _args: serde_json::Value) -> recursive::error::Result<String> {
            Ok("echoed".into())
        }
    }
    let reg = ToolRegistry::local().register(Arc::new(EchoTool));

    let sink = make_persistence_composite(&sw);
    let mut rt = AgentRuntime::builder()
        .llm(llm)
        .tools(reg)
        .event_sink(sink)
        .build()
        .unwrap();

    rt.run("run the tool").await.unwrap();
    drop(rt);

    let transcript = load_messages_from(&sw);
    // user + assistant(tool_call+reasoning) + tool_result + assistant("done") = 4
    assert!(transcript.len() >= 3, "got {} messages", transcript.len());

    let with_reasoning = transcript
        .iter()
        .find(|m| m.reasoning_content.is_some())
        .expect("no message with reasoning_content");
    assert_eq!(with_reasoning.content, "thinking out loud");
    assert_eq!(
        with_reasoning.reasoning_content.as_deref(),
        Some("my deep reasoning")
    );
    assert_eq!(with_reasoning.tool_calls.len(), 1);
    assert_eq!(with_reasoning.tool_calls[0].name, "echo_tool");
}

// ---------------------------------------------------------------------------
// Test 3: streaming partial tokens do NOT produce extra jsonl lines
// ---------------------------------------------------------------------------

/// When streaming mode is enabled, `PartialToken` events are emitted but
/// must not produce separate jsonl lines.  Only complete `Message`s are
/// persisted — the jsonl line count equals the committed-message count.
#[tokio::test]
async fn streaming_partial_tokens_dont_persist() {
    let home = HomePin::new();
    let sw = make_session(&home);

    let llm = Arc::new(MockProvider::new(vec![simple_completion("streamed reply")]));
    let sink = Arc::new(CompositeSink::new(vec![
        Box::new(NullSink) as Box<dyn EventSink>,
        Box::new(SessionPersistenceSink::new(sw.clone())) as Box<dyn EventSink>,
    ]));

    let mut rt = AgentRuntime::builder()
        .llm(llm)
        .event_sink(sink)
        .streaming(true)
        .build()
        .unwrap();

    rt.run("stream something").await.unwrap();
    drop(rt);

    let transcript = load_messages_from(&sw);
    // Exactly 2: user + assistant — no extra lines from streaming chunks.
    assert_eq!(
        transcript.len(),
        2,
        "expected exactly 2 messages (user + assistant), got {}",
        transcript.len()
    );
    assert_eq!(transcript[0].content, "stream something");
    assert_eq!(transcript[1].content, "streamed reply");
}

// ---------------------------------------------------------------------------
// Test 4: compaction summary appears in jsonl
// ---------------------------------------------------------------------------

/// When cross-turn compaction fires, the synthesised summary message is
/// appended to the jsonl (even though it is inserted at index 0 in memory).
/// Pre-compaction messages remain in the file (append-only log).
#[tokio::test]
async fn compaction_summary_appears_in_jsonl() {
    let home = HomePin::new();
    let sw = make_session(&home);

    // Three turns. MockProvider needs replies for:
    // - turn1 (reply1)
    // - turn2 (reply2)
    // - cross-turn compaction after turn 2 (Goal 289)
    // - turn3 LLM (reply3)
    // - cross-turn compaction after turn 3
    // (Goal 345: intra-turn compaction before turn 3 is now rejected
    //  because older has <2 conversational messages, so one fewer
    //  LLM call is needed.)
    let llm = Arc::new(MockProvider::new(vec![
        simple_completion("reply1"),
        simple_completion("reply2"),
        simple_completion("compact summary"),
        simple_completion("reply3"),
        simple_completion("compact summary"),
    ]));

    let sink = Arc::new(CompositeSink::new(vec![
        Box::new(NullSink) as Box<dyn EventSink>,
        Box::new(SessionPersistenceSink::new(sw.clone())) as Box<dyn EventSink>,
    ]));

    // Threshold=1 char forces compaction every turn; keep_recent_n=2.
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

    let transcript = load_messages_from(&sw);

    // Compaction summary must appear in jsonl.
    // The compactor prepends "[compacted: N messages → M chars]\n" before the
    // model's text, so we search by substring rather than exact match.
    let has_summary = transcript
        .iter()
        .any(|m| m.content.contains("compact summary"));
    assert!(
        has_summary,
        "compaction summary not found; contents: {:?}",
        transcript.iter().map(|m| &m.content).collect::<Vec<_>>()
    );
    // g157: The compact_boundary marker causes load_transcript to skip
    // pre-compaction messages. "reply1" lives before the boundary and is
    // therefore not returned by the reader — this is the intended behaviour
    // (the summary replaced it). The file still contains reply1 physically
    // (append-only), but the reader won't surface it on resume.
    let post_boundary_count = transcript.len();
    assert!(
        post_boundary_count >= 1,
        "expected at least the compaction summary after boundary, got 0"
    );
}

// ---------------------------------------------------------------------------
// Test 5: resume after clean run — no duplicate messages
// ---------------------------------------------------------------------------

/// Load an existing session, run a second turn on the same writer — no
/// message is duplicated and the count grows by exactly 2 (user + assistant).
#[tokio::test]
async fn resume_after_clean_run_no_double_write() {
    let home = HomePin::new();
    let sw = make_session(&home);

    // --- First run ---
    {
        let llm = Arc::new(MockProvider::new(vec![simple_completion("first answer")]));
        let sink = make_persistence_composite(&sw);
        let mut rt = AgentRuntime::builder()
            .llm(llm)
            .event_sink(sink)
            .build()
            .unwrap();
        rt.run("first turn").await.unwrap();
    }

    let count_after_first = load_messages_from(&sw).len();
    assert_eq!(count_after_first, 2); // user + assistant

    // --- Second run (seed from disk, same writer) ---
    {
        let seed = load_messages_from(&sw);
        let llm = Arc::new(MockProvider::new(vec![simple_completion("second answer")]));
        let sink = make_persistence_composite(&sw);
        let mut rt = AgentRuntime::builder()
            .llm(llm)
            .event_sink(sink)
            .seed_transcript(seed)
            .build()
            .unwrap();
        rt.run("second turn").await.unwrap();
    }

    let transcript = load_messages_from(&sw);
    assert_eq!(
        transcript.len(),
        count_after_first + 2,
        "expected exactly 2 new messages; before={count_after_first} after={}",
        transcript.len()
    );

    // No content duplicates.
    let contents: Vec<&str> = transcript.iter().map(|m| m.content.as_str()).collect();
    let unique: std::collections::HashSet<_> = contents.iter().copied().collect();
    assert_eq!(
        contents.len(),
        unique.len(),
        "duplicate content: {contents:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 6: orphan shape survives reload
// ---------------------------------------------------------------------------

/// A session whose last on-disk message is an `assistant` with `tool_calls`
/// but no matching `tool` reply (orphan shape) can be loaded without data
/// loss.  This is the input g153 orphan detection will consume.
#[tokio::test]
async fn resume_after_crash_orphan_visible() {
    let home = HomePin::new();
    let sw = make_session(&home);

    // Manually write an orphan: assistant with tool_calls, crash before result.
    let session_dir = {
        let mut w = sw.lock().unwrap();
        w.append(&Message::user("do something"), None, None)
            .unwrap();
        w.append(
            &Message {
                role: Role::Assistant,
                content: "I will call the tool".into(),
                tool_calls: vec![ToolCall {
                    id: "orphan_call_1".into(),
                    name: "Bash".into(),
                    arguments: json!({"cmd": "dangerous"}),
                }],
                tool_call_id: None,
                reasoning_content: None,
                is_compaction_summary: false,
            },
            None,
            None,
        )
        .unwrap();
        // "Crash" here — no tool result written.
        w.session_dir().to_path_buf()
    };

    let transcript = SessionReader::load_messages(&session_dir).expect("load");

    assert_eq!(transcript.len(), 2, "expected user + orphan assistant");
    let last = &transcript[1];
    assert_eq!(last.role, Role::Assistant);
    assert_eq!(last.tool_calls.len(), 1);
    assert_eq!(last.tool_calls[0].id, "orphan_call_1");
    assert!(
        transcript.iter().all(|m| m.role != Role::Tool),
        "unexpected tool result in orphan transcript"
    );
}

// ---------------------------------------------------------------------------
// Test 7 (real-time persistence): rows land on disk DURING the turn
// ---------------------------------------------------------------------------

/// While the turn is still running — parked inside a tool execution — the
/// transcript file on disk must already contain the user message and the
/// assistant message that requested the tool call. Before the real-time
/// persistence fix, kernel messages were batched until `emit_turn_messages`
/// ran at turn end, so a process killed mid-turn (SIGKILL, or SIGTERM
/// force-exit) lost every step of the turn.
#[tokio::test]
async fn messages_persisted_while_turn_still_running() {
    use recursive::tools::ToolRegistry;
    use recursive::Tool;

    let home = HomePin::new();
    let sw = make_session(&home);

    // A tool that signals when it starts and blocks until released.
    let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let (release_tx, release_rx) = tokio::sync::watch::channel(false);
    struct Gate {
        started: tokio::sync::mpsc::UnboundedSender<()>,
        release: tokio::sync::watch::Receiver<bool>,
    }
    #[async_trait::async_trait]
    impl Tool for Gate {
        fn spec(&self) -> recursive::llm::ToolSpec {
            recursive::llm::ToolSpec {
                name: "gate_tool".into(),
                description: "blocks until released".into(),
                parameters: json!({"type": "object", "properties": {}}),
            }
        }
        async fn execute(&self, _args: serde_json::Value) -> recursive::error::Result<String> {
            let _ = self.started.send(());
            let mut rx = self.release.clone();
            while !*rx.borrow_and_update() {
                if rx.changed().await.is_err() {
                    break;
                }
            }
            Ok("released".into())
        }
    }

    let llm = Arc::new(MockProvider::new(vec![
        Completion {
            content: "calling the gate".into(),
            tool_calls: vec![ToolCall {
                id: "gate_call_1".into(),
                name: "gate_tool".into(),
                arguments: json!({}),
            }],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
        simple_completion("after the gate"),
    ]));
    let reg = ToolRegistry::local().register(Arc::new(Gate {
        started: started_tx,
        release: release_rx,
    }));
    let sink = make_persistence_composite(&sw);
    let mut rt = AgentRuntime::builder()
        .llm(llm)
        .tools(reg)
        .event_sink(sink)
        .build()
        .unwrap();

    let run = tokio::spawn(async move { rt.run("walk through the gate").await });

    // Wait until the gate tool is executing (the assistant message that
    // requested it has been pushed by then).
    tokio::time::timeout(std::time::Duration::from_secs(5), started_rx.recv())
        .await
        .expect("gate tool must start within 5s")
        .expect("started channel open");

    // THE regression: while the turn is still parked inside the tool, the
    // flushed transcript already holds the user + assistant(tool_calls) rows.
    // Poll briefly so the push-time event has traversed the forwarder task.
    let jsonl_path = get_session_dir(&sw).join("transcript.jsonl");
    let mid_run = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(content) = std::fs::read_to_string(&jsonl_path) {
                if content.contains("calling the gate") {
                    break content;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("assistant row must be flushed to disk mid-turn");
    assert!(
        mid_run.contains("walk through the gate"),
        "user row must be on disk mid-turn"
    );
    assert!(
        mid_run.contains("\"gate_call_1\""),
        "assistant tool_call must be on disk mid-turn"
    );

    // Release the gate; the turn finishes normally.
    let _ = release_tx.send(true);
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), run)
        .await
        .expect("turn must finish after release")
        .expect("run task must not panic")
        .expect("run must succeed");
    assert_eq!(outcome.steps, 2);

    // Exactly 4 rows — user, assistant(tool_calls), tool result, assistant
    // final. Guards against double-writing rows now that the kernel emits
    // MessageAppended incrementally: emit_turn_messages must not re-emit.
    let transcript = load_messages_from(&sw);
    let contents: Vec<&str> = transcript.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(
        transcript.len(),
        4,
        "exactly one row per committed message, got {contents:?}"
    );
    assert_eq!(transcript[2].role, Role::Tool);
    assert_eq!(transcript[2].tool_call_id.as_deref(), Some("gate_call_1"));
    assert_eq!(transcript[3].content, "after the gate");
}

// ---------------------------------------------------------------------------
// Test 8: per-step usage on assistant rows
// ---------------------------------------------------------------------------

/// With real-time persistence, usage is attached per assistant message (the
/// LLM call that produced it) instead of the whole turn's total riding on the
/// last row. Per-row values must sum to the same total, and non-assistant
/// rows carry no usage.
#[tokio::test]
async fn assistant_rows_carry_per_step_usage() {
    let home = HomePin::new();
    let sw = make_session(&home);

    let llm = Arc::new(MockProvider::new(vec![
        Completion {
            content: "step one".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: Some(recursive::llm::TokenUsage {
                prompt_tokens: 100,
                completion_tokens: 10,
                total_tokens: 110,
                cache_hit_tokens: 0,
                cache_miss_tokens: 0,
                reasoning_tokens: 0,
            }),
            reasoning_content: None,
        },
    ]));
    let sink = make_persistence_composite(&sw);
    let mut rt = AgentRuntime::builder()
        .llm(llm)
        .event_sink(sink)
        .build()
        .unwrap();
    rt.run("hi").await.unwrap();
    drop(rt);

    let entries = SessionReader::load_transcript(&get_session_dir(&sw)).unwrap();
    assert_eq!(entries.len(), 2, "user + assistant");
    assert!(entries[0].usage.is_none(), "user row carries no usage");
    let usage = entries[1]
        .usage
        .as_ref()
        .expect("assistant row carries the usage of its LLM call");
    assert_eq!(usage.input_tokens, 100);
    assert_eq!(usage.output_tokens, 10);
}
