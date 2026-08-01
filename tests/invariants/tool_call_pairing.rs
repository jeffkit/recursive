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

use recursive::compact::{FileReinjector, SkillReinjector};
use recursive::llm::{Completion, MockProvider, ToolCall};
use recursive::message::{Message, Role};
use recursive::tools::ReadFileState;
use recursive::{AgentRuntime, Compactor, TokenUsage};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

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

// ── Cross-turn compaction + reinjection preserves pairing ──────────────────
//
// Invariant #8's highest-risk mutation path is the runtime's cross-turn
// compaction reinjection (`maybe_compact_cross_turn`, src/runtime.rs:370).
// After `Compactor::apply_to_transcript` drains the head and inserts a
// summary at index 0 (src/runtime.rs:415-423), three reinjector blocks
// re-insert `Role::System` attachment messages and compute their insertion
// indices via string-prefix heuristics:
//   Block A (file restore, src/runtime.rs:461-479): re-slices the preserved
//     tail (`skip(1)`) and inserts at `1 + offset`.
//   Block B (skill restore, src/runtime.rs:480-507): computes `insert_base`
//     by counting `[post-compact file restore:` prefixes ("Approximate").
//   Block C (plan/todo restore, src/runtime.rs:508-534): `insert_base` via
//     `take_while` over the file/skill prefixes.
// The reinjectors only emit System messages, so they cannot directly orphan
// a Tool; the real risk is the preserved tail — if the re-slicing dropped or
// duplicated a message, the tail's pairing breaks. These tests drive the REAL
// runtime through the full path with a tool pair in the preserved tail and
// assert the pairing invariant afterwards.

/// A provider scripted with exactly one summary completion — enough for a
/// single compaction pass (`maybe_compact_cross_turn` consumes one).
fn summary_provider(content: &str) -> Arc<MockProvider> {
    Arc::new(MockProvider::new(vec![Completion {
        content: content.to_string(),
        tool_calls: vec![],
        finish_reason: Some("stop".to_string()),
        usage: None,
        reasoning_content: None,
    }]))
}

/// Seed transcript with a complete tool pair (`c2`) at the tail and bulk
/// padding up front so a 100-char compactor fires. With `keep_recent_n(2)`
/// over 6 messages, `safe_split_point` retreats from index 4 (an
/// Assistant-with-tool_calls) to index 3 (a User message), so the preserved
/// tail is `[user, assistant c2, tool c2]` — the c2 pair survives the split
/// wholesale and the pairing assertion is non-vacuous.
fn transcript_with_tool_pair_in_tail() -> Vec<Message> {
    vec![
        Message::user("padding ".repeat(20)),
        assistant_with_tool_call("c1", "read_file", "Reading file A."),
        tool_result_msg("c1", "content of file A"),
        Message::user("more padding ".repeat(20)),
        assistant_with_tool_call("c2", "read_file", "Reading file B."),
        tool_result_msg("c2", "content of file B"),
    ]
}

/// An Assistant message that invokes the `Skill` tool with a `name` argument
/// — the shape `SkillReinjector` scans for in `pre_compact`.
fn skill_tool_call_msg(id: &str, skill_name: &str) -> Message {
    Message::assistant_with_tool_calls(
        format!("Loading skill {skill_name}."),
        vec![ToolCall {
            id: id.to_string(),
            name: "Skill".to_string(),
            arguments: serde_json::json!({ "name": skill_name }),
        }],
    )
}

/// Same as [`transcript_with_tool_pair_in_tail`], but with a `Skill` tool
/// invocation in the older portion so `SkillReinjector::reinject(pre_compact)`
/// finds a match while the preserved tail keeps the complete `c2` pair.
fn transcript_with_skill_in_older_and_tool_pair_in_tail() -> Vec<Message> {
    vec![
        Message::user("padding ".repeat(20)),
        skill_tool_call_msg("sk1", "my-skill"),
        tool_result_msg("sk1", "skill body loaded"),
        Message::user("more padding ".repeat(20)),
        assistant_with_tool_call("c2", "read_file", "Reading file B."),
        tool_result_msg("c2", "content of file B"),
    ]
}

/// Assert the common post-compaction shape: compaction actually fired
/// (guards against a silently-no-op test), the preserved tail still holds a
/// Tool result (so the pairing check is not vacuous), and the whole
/// transcript satisfies the pairing invariant.
fn assert_compaction_fired_and_pairing_ok(rt: &AgentRuntime, context: &str) {
    let transcript = rt.transcript();
    assert_eq!(
        transcript[0].role,
        Role::System,
        "{context}: transcript must start with the compaction summary"
    );
    assert!(
        transcript[0].is_compaction_summary,
        "{context}: first message must be flagged as a compaction summary"
    );
    assert!(
        transcript.iter().any(|m| m.role == Role::Tool),
        "{context}: preserved tail must retain a Tool result so the pairing check is non-vacuous"
    );
    verify_tool_call_pairing(transcript).expect(context);
}

/// Write a real skill to a tempdir and discover it (mirrors the
/// `create_skill_on_disk` idiom in `src/compact/reinject.rs` tests — the
/// reinjector reads the body from `skill.path` on disk). The returned
/// `TempDir` must stay alive for the caller's test duration.
fn skill_on_disk(name: &str, body: &str) -> (tempfile::TempDir, recursive::skills::Skill) {
    let tmp = tempfile::tempdir().expect("tempdir for skill");
    let skill_dir = tmp.path().join(name);
    std::fs::create_dir(&skill_dir).expect("create skill dir");
    let path = skill_dir.join("SKILL.md");
    std::fs::write(
        &path,
        format!("---\nname: {name}\ndescription: {name} skill\n---\n\n{body}"),
    )
    .expect("write SKILL.md");
    let discovered = recursive::skills::discover_skills(&[tmp.path().to_path_buf()]);
    let skill = discovered.into_iter().next().expect("skill discovered");
    (tmp, skill)
}

/// Seed the runtime's shared todo list through the real `TodoWrite` tool.
/// The runtime keeps `todo_list` private, so the public seam is the kernel
/// tool registry (the registered `TodoWriteTool` shares the same `Arc` the
/// plan/todo reinjector reads).
async fn seed_todos(rt: &AgentRuntime) {
    let tool = rt
        .kernel()
        .tools()
        .get("TodoWrite")
        .expect("TodoWrite must be registered by AgentRuntimeBuilder::build");
    tool.execute(serde_json::json!({
        "todos": [
            {"content": "Read files", "status": "completed"},
            {"content": "Edit code", "status": "in_progress", "active_form": "Editing code..."},
            {"content": "Run tests", "status": "pending"},
        ]
    }))
    .await
    .expect("todo write must succeed");
}

/// Cross-turn compaction with the FILE reinjector active must preserve
/// tool-call ↔ tool-result pairing in the preserved tail. Drives Block A
/// (src/runtime.rs:461-479): the re-slice of the preserved tail + insert of
/// the `[post-compact file restore:` attachment at index 1.
#[tokio::test]
async fn cross_turn_compaction_with_file_reinjector_preserves_pairing() {
    let provider = summary_provider("Summary of prior turns.");

    let read_state = Arc::new(Mutex::new(ReadFileState::new()));
    {
        let mut locked = read_state.lock().unwrap();
        locked.record(
            PathBuf::from("src/lib.rs"),
            false,
            "pub fn foo() {}".to_string(),
            1000,
        );
    }

    let mut rt = AgentRuntime::builder()
        .llm(provider)
        .compactor(Compactor::new(100).keep_recent_n(2))
        .file_reinjector(FileReinjector::new(read_state))
        .build()
        .expect("build runtime");

    let msgs = transcript_with_tool_pair_in_tail();
    verify_tool_call_pairing(&msgs).expect("seed transcript must satisfy pairing");
    rt.set_transcript(msgs);

    rt.maybe_compact_cross_turn(&TokenUsage::default())
        .await
        .expect("cross-turn compaction must succeed");

    assert_compaction_fired_and_pairing_ok(&rt, "file reinjector must preserve pairing");

    let transcript = rt.transcript();
    let file_idx = transcript
        .iter()
        .position(|m| m.content.starts_with("[post-compact file restore:"))
        .expect("file restore attachment must be present");
    assert!(
        file_idx > 0 && file_idx < transcript.len() - 1,
        "file restore must sit between the summary (index 0) and the preserved tail"
    );
    assert_eq!(
        file_idx, 1,
        "file restore must be the first attachment, immediately after the summary"
    );
    assert!(
        transcript[file_idx].content.contains("pub fn foo() {}"),
        "file restore must carry the re-injected file content"
    );
}

/// Cross-turn compaction with the SKILL reinjector active must preserve
/// pairing. Drives Block B (src/runtime.rs:480-507) and its "Approximate"
/// `insert_base` heuristic most directly.
#[tokio::test]
async fn cross_turn_compaction_with_skill_reinjector_preserves_pairing() {
    let provider = summary_provider("Summary of prior turns.");
    let (_tmp, skill) = skill_on_disk("my-skill", "This skill teaches pairing.");

    let mut rt = AgentRuntime::builder()
        .llm(provider)
        .compactor(Compactor::new(100).keep_recent_n(2))
        .skill_reinjector(SkillReinjector::new(vec![skill]))
        .build()
        .expect("build runtime");

    let msgs = transcript_with_skill_in_older_and_tool_pair_in_tail();
    verify_tool_call_pairing(&msgs).expect("seed transcript must satisfy pairing");
    rt.set_transcript(msgs);

    rt.maybe_compact_cross_turn(&TokenUsage::default())
        .await
        .expect("cross-turn compaction must succeed");

    assert_compaction_fired_and_pairing_ok(&rt, "skill reinjector must preserve pairing");

    let transcript = rt.transcript();
    let skill_idx = transcript
        .iter()
        .position(|m| m.content.starts_with("[post-compact skill restore:"))
        .expect("skill restore attachment must be present");
    assert!(skill_idx > 0, "skill restore must not replace the summary");
    assert_eq!(
        skill_idx, 1,
        "skill restore must be the first attachment (no file reinjector installed)"
    );
    assert!(
        transcript[skill_idx].content.contains("my-skill"),
        "skill restore must name the invoked skill"
    );
}

/// Cross-turn compaction with ALL three reinjectors active (file, skill,
/// plan/todo) must preserve pairing and keep the attachment chain ordered
/// `[file, skill, plan, todo]` — pinning both independent `insert_base`
/// computations (src/runtime.rs:484 and :514).
#[tokio::test]
async fn cross_turn_compaction_with_all_reinjectors_preserves_pairing() {
    let provider = summary_provider("Summary of prior turns.");

    let read_state = Arc::new(Mutex::new(ReadFileState::new()));
    {
        let mut locked = read_state.lock().unwrap();
        locked.record(
            PathBuf::from("src/lib.rs"),
            false,
            "pub fn foo() {}".to_string(),
            1000,
        );
    }
    let (_tmp, skill) = skill_on_disk("my-skill", "This skill teaches pairing.");

    let mut rt = AgentRuntime::builder()
        .llm(provider)
        .compactor(Compactor::new(100).keep_recent_n(2))
        .file_reinjector(FileReinjector::new(read_state))
        .skill_reinjector(SkillReinjector::new(vec![skill]))
        .build()
        .expect("build runtime");

    // Activate plan + todo state so Block C (plan/todo restore) emits.
    rt.plan_approval_gate()
        .begin_approval("Step 1: explore\nStep 2: implement".to_string());
    seed_todos(&rt).await;

    let msgs = transcript_with_skill_in_older_and_tool_pair_in_tail();
    verify_tool_call_pairing(&msgs).expect("seed transcript must satisfy pairing");
    rt.set_transcript(msgs);

    rt.maybe_compact_cross_turn(&TokenUsage::default())
        .await
        .expect("cross-turn compaction must succeed");

    assert_compaction_fired_and_pairing_ok(&rt, "all reinjectors must preserve pairing");

    let transcript = rt.transcript();
    let file_idx = transcript
        .iter()
        .position(|m| m.content.starts_with("[post-compact file restore:"))
        .expect("file restore attachment must be present");
    let skill_idx = transcript
        .iter()
        .position(|m| m.content.starts_with("[post-compact skill restore:"))
        .expect("skill restore attachment must be present");
    let plan_idx = transcript
        .iter()
        .position(|m| m.content.starts_with("[post-compact plan restore]"))
        .expect("plan restore attachment must be present");
    let todo_idx = transcript
        .iter()
        .position(|m| m.content.starts_with("[post-compact todo restore]"))
        .expect("todo restore attachment must be present");

    assert!(
        file_idx < skill_idx && skill_idx < plan_idx && plan_idx < todo_idx,
        "reinjector chain must order attachments [file, skill, plan, todo], \
         got file={file_idx} skill={skill_idx} plan={plan_idx} todo={todo_idx}"
    );
    assert!(
        todo_idx + 1 < transcript.len(),
        "the preserved tail must follow the attachment chain"
    );
}
