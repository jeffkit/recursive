use super::*;
use crate::hooks::HookRegistry;
use crate::llm::{Completion, MockProvider};
use crate::tools::plan_mode::{ENTER_PLAN_MODE_TOOL_NAME, EXIT_PLAN_MODE_TOOL_NAME};
use crate::tools::todo::TodoStatus;
use crate::tools::Tool;
use crate::tools::ToolRegistry;
use async_trait::async_trait;
use serde_json::{json, Value};

struct Adder;

#[async_trait]
impl Tool for Adder {
    fn spec(&self) -> crate::llm::ToolSpec {
        crate::llm::ToolSpec {
            name: "add".into(),
            description: "add two numbers".into(),
            parameters: json!({"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}}}),
        }
    }
    async fn execute(&self, args: Value) -> crate::error::Result<String> {
        let a = args["a"].as_i64().unwrap_or(0);
        let b = args["b"].as_i64().unwrap_or(0);
        Ok((a + b).to_string())
    }
}

// ── basic turn execution ──────────────────────────────────────────

#[tokio::test]
async fn single_turn_no_tools() {
    let llm = Arc::new(MockProvider::new(vec![Completion {
        content: "Hello!".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }]));
    let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();
    let out = rt.run("hi").await.unwrap();
    assert_eq!(out.final_text.as_deref(), Some("Hello!"));
    assert_eq!(out.steps, 1);
    assert_eq!(rt.transcript().len(), 2); // user + assistant
}

#[tokio::test]
async fn turn_with_tool() {
    let llm = Arc::new(MockProvider::new(vec![
        Completion {
            content: "Let me check...".into(),
            tool_calls: vec![crate::llm::ToolCall {
                id: "c1".into(),
                name: "add".into(),
                arguments: json!({"a": 3, "b": 4}),
            }],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
        Completion {
            content: "7".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
    ]));
    let tools = ToolRegistry::local().register(Arc::new(Adder));
    let mut rt = AgentRuntime::builder()
        .llm(llm)
        .tools(tools)
        .build()
        .unwrap();
    let out = rt.run("3+4?").await.unwrap();
    assert_eq!(out.final_text.as_deref(), Some("7"));
    assert_eq!(out.steps, 2);
    assert_eq!(rt.transcript().len(), 4); // user, assistant, tool, assistant
}

// ── transcript accumulation across turns ──────────────────────────

#[tokio::test]
async fn multi_turn_transcript_grows() {
    let llm = Arc::new(MockProvider::new(vec![
        Completion {
            content: "First reply".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
        Completion {
            content: "Second reply".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
    ]));
    let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();

    let o1 = rt.run("turn 1").await.unwrap();
    assert_eq!(o1.final_text.as_deref(), Some("First reply"));
    assert_eq!(rt.transcript().len(), 2);

    let o2 = rt.run("turn 2").await.unwrap();
    assert_eq!(o2.final_text.as_deref(), Some("Second reply"));
    assert_eq!(rt.transcript().len(), 4);
}

// ── builder options ───────────────────────────────────────────────

#[tokio::test]
async fn system_prompt_is_prepended() {
    let llm = Arc::new(MockProvider::new(vec![Completion {
        content: "ok".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }]));
    let mut rt = AgentRuntime::builder()
        .llm(llm)
        .system_prompt("Be helpful.")
        .build()
        .unwrap();
    rt.run("hello").await.unwrap();
    assert_eq!(rt.transcript()[0].role, crate::message::Role::System);
    assert_eq!(rt.transcript()[0].content, "Be helpful.");
}

#[tokio::test]
async fn seed_transcript_is_included() {
    let seed = vec![
        Message::user("old Q".to_string()),
        Message::assistant("old A".to_string()),
    ];
    let llm = Arc::new(MockProvider::new(vec![Completion {
        content: "fresh".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }]));
    let mut rt = AgentRuntime::builder()
        .llm(llm)
        .seed_transcript(seed)
        .build()
        .unwrap();
    rt.run("new Q").await.unwrap();
    // seed(2) + new user + new assistant = 4
    assert_eq!(rt.transcript().len(), 4);
    assert_eq!(rt.transcript()[0].content, "old Q");
    assert_eq!(rt.transcript()[1].content, "old A");
    assert_eq!(rt.transcript()[2].content, "new Q");
    assert_eq!(rt.transcript()[3].content, "fresh");
}

#[tokio::test]
async fn system_and_seed_ordering() {
    let seed = vec![Message::user("seeded user".to_string())];
    let llm = Arc::new(MockProvider::new(vec![Completion {
        content: "r".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }]));
    let mut rt = AgentRuntime::builder()
        .llm(llm)
        .system_prompt("sys prompt")
        .seed_transcript(seed)
        .build()
        .unwrap();
    rt.run("real").await.unwrap();
    assert_eq!(rt.transcript()[0].role, crate::message::Role::System);
    assert_eq!(rt.transcript()[0].content, "sys prompt");
    assert_eq!(rt.transcript()[1].content, "seeded user");
    assert_eq!(rt.transcript()[2].content, "real");
}

// ── state inspection / mutation ───────────────────────────────────

#[tokio::test]
async fn set_transcript_replaces() {
    let llm = Arc::new(MockProvider::new(vec![Completion {
        content: "ok".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }]));
    let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();
    rt.set_transcript(vec![Message::user("custom".to_string())]);
    assert_eq!(rt.transcript().len(), 1);
    assert_eq!(rt.transcript()[0].content, "custom");
}

#[tokio::test]
async fn kernel_accessor_works() {
    let llm = Arc::new(MockProvider::new(vec![Completion {
        content: "ok".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }]));
    let rt = AgentRuntime::builder().llm(llm).build().unwrap();
    let _kernel = rt.kernel(); // should compile and return a reference
}

// ── default values ────────────────────────────────────────────────

#[tokio::test]
async fn defaults_are_sensible() {
    let llm = Arc::new(MockProvider::new(vec![Completion {
        content: "done".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }]));
    let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();
    let out = rt.run("test").await.unwrap();
    assert_eq!(out.finish_reason, FinishReason::NoMoreToolCalls);
    assert_eq!(rt.transcript().len(), 2);
}

// ── checkpoint integration ────────────────────────────────────────

fn has_git() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok()
}

/// Workspace tempdir + sibling shadow tempdir, both alive together.
/// Tests open `ShadowRepo::open_at(...)` against `shadow_dir()` to
/// avoid touching `paths::user_data_dir()` and the global env lock.
struct ShadowWs {
    workspace: tempfile::TempDir,
    shadow: tempfile::TempDir,
}

impl ShadowWs {
    fn path(&self) -> &std::path::Path {
        self.workspace.path()
    }
    fn shadow_dir(&self) -> std::path::PathBuf {
        self.shadow.path().join("shadow-git")
    }
}

fn shadow_ws() -> ShadowWs {
    ShadowWs {
        workspace: tempfile::tempdir().expect("workspace tempdir"),
        shadow: tempfile::tempdir().expect("shadow tempdir"),
    }
}

/// Goal 284: with on-demand checkpoints, automatic per-turn snapshots
/// are gone. Verify that `outcome.checkpoint_id` is `None` and no
/// log entries are written automatically. The agent must call
/// `checkpoint_save` to persist a checkpoint.
#[tokio::test]
async fn runtime_no_auto_snapshots_with_checkpoints_enabled() {
    if !has_git() {
        return;
    }
    let dir = shadow_ws();
    std::fs::write(dir.path().join("seed.txt"), "v0").unwrap();

    let llm = Arc::new(MockProvider::new(vec![
        Completion {
            content: "ok".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
        Completion {
            content: "ok2".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
    ]));
    let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();

    let shadow = Arc::new(crate::ShadowRepo::open_at(dir.path(), dir.shadow_dir()).unwrap());
    let log_path = dir.path().join("checkpoints.jsonl");
    rt.enable_checkpoints(shadow.clone(), "sess", log_path.clone(), None)
        .unwrap();
    assert!(rt.checkpoints_enabled());

    let o1 = rt.run("turn 0").await.unwrap();
    assert!(o1.checkpoint_id.is_none(), "no auto-snapshot in Goal 284");
    let o2 = rt.run("turn 1").await.unwrap();
    assert!(o2.checkpoint_id.is_none(), "no auto-snapshot in Goal 284");

    // No log entries should exist (agent never called checkpoint_save).
    let recs = crate::read_checkpoint_log(&log_path).unwrap();
    assert_eq!(recs.len(), 0, "no auto log entries");
}

/// Goal 284: verify that `checkpoint_save` tool is registered
/// when checkpoints are enabled.
#[tokio::test]
async fn checkpoint_save_tool_is_registered() {
    if !has_git() {
        return;
    }
    let dir = shadow_ws();
    std::fs::write(dir.path().join("a.txt"), "hi").unwrap();

    let llm = Arc::new(MockProvider::new(vec![Completion {
        content: "ok".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }]));
    let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();

    let shadow = Arc::new(crate::ShadowRepo::open_at(dir.path(), dir.shadow_dir()).unwrap());
    let log_path = dir.path().join("checkpoints.jsonl");
    rt.enable_checkpoints(shadow, "sess", log_path, None)
        .unwrap();

    let tools = rt.kernel.tools();
    assert!(
        tools.get("checkpoint_save").is_some(),
        "checkpoint_save must be registered"
    );
    assert!(
        tools.get("checkpoint_list").is_some(),
        "checkpoint_list must be registered"
    );
    assert!(
        tools.get("checkpoint_diff").is_some(),
        "checkpoint_diff must be registered"
    );
}

#[tokio::test]
async fn runtime_works_when_checkpoints_disabled() {
    // No call to enable_checkpoints → outcome.checkpoint_id is None,
    // no log file created.
    let llm = Arc::new(MockProvider::new(vec![Completion {
        content: "ok".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }]));
    let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();
    let out = rt.run("hi").await.unwrap();
    assert!(out.checkpoint_id.is_none());
    assert!(!rt.checkpoints_enabled());
}

// ── compact_now (Goal 146) ────────────────────────────────────────────

#[tokio::test]
async fn compact_now_invokes_compactor() {
    // Provider used (a) to answer two normal turns, (b) to answer
    // the compactor's "summarize" call.
    let llm = Arc::new(MockProvider::new(vec![
        Completion {
            content: "first reply".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
        Completion {
            content: "second reply".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
        Completion {
            content: "compacted summary".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
    ]));
    // Threshold = MAX so the auto-compaction in `run` never fires;
    // keep_recent_n=1 so we only need 3 messages before compact_now
    // has work to do.
    let compactor = crate::compact::Compactor::new(usize::MAX).keep_recent_n(1);
    let mut rt = AgentRuntime::builder()
        .llm(llm)
        .compactor(compactor)
        .build()
        .unwrap();
    rt.run("turn 1").await.unwrap();
    rt.run("turn 2").await.unwrap();
    let len_before = rt.transcript().len();
    assert!(len_before >= 3, "expected ≥3 messages, got {len_before}");

    rt.compact_now().await.unwrap();
    // The compactor replaces older messages with one summary
    // system message plus keep_recent_n=1 verbatim message.
    assert_eq!(rt.transcript().len(), 2);
    assert_eq!(rt.transcript()[0].role, crate::message::Role::System);
    assert!(rt.transcript()[0].content.starts_with("[compacted:"));
}

#[tokio::test]
async fn compact_now_is_noop_without_compactor() {
    let llm = Arc::new(MockProvider::new(vec![Completion {
        content: "x".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }]));
    let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();
    rt.run("hi").await.unwrap();
    let before = rt.transcript().len();
    rt.compact_now().await.unwrap();
    assert_eq!(rt.transcript().len(), before);
}

// ── Goal-305: turn index propagated to compaction summary header ──

#[tokio::test]
async fn compact_now_uses_turn_index_in_header() {
    let llm = Arc::new(MockProvider::new(vec![
        Completion {
            content: "first reply".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
        Completion {
            content: "second reply".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
        Completion {
            content: "compacted summary text".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
    ]));
    // keep_recent_n = 1 → transcript after compaction is [summary, last message].
    // The summary is always at index 0, so we can inspect its header.
    let compactor = crate::compact::Compactor::new(usize::MAX).keep_recent_n(1);
    let mut rt = AgentRuntime::builder()
        .llm(llm)
        .compactor(compactor)
        .build()
        .unwrap();

    // 2 turns → turn_index advances to 2.
    rt.run("turn 1").await.unwrap();
    rt.run("turn 2").await.unwrap();
    assert_eq!(rt.turn_index(), 2, "turn_index should be 2 after 2 turns");

    rt.compact_now().await.unwrap();

    // Transcript: [compaction summary, last verbatim message].
    assert_eq!(rt.transcript().len(), 2);
    let summary = &rt.transcript()[0].content;
    assert!(
        summary.contains("at step 2"),
        "compaction header should contain 'at step 2', got: {summary}"
    );
}

// ── Goal-342: compact_partial_before / compact_partial_after ─────────

#[tokio::test]
async fn compact_partial_before_summarizes_prefix_keeps_suffix() {
    let provider = Arc::new(MockProvider::new(vec![Completion {
        content: "partial summary before pivot".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }]));
    let compactor = crate::compact::Compactor::new(usize::MAX).keep_recent_n(2);
    let mut rt = AgentRuntime::builder()
        .llm(provider)
        .compactor(compactor)
        .build()
        .unwrap();
    // Seed: [system, user0, asst0, user1, asst1, user2, asst2]
    let msgs = vec![
        Message::system("sys"),
        Message::user("u0"),
        Message::assistant("a0"),
        Message::user("u1"),
        Message::assistant("a1"),
        Message::user("u2"),
        Message::assistant("a2"),
    ];
    *Arc::make_mut(&mut rt.transcript) = msgs;

    // Compact before index 5 (u2). The exact split depends on
    // safe_split_point; assert the invariants that matter: compaction
    // happened (transcript shrank, a summary now leads), and the tail
    // (pivot and after) is preserved verbatim.
    let len_before = rt.transcript().len();
    rt.compact_partial_before(5).await.unwrap();

    let after = rt.transcript();
    assert!(
        after.len() < len_before,
        "compact_partial_before must shrink the transcript: {} -> {}",
        len_before,
        after.len()
    );
    assert!(
        after[0].content.starts_with("[compacted:"),
        "summary must lead the transcript after compact_partial_before"
    );
    // The pivot (index 5 = "u2") and the message after it (a2) must be
    // preserved verbatim in the tail.
    assert!(
        after.iter().any(|m| m.content == "u2"),
        "pivot message u2 must survive in the tail"
    );
    assert!(
        after.iter().any(|m| m.content == "a2"),
        "message after pivot (a2) must survive in the tail"
    );
}

#[tokio::test]
async fn compact_partial_after_summarizes_suffix_keeps_prefix() {
    let provider = Arc::new(MockProvider::new(vec![Completion {
        content: "partial summary after pivot".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }]));
    let compactor = crate::compact::Compactor::new(usize::MAX).keep_recent_n(2);
    let mut rt = AgentRuntime::builder()
        .llm(provider)
        .compactor(compactor)
        .build()
        .unwrap();
    // Seed: [system, user0, asst0, user1, asst1, user2, asst2]
    let msgs = vec![
        Message::system("sys"),
        Message::user("u0"),
        Message::assistant("a0"),
        Message::user("u1"),
        Message::assistant("a1"),
        Message::user("u2"),
        Message::assistant("a2"),
    ];
    *Arc::make_mut(&mut rt.transcript) = msgs;

    // Compact after index 3 (u1). The suffix from the pivot onward is
    // summarised; assert invariants: transcript shrank, the prefix before
    // the pivot is preserved verbatim, and a summary now closes it.
    let len_before = rt.transcript().len();
    rt.compact_partial_after(3).await.unwrap();

    let after = rt.transcript();
    assert!(
        after.len() < len_before,
        "compact_partial_after must shrink the transcript: {} -> {}",
        len_before,
        after.len()
    );
    // Prefix before index 3 ([sys, u0, a0]) must be preserved verbatim.
    assert_eq!(after[0].content, "sys");
    assert_eq!(after[1].content, "u0");
    assert_eq!(after[2].content, "a0");
    // The last message must be the compaction summary.
    assert!(
        after.last().unwrap().content.starts_with("[compacted:"),
        "summary must be the last message after compact_partial_after"
    );
}

#[tokio::test]
async fn compact_partial_too_short_is_noop() {
    let provider = Arc::new(MockProvider::new(vec![Completion {
        content: "summary".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }]));
    let compactor = crate::compact::Compactor::new(usize::MAX).keep_recent_n(2);
    let mut rt = AgentRuntime::builder()
        .llm(provider)
        .compactor(compactor)
        .build()
        .unwrap();
    // Very short transcript: just a system prompt.
    *Arc::make_mut(&mut rt.transcript) = vec![Message::system("sys")];

    let before = rt.transcript().len();
    rt.compact_partial_before(0).await.unwrap();
    assert_eq!(rt.transcript().len(), before);

    rt.compact_partial_after(0).await.unwrap();
    assert_eq!(rt.transcript().len(), before);
}

#[tokio::test]
async fn compact_partial_preserves_tool_call_pairing() {
    let provider = Arc::new(MockProvider::new(vec![Completion {
        content: "tool-pair-summary".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }]));
    let compactor = crate::compact::Compactor::new(usize::MAX).keep_recent_n(1);
    let mut rt = AgentRuntime::builder()
        .llm(provider)
        .compactor(compactor)
        .build()
        .unwrap();

    // Messages with a tool-call pair: [u0, a0(tool_calls), tool0, u1, a1]
    let tc = crate::llm::ToolCall {
        id: "c1".into(),
        name: "add".into(),
        arguments: serde_json::json!({}),
    };
    let msgs = vec![
        Message::user("u0"),
        Message::assistant_with_tool_calls("a0", vec![tc]),
        Message::tool_result("call_c1", "3"),
        Message::user("u1"),
        Message::assistant("a1"),
    ];
    *Arc::make_mut(&mut rt.transcript) = msgs;

    // compact_partial_before at index 3 (u1). safe_split_point backs up
    // past Tool at index 2 in scope[..=3] → split=2. Older [0..2] compacted.
    rt.compact_partial_before(3).await.unwrap();

    // After: [summary, u1, a1] — no orphan Tool messages.
    assert_eq!(rt.transcript().len(), 3, "summary + 2 kept");
    assert!(rt.transcript()[0].content.starts_with("[compacted:"));
    assert_eq!(rt.transcript()[1].content, "u1");
    assert_eq!(rt.transcript()[2].content, "a1");
    // No orphan Tool messages in the kept region
    for msg in &rt.transcript()[1..] {
        if msg.role == crate::message::Role::Tool {
            panic!("orphan Tool message in kept region: {:?}", msg);
        }
    }
}

#[tokio::test]
async fn compact_partial_noop_without_compactor() {
    let provider = Arc::new(MockProvider::new(vec![Completion {
        content: "x".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }]));
    let mut rt = AgentRuntime::builder().llm(provider).build().unwrap();
    rt.set_transcript(vec![Message::user("hi"), Message::assistant("ok")]);
    let before = rt.transcript().len();
    rt.compact_partial_before(1).await.unwrap();
    assert_eq!(rt.transcript().len(), before);
    rt.compact_partial_after(0).await.unwrap();
    assert_eq!(rt.transcript().len(), before);
}

// ── Goal-168: GoalState / GoalEvaluator / run_goal_loop tests ──────────

#[tokio::test]
async fn set_goal_stores_state() {
    let llm = Arc::new(MockProvider::new(vec![]));
    let rt = AgentRuntime::builder().llm(llm).build().unwrap();
    assert!(rt.current_goal().is_none());
    rt.set_goal("task is done".to_string(), 10).await;
    let g = rt.current_goal().expect("goal should be set");
    assert_eq!(g.condition, "task is done");
    assert_eq!(g.max_turns, 10);
    assert_eq!(g.turns, 0);
    assert_eq!(g.status, GoalStatus::Pursuing);
}

#[tokio::test]
async fn clear_goal_removes_state() {
    let llm = Arc::new(MockProvider::new(vec![]));
    let rt = AgentRuntime::builder().llm(llm).build().unwrap();
    rt.set_goal("anything".to_string(), 5).await;
    assert!(rt.current_goal().is_some());
    rt.clear_goal().await;
    assert!(rt.current_goal().is_none());
}

#[tokio::test]
async fn goal_status_default_is_pursuing() {
    let g = GoalState {
        condition: "done".to_string(),
        status: GoalStatus::Pursuing,
        turns: 0,
        max_turns: 20,
        last_reason: None,
    };
    assert_eq!(g.status, GoalStatus::Pursuing);
    assert_eq!(g.turns, 0);
    assert!(g.last_reason.is_none());
}

#[tokio::test]
async fn goal_evaluator_returns_achieved_on_yes_response() {
    // Mock a provider that returns "YES\nLooks complete."
    let llm = Arc::new(MockProvider::new(vec![Completion {
        content: "YES\nLooks complete.".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }]));
    let evaluator = GoalEvaluator::new(llm);
    let msgs = vec![crate::message::Message::user("I completed the task.")];
    let verdict = evaluator
        .evaluate("task is done", &msgs)
        .await
        .expect("evaluate should succeed");
    assert!(verdict.achieved);
    assert!(!verdict.reason.is_empty());
}

#[tokio::test]
async fn goal_evaluator_returns_not_achieved_on_no_response() {
    let llm = Arc::new(MockProvider::new(vec![Completion {
        content: "NO\nStill in progress.".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }]));
    let evaluator = GoalEvaluator::new(llm);
    let msgs = vec![crate::message::Message::user("I started the task.")];
    let verdict = evaluator
        .evaluate("task is done", &msgs)
        .await
        .expect("evaluate should succeed");
    assert!(!verdict.achieved);
}

#[tokio::test]
async fn goal_evaluator_tolerates_empty_transcript() {
    let llm = Arc::new(MockProvider::new(vec![Completion {
        content: "YES\nEmpty transcript but condition trivially met.".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }]));
    let evaluator = GoalEvaluator::new(llm);
    let verdict = evaluator
        .evaluate("anything", &[])
        .await
        .expect("should not error on empty transcript");
    assert!(verdict.achieved);
}

#[tokio::test]
async fn run_goal_loop_stops_when_achieved() {
    // Provider: first call for the agent turn, second for the judge.
    let llm = Arc::new(MockProvider::new(vec![
        Completion {
            content: "I wrote the greeting.".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
        Completion {
            content: "YES\nGreeting was written.".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
    ]));
    let null_sink = Arc::new(crate::event::NullSink);
    let mut rt = AgentRuntime::builder()
        .llm(llm)
        .event_sink(null_sink)
        .build()
        .unwrap();
    let _ = rt
        .run_goal_loop("write a greeting", "write a greeting", 5)
        .await;
    // Goal should be cleared after achievement.
    assert!(rt.current_goal().is_none());
}

#[tokio::test]
async fn run_goal_loop_stops_at_max_turns() {
    // Provider: every judge call returns NO → loop hits max_turns.
    let completions: Vec<Completion> = (0..20)
        .flat_map(|_| {
            vec![
                Completion {
                    content: "still working".into(),
                    tool_calls: vec![],
                    finish_reason: Some("stop".into()),
                    usage: None,
                    reasoning_content: None,
                },
                Completion {
                    content: "NO\nNot done yet.".into(),
                    tool_calls: vec![],
                    finish_reason: Some("stop".into()),
                    usage: None,
                    reasoning_content: None,
                },
            ]
        })
        .collect();
    let llm = Arc::new(MockProvider::new(completions));
    let null_sink = Arc::new(crate::event::NullSink);
    let mut rt = AgentRuntime::builder()
        .llm(llm)
        .event_sink(null_sink)
        .build()
        .unwrap();
    // max_turns=2 so we stop after 2 regardless.
    let _ = rt
        .run_goal_loop("start on impossible task", "impossible task", 2)
        .await;
    // Goal should be cleared after budget exhaustion.
    assert!(rt.current_goal().is_none());
}

#[tokio::test]
async fn goal_serde_round_trip() {
    let g = GoalState {
        condition: "file written".to_string(),
        status: GoalStatus::Achieved,
        turns: 3,
        max_turns: 10,
        last_reason: Some("File was created.".to_string()),
    };
    let json = serde_json::to_string(&g).expect("serialize");
    let g2: GoalState = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(g2.condition, g.condition);
    assert_eq!(g2.status, GoalStatus::Achieved);
    assert_eq!(g2.turns, 3);
    assert_eq!(g2.last_reason, Some("File was created.".to_string()));
}

#[tokio::test]
async fn multiple_set_goal_calls_overwrite_state() {
    let llm = Arc::new(MockProvider::new(vec![]));
    let rt = AgentRuntime::builder().llm(llm).build().unwrap();
    rt.set_goal("first goal".to_string(), 5).await;
    rt.set_goal("second goal".to_string(), 15).await;
    let g = rt.current_goal().unwrap();
    assert_eq!(g.condition, "second goal");
    assert_eq!(g.max_turns, 15);
}

// ── Goal-260: transcript_tail accessor ───────────────────────────────

#[test]
fn transcript_tail_returns_full_when_n_exceeds_len() -> Result<(), Box<dyn std::error::Error>> {
    let llm = Arc::new(MockProvider::new(vec![]));
    let mut rt = AgentRuntime::builder().llm(llm).build()?;
    // Build a 3-message transcript directly (no LLM calls).
    rt.set_transcript(vec![
        crate::message::Message::user("one"),
        crate::message::Message::assistant("two"),
        crate::message::Message::user("three"),
    ]);
    let tail = rt.transcript_tail(10);
    assert_eq!(tail.len(), 3, "n > len should return the full transcript");
    assert_eq!(tail[0].content, "one");
    assert_eq!(tail[2].content, "three");
    Ok(())
}

#[test]
fn transcript_tail_returns_last_n() -> Result<(), Box<dyn std::error::Error>> {
    let llm = Arc::new(MockProvider::new(vec![]));
    let mut rt = AgentRuntime::builder().llm(llm).build()?;
    rt.set_transcript(vec![
        crate::message::Message::user("m0"),
        crate::message::Message::assistant("m1"),
        crate::message::Message::user("m2"),
        crate::message::Message::assistant("m3"),
        crate::message::Message::user("m4"),
    ]);
    let tail = rt.transcript_tail(2);
    assert_eq!(tail.len(), 2, "should return exactly the last 2 messages");
    assert_eq!(tail[0].content, "m3");
    assert_eq!(tail[1].content, "m4");
    Ok(())
}

#[test]
fn transcript_tail_handles_zero() -> Result<(), Box<dyn std::error::Error>> {
    let llm = Arc::new(MockProvider::new(vec![]));
    let mut rt = AgentRuntime::builder().llm(llm).build()?;
    rt.set_transcript(vec![
        crate::message::Message::user("only"),
        crate::message::Message::assistant("reply"),
    ]);
    let tail = rt.transcript_tail(0);
    assert_eq!(tail.len(), 0, "n == 0 should return an empty slice");
    assert!(tail.is_empty());
    Ok(())
}

// ── Goal-291: configurable goal_eval_transcript_tail ──────────────────
//
// The `goal_eval_transcript_tail` field replaces the old
// `GOAL_EVAL_TRANSCRIPT_TAIL` constant. We verify three things:
//   1. The builder field is wired through to the runtime.
//   2. The default stays at 12 (backward-compatible with sessions that
//      don't set the field).
//   3. The configured value is honored, not silently overwritten by
//      the old constant.
#[test]
fn goal_eval_transcript_tail_builder_default_is_twelve() {
    let llm = Arc::new(MockProvider::new(vec![]));
    let rt = AgentRuntime::builder().llm(llm).build().unwrap();
    // The default is 12 — same value the old constant held.
    assert_eq!(rt.goal_eval_transcript_tail, 12);
}

#[test]
fn goal_eval_transcript_tail_builder_override_propagates() {
    let llm = Arc::new(MockProvider::new(vec![]));
    let rt = AgentRuntime::builder()
        .llm(llm)
        .goal_eval_transcript_tail(3)
        .build()
        .unwrap();
    assert_eq!(rt.goal_eval_transcript_tail, 3);
}

/// Source-level check: with `goal_eval_transcript_tail = 3` and 6
/// messages in the transcript, `transcript_tail(n)` returns 3 — the
/// value the runtime would pass to the judge. Verifies the value
/// is honored, not silently overwritten by the old constant.
#[test]
fn goal_eval_transcript_tail_honored_over_old_default() {
    let llm = Arc::new(MockProvider::new(vec![]));
    let mut rt = AgentRuntime::builder()
        .llm(llm)
        .goal_eval_transcript_tail(3)
        .build()
        .unwrap();
    // 6 messages: u0, a0, u1, a1, u2, a2.
    rt.set_transcript(vec![
        crate::message::Message::user("m0"),
        crate::message::Message::assistant("m1"),
        crate::message::Message::user("m2"),
        crate::message::Message::assistant("m3"),
        crate::message::Message::user("m4"),
        crate::message::Message::assistant("m5"),
    ]);
    // The judge slice should be exactly 3 (the configured value),
    // not 6 (full transcript) and not 12 (old default).
    let judge_slice = rt.transcript_tail(rt.goal_eval_transcript_tail);
    assert_eq!(
        judge_slice.len(),
        3,
        "judge should see only 3 messages, got {}",
        judge_slice.len()
    );
    assert_eq!(judge_slice[0].content, "m3");
    assert_eq!(judge_slice[1].content, "m4");
    assert_eq!(judge_slice[2].content, "m5");
}

/// End-to-end check: with `goal_eval_transcript_tail = 1` the loop
/// runs to completion (no panic, no wrong-tail-length error) using
/// the configured tail size. This validates the wiring change in
/// `run_goal_loop` — it now reads from the field, not the constant.
#[tokio::test]
async fn run_goal_loop_respects_tail_config() {
    use crate::event::ChannelSink;

    let completions = vec![
        // First turn: agent reply
        Completion {
            content: "still working".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
        // First judge call: NO
        Completion {
            content: "NO\nNot done yet.".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
        // Second turn: agent reply
        Completion {
            content: "trying again".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
        // Second judge call: YES — loop should exit here
        Completion {
            content: "YES\nAll good.".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
    ];
    let llm = Arc::new(MockProvider::new(completions));
    let (sink, _rx) = ChannelSink::new();
    let mut rt = AgentRuntime::builder()
        .llm(llm)
        .event_sink(Arc::new(sink))
        .goal_eval_transcript_tail(1)
        .build()
        .unwrap();

    // With tail=1, the judge sees only the most recent message per
    // call. This test just verifies the loop runs to completion
    // (no panic, no wrong-tail-length error) using the configured
    // tail size.
    let _ = rt
        .run_goal_loop("achieve it", "achieve it", 5)
        .await
        .expect("goal loop should run without error");
    // Goal is cleared on achievement.
    assert!(rt.current_goal().is_none());
    assert_eq!(rt.goal_eval_transcript_tail, 1);
}

// ── Goal-181: message queue ───────────────────────────────────────────

#[tokio::test]
async fn enqueue_processes_single_message() {
    let llm = Arc::new(MockProvider::new(vec![Completion {
        content: "queued reply".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }]));
    let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();
    let out = rt.enqueue("hello from queue").await.unwrap();
    assert!(out.is_some());
    assert_eq!(out.unwrap().final_text.as_deref(), Some("queued reply"));
    assert_eq!(rt.transcript().len(), 2);
}

#[tokio::test]
async fn enqueue_drains_multiple_messages_in_order() {
    let llm = Arc::new(MockProvider::new(vec![
        Completion {
            content: "reply A".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
        Completion {
            content: "reply B".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
    ]));
    let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();
    // Push two messages directly into the queue to simulate concurrent enqueue.
    rt.message_queue.push_back("msg A".into());
    rt.message_queue.push_back("msg B".into());
    let last = rt.drain_queue().await.unwrap();
    assert_eq!(last.unwrap().final_text.as_deref(), Some("reply B"));
    // Both user messages + both assistant replies are in transcript.
    assert_eq!(rt.transcript().len(), 4);
}

#[test]
fn queue_len_reflects_pending_messages() {
    let llm = Arc::new(MockProvider::new(vec![]));
    let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();
    assert_eq!(rt.queue_len(), 0);
    rt.message_queue.push_back("pending".into());
    assert_eq!(rt.queue_len(), 1);
    rt.message_queue.push_back("also pending".into());
    assert_eq!(rt.queue_len(), 2);
}

// ── Goal-244: drain_queue error propagation ──

#[tokio::test]
async fn drain_queue_returns_ok_for_all_messages() {
    let llm = Arc::new(MockProvider::new(vec![
        Completion {
            content: "reply A".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
        Completion {
            content: "reply B".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
    ]));
    let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();
    rt.message_queue.push_back("msg A".into());
    rt.message_queue.push_back("msg B".into());
    let result = rt.drain_queue().await;
    assert!(result.is_ok());
    let last = result.unwrap();
    assert!(last.is_some());
    assert_eq!(last.unwrap().final_text.as_deref(), Some("reply B"));
    // Both user messages + both assistant replies are in transcript.
    assert_eq!(rt.transcript().len(), 4);
}

#[tokio::test]
async fn drain_queue_stops_on_first_error() {
    // Only one completion available — second message will fail.
    let llm = Arc::new(MockProvider::new(vec![Completion {
        content: "reply A".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }]));
    let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();
    rt.message_queue.push_back("msg A".into());
    rt.message_queue.push_back("msg B".into());
    let result = rt.drain_queue().await;
    assert!(result.is_err(), "expected error, got {:?}", result);
    // Goal-259: the in-flight message must remain at the front of the
    // queue so it can be retried by calling drain_queue again.
    assert_eq!(
        rt.queue_len(),
        1,
        "second message should remain in queue for retry"
    );
    // Verify it is indeed the second message that was preserved.
    assert_eq!(
        rt.message_queue.front().map(String::as_str),
        Some("msg B"),
        "msg B should still be at the front of the queue"
    );
    // First message was successfully processed and is reflected in the
    // transcript (user message + assistant reply).
    assert_eq!(
        rt.transcript().len(),
        3,
        "transcript should hold msg A, reply A, and the in-flight msg B"
    );
}

#[tokio::test]
async fn drain_queue_preserves_remaining_messages_on_error() {
    // Goal-259: 3 messages queued, only 1 completion available. The
    // second message will fail. The first message must be popped
    // (success), and the remaining two (B and C) must stay in the
    // queue for later retry.
    let llm = Arc::new(MockProvider::new(vec![Completion {
        content: "reply A".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }]));
    let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();
    rt.message_queue.push_back("msg A".into());
    rt.message_queue.push_back("msg B".into());
    rt.message_queue.push_back("msg C".into());
    let result = rt.drain_queue().await;
    assert!(result.is_err(), "expected error, got {:?}", result);
    // First message was processed and popped; B and C remain.
    assert_eq!(
        rt.queue_len(),
        2,
        "B and C should remain in queue for retry"
    );
    // FIFO order preserved: B at the front, C behind it.
    assert_eq!(rt.message_queue.front().map(String::as_str), Some("msg B"));
    // First turn reflected in transcript. The in-flight msg B is also
    // present (run() appends the user message to the transcript before
    // the LLM call), but it has no assistant reply yet because the
    // LLM call failed — the same pre-existing behaviour as in
    // drain_queue_stops_on_first_error.
    assert_eq!(rt.transcript().len(), 3);
}

// ── Goal-201: plan mode tools are registered by the runtime builder ──

#[test]
fn runtime_builder_skills_stores_skills_list() {
    // kills `replace AgentRuntimeBuilder::skills -> Self with Default::default()`:
    // if skills() discards the argument, the runtime's globs_skills would be empty.
    use crate::skills::{Skill, SkillMode};
    let llm = Arc::new(MockProvider::new(vec![]));
    let skill = Skill {
        name: "my-skill".to_string(),
        description: "A test skill".to_string(),
        path: std::path::PathBuf::from("/tmp/my-skill/SKILL.md"),
        mode: SkillMode::Always,
        triggers: vec![],
        hint: String::new(),
        depends_on: vec![],
        refs: vec![],
        params: vec![],
        scripts: vec![],
        sections: vec![],
        globs: None,
        body: None,
    };
    let rt = AgentRuntime::builder()
        .llm(llm)
        .skills(vec![skill])
        .build()
        .unwrap();
    // `globs_skills` is pub(crate); it must contain the skill we passed.
    assert_eq!(
        rt.kernel.globs_skills.len(),
        1,
        "skills() must store the provided skills list; len was {}",
        rt.kernel.globs_skills.len()
    );
    assert_eq!(rt.kernel.globs_skills[0].name, "my-skill");
}

#[test]
fn reject_plan_appends_rejection_message_to_transcript() {
    // kills `replace AgentRuntime::reject_plan with ()` mutation.
    // If reject_plan is a no-op, the transcript won't grow.
    let llm = Arc::new(MockProvider::new(vec![]));
    let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();
    let before = rt.transcript_tail(100).len();
    rt.reject_plan("too risky");
    let after = rt.transcript_tail(100).len();
    assert!(
        after > before,
        "reject_plan must append a rejection message to the transcript"
    );
    // Verify the message contains the reason
    let tail = rt.transcript_tail(100);
    let last = tail.last().expect("at least one message");
    assert!(
        last.content.contains("too risky"),
        "rejection message must contain the provided reason; got: {:?}",
        last.content
    );
}

#[test]
fn runtime_builder_has_plan_mode_tools() {
    // AgentRuntimeBuilder::build() must register enter_plan_mode and
    // exit_plan_mode when with_plan_mode_tools(true) is set.
    // These are channel capabilities used by the TUI and HTTP paths.
    let llm = Arc::new(MockProvider::new(vec![]));
    let rt = AgentRuntime::builder()
        .llm(llm)
        .with_plan_mode_tools(true)
        .build()
        .unwrap();
    let tools = rt.kernel.tools();
    assert!(
        tools.get(ENTER_PLAN_MODE_TOOL_NAME).is_some(),
        "enter_plan_mode must be registered by AgentRuntimeBuilder"
    );
    assert!(
        tools.get(EXIT_PLAN_MODE_TOOL_NAME).is_some(),
        "exit_plan_mode must be registered by AgentRuntimeBuilder"
    );
}

// ── Goal-275: tool_audits keyed by (turn, tool_call_id) ──────────────

/// When two turns reuse the same `tool_call_id`, the new `(turn, id)`
/// keying prevents the second turn's audit from overwriting the first
/// turn's audit before it can be emitted.
#[tokio::test]
async fn audit_survives_collision_across_turns() {
    let llm = Arc::new(MockProvider::new(vec![
        // Turn 1: tool call "c1" (adder)
        Completion {
            content: "calculating...".into(),
            tool_calls: vec![crate::llm::ToolCall {
                id: "c1".into(),
                name: "add".into(),
                arguments: json!({"a": 1, "b": 2}),
            }],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
        // Turn 1: finish
        Completion {
            content: "3".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
        // Turn 2: tool call "c1" (SAME id reused)
        Completion {
            content: "calculating again...".into(),
            tool_calls: vec![crate::llm::ToolCall {
                id: "c1".into(),
                name: "add".into(),
                arguments: json!({"a": 5, "b": 7}),
            }],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
        // Turn 2: finish
        Completion {
            content: "12".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
    ]));
    let tools = ToolRegistry::local().register(Arc::new(Adder));
    let (sink, mut rx) = crate::event::ChannelSink::new();
    let sink_arc = Arc::new(sink);
    let mut rt = AgentRuntime::builder()
        .llm(llm)
        .tools(tools)
        .event_sink(sink_arc)
        .build()
        .unwrap();

    // Drain events from builder registration.
    while let Ok(_ev) = rx.try_recv() {}

    let _ = rt.run("turn 1").await.unwrap();
    let _ = rt.run("turn 2").await.unwrap();

    let mut audit_count = 0usize;
    while let Ok(ev) = rx.try_recv() {
        if matches!(ev, AgentEvent::MessageAppendedWithAudit { .. }) {
            audit_count += 1;
        }
    }

    // Both turns should produce a tool result with audit metadata.
    // Without (turn, id) keying, turn-2's audit overwrites turn-1's
    // entry before emit_turn_messages processes either, so
    // audit_count would be 1 instead of 2.
    assert_eq!(
        audit_count, 2,
        "expected both turns' tool results to have audit metadata"
    );
}

/// A buggy model that emits the same `tool_call_id` twice in a single
/// assistant message.  The `remove()` semantics mean only the first
/// tool-result message gets the audit, but at least it gets *one*.
/// Before the (turn, id) keying fix, cross-turn collisions could
/// nuke even this one.
#[tokio::test]
async fn duplicate_tool_call_id_in_same_response_attaches_at_least_one() {
    let llm = Arc::new(MockProvider::new(vec![
        // Turn 1: two tool calls, both with id "c1"
        Completion {
            content: "doing two things...".into(),
            tool_calls: vec![
                crate::llm::ToolCall {
                    id: "c1".into(),
                    name: "add".into(),
                    arguments: json!({"a": 1, "b": 2}),
                },
                crate::llm::ToolCall {
                    id: "c1".into(), // duplicate id
                    name: "add".into(),
                    arguments: json!({"a": 3, "b": 4}),
                },
            ],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
        // Turn 1: finish
        Completion {
            content: "done".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
    ]));
    let tools = ToolRegistry::local().register(Arc::new(Adder));
    let (sink, mut rx) = crate::event::ChannelSink::new();
    let sink_arc = Arc::new(sink);
    let mut rt = AgentRuntime::builder()
        .llm(llm)
        .tools(tools)
        .event_sink(sink_arc)
        .build()
        .unwrap();

    while let Ok(_ev) = rx.try_recv() {}

    let _ = rt.run("do it").await.unwrap();

    let mut audit_count = 0usize;
    while let Ok(ev) = rx.try_recv() {
        if matches!(ev, AgentEvent::MessageAppendedWithAudit { .. }) {
            audit_count += 1;
        }
    }

    // At least one of the two tool results should carry audit metadata.
    assert!(
        audit_count >= 1,
        "expected at least one tool result to have audit metadata, got {audit_count}"
    );
}

// ── Goal-285: DENIAL_LIMIT_SENTINEL double-push regression ──────────

/// When a batch of tool calls includes a `DENIAL_LIMIT_SENTINEL` as the
/// second result, the transcript must have exactly N tool-result messages
/// — not N+duplicates. The pre-Goal-285 code pushed earlier non-sentinel
/// results twice (once in the outer loop, once in the sentinel inner loop),
/// violating Invariant #8 (unique tool-call ↔ tool-result pairing).
#[tokio::test]
async fn denial_limit_sentinel_no_duplicate_pushes() {
    use crate::error::Error;

    struct DenialTool;

    #[async_trait]
    impl Tool for DenialTool {
        fn spec(&self) -> crate::llm::ToolSpec {
            crate::llm::ToolSpec {
                name: "denial_tool".into(),
                description: "always triggers permission denial limit".into(),
                parameters: json!({"type": "object", "properties": {}}),
            }
        }
        async fn execute(&self, _args: Value) -> crate::error::Result<String> {
            Err(Error::PermissionDeniedLimit {
                name: "denial_tool".into(),
            })
        }
    }

    let llm = Arc::new(MockProvider::new(vec![Completion {
        content: "Let me try two things...".into(),
        tool_calls: vec![
            crate::llm::ToolCall {
                id: "c1".into(),
                name: "add".into(),
                arguments: json!({"a": 1, "b": 2}),
            },
            crate::llm::ToolCall {
                id: "c2".into(),
                name: "denial_tool".into(),
                arguments: json!({}),
            },
        ],
        finish_reason: Some("tool_calls".into()),
        usage: None,
        reasoning_content: None,
    }]));

    let tools = ToolRegistry::local()
        .register(Arc::new(Adder))
        .register(Arc::new(DenialTool));

    let mut rt = AgentRuntime::builder()
        .llm(llm)
        .tools(tools)
        .build()
        .unwrap();

    let out = rt.run("test").await.unwrap();

    // Verify finish reason
    assert_eq!(out.finish_reason, FinishReason::PermissionDenialLimit);

    // Count tool-result messages in transcript.
    // Should have exactly 2 (add + denial_tool), NOT 3.
    let tool_msgs: Vec<_> = rt
        .transcript()
        .iter()
        .filter(|m| m.role == crate::message::Role::Tool)
        .collect();

    assert_eq!(
        tool_msgs.len(),
        2,
        "expected exactly 2 tool-result messages, got {} (double-push bug?)",
        tool_msgs.len()
    );

    // The first tool result ("add") must appear exactly once.
    let add_count = tool_msgs
        .iter()
        .filter(|m| m.tool_call_id.as_deref() == Some("c1"))
        .count();
    assert_eq!(
        add_count, 1,
        "add result (c1) should appear exactly once, got {add_count}"
    );

    // The denial tool result must also appear exactly once.
    let denial_count = tool_msgs
        .iter()
        .filter(|m| m.tool_call_id.as_deref() == Some("c2"))
        .count();
    assert_eq!(
        denial_count, 1,
        "denial result (c2) should appear exactly once, got {denial_count}"
    );

    // Total transcript messages: user(1) + assistant(1) + 2 tool results = 4
    assert_eq!(
        rt.transcript().len(),
        4,
        "transcript should have 4 messages (user, assistant, 2× tool), got {}",
        rt.transcript().len()
    );
}

/// Invariant #8 regression: when stuck detection fires *mid-batch* (the
/// error rate threshold is reached while iterating the results of a
/// multi-call step), the turn must still push a tool_result for EVERY
/// tool_call of the triggering assistant message. The old code returned
/// from inside the result loop before pushing the remaining results,
/// leaving orphaned `tool_use` blocks in the committed transcript — which
/// the provider then rejects on every subsequent turn with HTTP 400
/// ("tool_use ids ... were found without tool_result blocks").
#[tokio::test]
async fn stuck_detection_keeps_tool_calls_paired() {
    use crate::error::Error;

    struct AlwaysFails;

    #[async_trait]
    impl Tool for AlwaysFails {
        fn spec(&self) -> crate::llm::ToolSpec {
            crate::llm::ToolSpec {
                name: "always_fails".into(),
                description: "always returns an error".into(),
                parameters: json!({"type": "object", "properties": {}}),
            }
        }
        async fn execute(&self, _args: Value) -> crate::error::Result<String> {
            Err(Error::Tool {
                name: "always_fails".into(),
                call_id: None,
                message: "boom".into(),
            })
        }
    }

    // One assistant message with three failing tool_calls. With
    // stuck_window=2 and stuck_error_rate=1.0, the second error trips
    // the stuck threshold — mid-batch, before the third result is
    // processed.
    let llm = Arc::new(MockProvider::new(vec![Completion {
        content: "Trying three things at once...".into(),
        tool_calls: vec![
            crate::llm::ToolCall {
                id: "c1".into(),
                name: "always_fails".into(),
                arguments: json!({}),
            },
            crate::llm::ToolCall {
                id: "c2".into(),
                name: "always_fails".into(),
                arguments: json!({}),
            },
            crate::llm::ToolCall {
                id: "c3".into(),
                name: "always_fails".into(),
                arguments: json!({}),
            },
        ],
        finish_reason: Some("tool_calls".into()),
        usage: None,
        reasoning_content: None,
    }]));

    let tools = ToolRegistry::local().register(Arc::new(AlwaysFails));

    let mut rt = AgentRuntime::builder()
        .llm(llm)
        .tools(tools)
        .stuck_window(2)
        .stuck_error_rate(1.0)
        .build()
        .unwrap();

    let out = rt.run("go").await.unwrap();

    // The turn ends as Stuck (error rate hit the threshold).
    assert!(
        matches!(out.finish_reason, FinishReason::Stuck { .. }),
        "expected Stuck finish, got {:?}",
        out.finish_reason
    );

    // Every one of the assistant's three tool_calls must have a matching
    // tool_result, even though stuck fired after the second.
    let assistant = rt
        .transcript()
        .iter()
        .find(|m| m.role == crate::message::Role::Assistant && !m.tool_calls.is_empty())
        .expect("assistant-with-tool_calls must be in transcript");
    let tool_results: Vec<&str> = rt
        .transcript()
        .iter()
        .filter(|m| m.role == crate::message::Role::Tool)
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();
    for tc in &assistant.tool_calls {
        assert!(
                tool_results.contains(&tc.id.as_str()),
                "tool_call {} has no matching tool_result (orphaned tool_use); results={tool_results:?}",
                tc.id
            );
    }
    assert_eq!(
        tool_results.len(),
        3,
        "expected exactly 3 tool_result messages, got {}",
        tool_results.len()
    );
}

/// Issue #100: a transient provider error (`RateLimited` here) is retried at
/// the step level instead of ending the turn. `MockProvider` never retries
/// internally, so recovering the turn proves the run_core loop re-issued the
/// call. The `LlmRetry` event must still be emitted for TUI / SDK consumers.
#[tokio::test]
async fn llm_retry_recovers_and_emits_event() {
    use crate::event::ChannelSink;

    let (sink, mut rx) = ChannelSink::new();
    let sink = Arc::new(sink);

    let provider = Arc::new(
        MockProvider::new(vec![Completion {
            content: "Hello!".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }])
        .with_errors(vec![crate::error::Error::RateLimited {
            provider: "mock".into(),
            retry_after_ms: 1,
        }]),
    );

    let mut rt = AgentRuntime::builder()
        .llm(provider)
        .event_sink(sink)
        // Fast backoff so the test does not sleep the default second.
        .step_retry(crate::llm::RetryPolicy {
            max_retries: 2,
            initial_backoff: std::time::Duration::from_millis(1),
            max_backoff: std::time::Duration::from_millis(1),
        })
        .build()
        .unwrap();

    // Drain registration events.
    while rx.try_recv().is_ok() {}

    let outcome = rt.run("hi").await.expect("a transient 429 must be retried");
    assert_eq!(
        outcome.final_text.as_deref(),
        Some("Hello!"),
        "the retried call's completion must be the turn's final text"
    );

    let mut retried = false;
    while let Ok(ev) = rx.try_recv() {
        if let AgentEvent::LlmRetry {
            attempt, reason, ..
        } = ev
        {
            assert_eq!(attempt, 1, "first retry after the initial failure");
            assert_eq!(reason, "rate_limited");
            retried = true;
        }
    }
    assert!(retried, "an LlmRetry event must be emitted for the backoff");
}

// ── P0-2: set_event_sink / replace_event_sink side-effect contract ────

/// `replace_event_sink` swaps the runtime sink but must NOT touch the
/// tool registry. This pins down the explicit non-side-effect path —
/// callers that only want to redirect `MessageAppended` / `TurnFinished`
/// events without triggering `TodoWriteTool` / `ExitPlanModeTool`
/// re-registration can use this.
#[tokio::test]
async fn replace_event_sink_does_not_reregister_tools() {
    let llm = Arc::new(MockProvider::new(vec![]));
    let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();

    // Capture the pre-swap TodoWriteTool Arc identity.
    let pre_todo = rt
        .kernel
        .tools()
        .get("TodoWrite")
        .expect("TodoWrite is in the default registry")
        .clone();

    let (new_sink, _rx) = crate::event::ChannelSink::new();
    rt.replace_event_sink(Arc::new(new_sink));

    // The registry's TodoWriteTool identity is unchanged — no re-register.
    let post_todo = rt
        .kernel
        .tools()
        .get("TodoWrite")
        .expect("TodoWrite still registered")
        .clone();
    assert!(
        Arc::ptr_eq(&pre_todo, &post_todo),
        "replace_event_sink must not re-register TodoWriteTool"
    );
}

/// `set_event_sink` keeps its existing side effect: re-registering
/// TodoWriteTool so it points to the new sink. This is the contract
/// every caller (CLI per-turn, HTTP per-session, TUI on backend init)
/// depends on; if you intentionally change it, also audit those callers.
#[tokio::test]
async fn set_event_sink_reregisters_todo_write_tool() {
    let llm = Arc::new(MockProvider::new(vec![]));
    let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();

    let pre_todo = rt
        .kernel
        .tools()
        .get("TodoWrite")
        .expect("TodoWrite registered")
        .clone();

    let (new_sink, _rx) = crate::event::ChannelSink::new();
    rt.set_event_sink(Arc::new(new_sink));

    let post_todo = rt
        .kernel
        .tools()
        .get("TodoWrite")
        .expect("TodoWrite still registered after set_event_sink")
        .clone();
    assert!(
        !Arc::ptr_eq(&pre_todo, &post_todo),
        "set_event_sink MUST re-register TodoWriteTool — this side effect is \
             load-bearing for the TUI/CLI/HTTP sink-swap flows; removing it would \
             silently drop TodoUpdated events on the new sink."
    );
}

/// Issue #65: `set_event_sink` re-points the sink-dependent tools that are
/// present — it must not re-introduce tools a surface filter dropped.
/// HTTP/CLI sessions call `set_event_sink` right after build, so an
/// allow-list without TodoWrite would otherwise be silently undone on the
/// first session.
#[tokio::test]
async fn set_event_sink_respects_a_filtered_todo_write() {
    let llm = Arc::new(MockProvider::new(vec![]));
    let mut filtered = crate::tools::build_standard_tools(std::path::Path::new("."), &[], 30);
    filtered.retain_tools(&["Read".to_string()]);

    let mut rt = AgentRuntime::builder()
        .llm(llm)
        .tools(filtered)
        .build()
        .expect("build() with a filtered registry must succeed");
    assert!(rt.kernel.tools().find_by_name("TodoWrite").is_none());

    let (new_sink, _rx) = crate::event::ChannelSink::new();
    rt.set_event_sink(Arc::new(new_sink));

    assert!(
        rt.kernel.tools().find_by_name("TodoWrite").is_none(),
        "set_event_sink must not re-add a TodoWrite removed by the allow-list"
    );
    assert!(
        rt.kernel.tools().find_by_name("Read").is_some(),
        "allow-listed tools are untouched by the sink swap"
    );
}

/// Issue #65, interactive variant: an explicitly filtered registry with
/// `with_plan_mode_tools(true)` keeps the plan tools out, and the subsequent
/// `set_event_sink` does not sneak `exit_plan_mode` back in (interactive
/// hosts swap sinks right after build).
#[tokio::test]
async fn set_event_sink_respects_a_filtered_exit_plan_mode() {
    let llm = Arc::new(MockProvider::new(vec![]));
    let mut filtered = crate::tools::build_standard_tools(std::path::Path::new("."), &[], 30);
    filtered.retain_tools(&["Read".to_string()]);

    let mut rt = AgentRuntime::builder()
        .llm(llm)
        .tools(filtered)
        .with_plan_mode_tools(true)
        .build()
        .expect("build() with a filtered registry must succeed");
    assert!(
        rt.kernel
            .tools()
            .find_by_name(crate::tools::plan_mode::EXIT_PLAN_MODE_TOOL_NAME)
            .is_none(),
        "a filtered surface stays strict through build even with plan tools enabled"
    );

    let (new_sink, _rx) = crate::event::ChannelSink::new();
    rt.set_event_sink(Arc::new(new_sink));

    assert!(
        rt.kernel
            .tools()
            .find_by_name(crate::tools::plan_mode::EXIT_PLAN_MODE_TOOL_NAME)
            .is_none(),
        "set_event_sink must not re-add a plan tool the filter dropped"
    );
}

// ── Issue #48 / Goal 409: the interrupt token reaches the approval wait ──

/// Regression for issue #48: `set_event_sink` must carry the per-turn
/// interrupt token onto the re-registered `ExitPlanModeTool`. Without the
/// mirror, the REPL's per-turn sink swap replaces the token-carrying tool
/// with a tokenless one and Ctrl-C cannot end a parked plan review.
///
/// Observable at the tool level: dispatch the registered `exit_plan_mode`
/// after `set_interrupt_token` + `set_event_sink` (the exact REPL order) and
/// cancel the runtime's kernel token mid-wait — the tool must return a
/// rejected result immediately, and `pending_plan` must be cleared.
#[tokio::test]
async fn set_event_sink_preserves_the_interrupt_token_on_exit_plan_mode() {
    let llm = Arc::new(MockProvider::new(vec![]));
    let mut rt = AgentRuntime::builder()
        .llm(llm)
        .with_plan_mode_tools(true)
        .build()
        .unwrap();

    // The exact REPL per-turn order: token first, then the sink swap.
    let token = tokio_util::sync::CancellationToken::new();
    rt.set_interrupt_token(token.clone());
    let (sink, _rx) = crate::event::ChannelSink::new();
    rt.set_event_sink(Arc::new(sink));

    let plan_tool = rt
        .kernel
        .tools()
        .find_by_name(crate::tools::plan_mode::EXIT_PLAN_MODE_TOOL_NAME)
        .expect("plan tool present (interactive registry, no filter)");
    let token_clone = token.clone();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        token_clone.cancel();
    });

    let start = std::time::Instant::now();
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        plan_tool.execute(json!({ "plan": "p" })),
    )
    .await
    .expect("approval wait must not park past the cancel")
    .expect("execute returns Ok (finish is data)");
    canceller.await.unwrap();
    assert!(
        start.elapsed() < std::time::Duration::from_secs(2),
        "Ctrl-C must end the wait immediately, took {start:?}"
    );
    assert!(out.contains("\"approved\":false"), "got: {out}");
    assert!(out.contains("cancelled"), "got: {out}");
    assert!(
        rt.plan_approval_gate().pending_plan().is_none(),
        "cancelled wait must clear pending_plan (no compaction resurrection)"
    );
}

/// `set_approval_wait_timeout_secs` / `clear_approval_wait_timeout` must also
/// refresh the registered tool (the REPL reads the env once per turn; the
/// timeout only takes effect if the registered tool is rebuilt).
#[tokio::test]
async fn timeout_setter_and_clearer_refresh_the_registered_plan_tool() {
    let llm = Arc::new(MockProvider::new(vec![]));
    let mut rt = AgentRuntime::builder()
        .llm(llm)
        .with_plan_mode_tools(true)
        .build()
        .unwrap();

    rt.set_approval_wait_timeout_secs(1);
    let plan_tool = rt
        .kernel
        .tools()
        .find_by_name(crate::tools::plan_mode::EXIT_PLAN_MODE_TOOL_NAME)
        .expect("plan tool present");
    let start = std::time::Instant::now();
    let out = plan_tool
        .execute(json!({ "plan": "p" }))
        .await
        .expect("bounded wait must return");
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "1s timeout must bound the wait, took {start:?}"
    );
    assert!(out.contains("timed out"), "got: {out}");

    // Clearing restores wait-forever semantics, but the token (installed
    // after the clear) still rescues the wait — no uncancellable path.
    rt.clear_approval_wait_timeout();
    let token = tokio_util::sync::CancellationToken::new();
    let token_for_cancel = token.clone();
    rt.set_interrupt_token(token);
    let plan_tool = rt
        .kernel
        .tools()
        .find_by_name(crate::tools::plan_mode::EXIT_PLAN_MODE_TOOL_NAME)
        .expect("plan tool present");
    token_for_cancel.cancel();
    let start = std::time::Instant::now();
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        plan_tool.execute(json!({ "plan": "p" })),
    )
    .await
    .expect("wait-forever host must still be cancellable")
    .expect("execute returns Ok");
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
    assert!(out.contains("cancelled"), "got: {out}");
}

// ── is_context_window_exceeded ──────────────────────────────────────────

#[test]
fn context_overflow_detector_matches_known_patterns() {
    let cases = [
            // OpenAI / NVIDIA NIM error code
            "HTTP 400: {\"error\":{\"code\":\"context_length_exceeded\",\"message\":\"too long\"}}",
            // OpenAI human message
            "HTTP 400: This model's maximum context length is 200000 tokens, however you requested 201234",
            // Generic phrasing
            "HTTP 400: prompt is too long for the model",
            "HTTP 400: tokens exceeds model limit",
            "HTTP 400: exceeds the model context window",
        ];
    for msg in &cases {
        let err = crate::error::Error::Llm {
            provider: "test".into(),
            message: msg.to_string(),
        };
        assert!(
            is_context_window_exceeded(&err),
            "should detect context overflow in: {msg}"
        );
    }
}

#[test]
fn context_overflow_detector_ignores_unrelated_errors() {
    let cases = [
        "HTTP 400: invalid request body",
        "HTTP 401: unauthorized",
        "HTTP 429: rate limit exceeded",
        "network error: connection refused",
    ];
    for msg in &cases {
        let err = crate::error::Error::Llm {
            provider: "test".into(),
            message: msg.to_string(),
        };
        assert!(
            !is_context_window_exceeded(&err),
            "should NOT detect context overflow in: {msg}"
        );
    }
}

#[test]
fn context_overflow_detector_ignores_non_llm_errors() {
    let err = crate::error::Error::Timeout {
        duration_ms: 30_000,
    };
    assert!(!is_context_window_exceeded(&err));
    let err = crate::error::Error::Config {
        message: "context_length_exceeded".into(),
    };
    assert!(
        !is_context_window_exceeded(&err),
        "Config errors must not be detected even if they contain the keyword"
    );
}

// ── cross-turn microcompact (Goal 333) ──────────────────────────────────

#[tokio::test]
async fn cross_turn_microcompact_prunes_before_summary_check() {
    // Build a runtime with a Microcompactor (low trigger=2) and a Compactor
    // (char threshold high enough that the post-prune transcript would NOT
    // trigger the LLM summary). Seed a transcript with many tool results.
    // Verify: Microcompact event was emitted AND the compactor's LLM was
    // NOT called (summary skipped).
    let provider = Arc::new(MockProvider::new(vec![])); // should not be called
    let (sink, mut rx) = crate::event::ChannelSink::new();
    let sink_arc = Arc::new(sink);

    let mc = crate::compact::Microcompactor::new(2, 1); // trigger at 2
    let compactor = crate::compact::Compactor::new(usize::MAX); // never fires by char
    let mut rt = AgentRuntime::builder()
        .llm(provider)
        .microcompactor(mc)
        .compactor(compactor)
        .event_sink(sink_arc)
        .build()
        .unwrap();

    // Seed transcript with 5 tool-result messages (trigger=2, keep=1).
    let mut msgs: Vec<Message> = Vec::new();
    for i in 0..5 {
        msgs.push(Message::user(format!("user {i}")));
        msgs.push(Message::assistant(format!("asst {i}")));
        msgs.push(Message::tool_result(format!("call_{i}"), "x".repeat(300)));
    }
    *Arc::make_mut(&mut rt.transcript) = msgs;

    // Drain channel events from builder setup.
    while rx.try_recv().is_ok() {}

    // Call maybe_compact_cross_turn directly.
    rt.maybe_compact_cross_turn(&TokenUsage::default())
        .await
        .unwrap();

    // Check for Microcompact event.
    let mut microcompact_fired = false;
    while let Ok(ev) = rx.try_recv() {
        if matches!(ev, AgentEvent::Microcompact { .. }) {
            microcompact_fired = true;
        }
    }

    assert!(
        microcompact_fired,
        "Microcompact event must be emitted when microcompactor prunes"
    );
}

#[tokio::test]
async fn cross_turn_microcompact_disabled_when_none() {
    // No microcompactor configured → behavior identical to today (no Microcompact).
    let provider = Arc::new(MockProvider::new(vec![]));
    let (sink, mut rx) = crate::event::ChannelSink::new();
    let sink_arc = Arc::new(sink);

    // Build runtime with ONLY a compactor (threshold=MAX so it never fires),
    // but NO microcompactor.
    let compactor = crate::compact::Compactor::new(usize::MAX);
    let mut rt = AgentRuntime::builder()
        .llm(provider)
        .compactor(compactor)
        .event_sink(sink_arc)
        .build()
        .unwrap();

    // Seed transcript with many tool results.
    let mut msgs: Vec<Message> = Vec::new();
    for i in 0..5 {
        msgs.push(Message::user(format!("user {i}")));
        msgs.push(Message::assistant(format!("asst {i}")));
        msgs.push(Message::tool_result(format!("call_{i}"), "x".repeat(300)));
    }
    *Arc::make_mut(&mut rt.transcript) = msgs;

    while rx.try_recv().is_ok() {}

    // Call maybe_compact_cross_turn — it has no microcompactor, so the
    // microcompact block is skipped.
    rt.maybe_compact_cross_turn(&TokenUsage::default())
        .await
        .unwrap();

    let mut microcompact_fired = false;
    while let Ok(ev) = rx.try_recv() {
        if matches!(ev, AgentEvent::Microcompact { .. }) {
            microcompact_fired = true;
        }
    }

    assert!(
        !microcompact_fired,
        "Microcompact must NOT fire when no microcompactor is configured"
    );
}

// ── compact_on_overflow ─────────────────────────────────────────────────

#[tokio::test]
async fn compact_on_overflow_compacts_long_transcript() {
    let summary_resp = Completion {
        content: "Summary of prior conversation.".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    };
    let llm = Arc::new(MockProvider::new(vec![summary_resp]));
    let mut rt = AgentRuntime::builder()
        .llm(llm)
        .compactor(crate::Compactor::new(usize::MAX))
        .build()
        .unwrap();

    // Populate a transcript long enough for compaction (> keep_recent_n + 2).
    let msgs: Vec<crate::message::Message> = (0..14)
        .map(|i| {
            if i % 2 == 0 {
                crate::message::Message::user(format!("msg {i}"))
            } else {
                crate::message::Message::assistant(format!("reply {i}"))
            }
        })
        .collect();
    *Arc::make_mut(&mut rt.transcript) = msgs;
    let before = rt.transcript.len();

    let compacted = rt.compact_on_overflow().await.unwrap();
    assert!(compacted, "should return true when compaction ran");
    assert!(
        rt.transcript.len() < before,
        "transcript must shrink after compaction"
    );
    assert_eq!(
        rt.transcript[0].role,
        crate::message::Role::System,
        "first message after compaction is the summary"
    );
}

#[tokio::test]
async fn compact_on_overflow_returns_false_without_compactor() {
    let llm = Arc::new(MockProvider::new(vec![]));
    let mut rt = AgentRuntime::builder().llm(llm).build().unwrap();
    let ok = rt.compact_on_overflow().await.unwrap();
    assert!(!ok, "no compactor → must return false");
}

#[tokio::test]
async fn compact_on_overflow_rejects_degenerate_transcript_without_hook_events() {
    struct CompactionHookRecorder(Arc<std::sync::Mutex<Vec<&'static str>>>);

    impl crate::hooks::Hook for CompactionHookRecorder {
        fn on_event(&self, event: HookEvent) -> crate::hooks::HookAction {
            match event {
                HookEvent::PreCompact { .. } => {
                    self.0.lock().unwrap().push("PreCompact");
                }
                HookEvent::PostCompact { .. } => {
                    self.0.lock().unwrap().push("PostCompact");
                }
                _ => {}
            }
            crate::hooks::HookAction::Continue
        }
    }

    let llm = Arc::new(MockProvider::new(vec![]));
    let hook_events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut hooks = HookRegistry::new();
    hooks.register(Arc::new(CompactionHookRecorder(hook_events.clone())));
    let mut rt = AgentRuntime::builder()
        .llm(llm.clone())
        .hooks(hooks)
        .compactor(Compactor::new(usize::MAX).keep_recent_n(8))
        .build()
        .unwrap();

    // The older slice contains only the system prompt, so it cannot be
    // summarized. This is long enough to reach the compaction guard.
    *Arc::make_mut(&mut rt.transcript) = vec![
        Message::system("System prompt".to_string()),
        Message::user("Add a feature".to_string()),
        Message::assistant("Working on it".to_string()),
        Message::user("Status?".to_string()),
        Message::assistant("Almost done".to_string()),
        Message::user("Run tests".to_string()),
        Message::assistant("Tests pass".to_string()),
        Message::user("Commit".to_string()),
        Message::assistant("Done".to_string()),
    ];

    assert!(
        !rt.compact_on_overflow().await.unwrap(),
        "a degenerate transcript must reject emergency compaction"
    );
    assert!(
        hook_events.lock().unwrap().is_empty(),
        "a rejected compaction must emit neither PreCompact nor PostCompact"
    );
    assert!(
        llm.calls().is_empty(),
        "a rejected compaction must not call the provider"
    );
}

/// Context-overflow error recovery integration test.
///
/// Scenario:
///   1. Agent transcript is long (14 msgs, enough to compact).
///   2. First LLM call fails with a `context_length_exceeded` error.
///   3. `compact_on_overflow` fires: summarises older messages (uses
///      scripted completion [0] for the summary).
///   4. Retry uses scripted completion [1] and succeeds.
#[tokio::test]
async fn context_overflow_triggers_compact_and_retry() {
    let overflow_err = crate::error::Error::Llm {
        provider: "test-model".into(),
        message: "HTTP 400: {\"error\":{\"code\":\"context_length_exceeded\",\
                      \"message\":\"maximum context length is 200000 tokens\"}}"
            .into(),
    };
    let llm = Arc::new(
        MockProvider::new(vec![
            // [0] compaction summary
            Completion {
                content: "Prior conversation summary for test.".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
            // [1] agent reply after successful retry
            Completion {
                content: "Reply after emergency compaction.".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
                reasoning_content: None,
            },
        ])
        .with_errors(vec![overflow_err]),
    );

    let mut rt = AgentRuntime::builder()
        .llm(llm)
        .compactor(crate::Compactor::new(usize::MAX))
        .build()
        .unwrap();

    // Pre-populate transcript (> keep_recent_n + 2 = 10) so compaction can run.
    let msgs: Vec<Message> = (0..14)
        .map(|i| {
            if i % 2 == 0 {
                Message::user(format!("prior msg {i}"))
            } else {
                Message::assistant(format!("prior reply {i}"))
            }
        })
        .collect();
    *Arc::make_mut(&mut rt.transcript) = msgs;

    let outcome = rt.run("test: overflow recovery").await.unwrap();
    assert_eq!(
        outcome.final_text.as_deref(),
        Some("Reply after emergency compaction."),
        "run() must return the retry's reply"
    );

    // After recovery, the transcript's first message should be the compaction summary.
    assert_eq!(
        rt.transcript[0].role,
        crate::message::Role::System,
        "transcript must start with compaction summary after overflow recovery"
    );
    assert!(
        rt.transcript[0].is_compaction_summary,
        "summary message must be flagged as compaction_summary"
    );
}

// ── Goal-340: cross-turn compaction re-injects plan and todos ─────────

#[tokio::test]
async fn cross_turn_compaction_reinjects_plan_and_todos() {
    let provider = Arc::new(MockProvider::new(vec![Completion {
        content: "Summary of prior conversation.".into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }]));

    let mut rt = AgentRuntime::builder()
        .llm(provider)
        .compactor(crate::Compactor::new(0)) // always compact
        .build()
        .unwrap();

    // Seed pending plan via the runtime's shared plan gate.
    rt.plan_approval_gate
        .begin_approval("Step 1: explore\nStep 2: implement".to_string());

    // Seed todos via the runtime's shared todo list.
    {
        let mut todos = rt.todo_list.write().unwrap();
        todos.push(TodoItem {
            content: "Read files".to_string(),
            status: TodoStatus::Completed,
            active_form: None,
        });
        todos.push(TodoItem {
            content: "Edit code".to_string(),
            status: TodoStatus::InProgress,
            active_form: Some("Editing code...".to_string()),
        });
        todos.push(TodoItem {
            content: "Run tests".to_string(),
            status: TodoStatus::Pending,
            active_form: None,
        });
    }

    // Populate transcript long enough for compaction.
    let msgs: Vec<Message> = (0..14)
        .map(|i| {
            if i % 2 == 0 {
                Message::user(format!("msg {i}"))
            } else {
                Message::assistant(format!("reply {i}"))
            }
        })
        .collect();
    *Arc::make_mut(&mut rt.transcript) = msgs;

    rt.maybe_compact_cross_turn(&TokenUsage::default())
        .await
        .unwrap();

    let transcript = rt.transcript();

    // Transcript order: [summary, plan-att, todo-att, ...preserved]
    assert!(
        transcript.len() >= 3,
        "at least summary + 2 atts + preserved"
    );
    assert_eq!(transcript[0].role, crate::message::Role::System);
    assert!(
        transcript[0].is_compaction_summary,
        "first message is the compaction summary"
    );

    // Find the plan and todo attachments.
    let plan_msg = transcript
        .iter()
        .find(|m| m.content.starts_with("[post-compact plan restore]"))
        .expect("plan restore message must be present");

    assert!(plan_msg.content.contains("Step 1: explore"));
    assert!(plan_msg.content.contains("You are in plan mode"));

    let todo_msg = transcript
        .iter()
        .find(|m| m.content.starts_with("[post-compact todo restore]"))
        .expect("todo restore message must be present");

    assert!(todo_msg.content.contains("- [x] Read files"));
    assert!(todo_msg.content.contains("- [/] Edit code"));
    assert!(todo_msg.content.contains("(active: Editing code...)"));
    assert!(todo_msg.content.contains("- [ ] Run tests"));

    // Plan attachment should be before the todo attachment (plan first).
    let plan_idx = transcript
        .iter()
        .position(|m| m.content.starts_with("[post-compact plan restore]"))
        .unwrap();
    let todo_idx = transcript
        .iter()
        .position(|m| m.content.starts_with("[post-compact todo restore]"))
        .unwrap();
    assert!(
        plan_idx < todo_idx,
        "plan attachment must come before todo attachment"
    );
}

// ── Goal 399: wall_timeout_secs forwarding ─────────────────────────────────

#[test]
fn runtime_builder_forwards_wall_timeout_to_kernel() {
    let runtime = AgentRuntimeBuilder::new()
        .llm(Arc::new(MockProvider::new(vec![])))
        .tools(ToolRegistry::local())
        .wall_timeout_secs(7)
        .build()
        .expect("runtime build");
    assert_eq!(runtime.kernel.wall_timeout_secs, 7);
}

#[test]
fn runtime_builder_wall_timeout_defaults_to_zero() {
    let runtime = AgentRuntimeBuilder::new()
        .llm(Arc::new(MockProvider::new(vec![])))
        .tools(ToolRegistry::local())
        .build()
        .expect("runtime build");
    assert_eq!(runtime.kernel.wall_timeout_secs, 0);
}

// ── Issue #94: cost_budget forwarding ──────────────────────────────────────

#[test]
fn runtime_builder_forwards_cost_budget_to_kernel() {
    let pricing = crate::llm::ModelPricing {
        input_per_million: 3.0,
        output_per_million: 9.0,
        cache_hit_input_per_million: 0.3,
    };
    let runtime = AgentRuntimeBuilder::new()
        .llm(Arc::new(MockProvider::new(vec![])))
        .tools(ToolRegistry::local())
        .cost_budget(Some(2.5), Some(pricing))
        .build()
        .expect("runtime build");
    assert_eq!(runtime.kernel.max_budget_usd, Some(2.5));
    assert_eq!(runtime.kernel.budget_pricing, Some(pricing));
}

#[test]
fn runtime_builder_cost_budget_defaults_to_none() {
    let runtime = AgentRuntimeBuilder::new()
        .llm(Arc::new(MockProvider::new(vec![])))
        .tools(ToolRegistry::local())
        .build()
        .expect("runtime build");
    assert!(runtime.kernel.max_budget_usd.is_none());
    assert!(runtime.kernel.budget_pricing.is_none());
}

// ── Issue #99: loop retry safety + wakeup-store wiring ─────────────────────

#[tokio::test]
async fn retry_is_safe_only_when_the_tail_can_be_re_dispatched() {
    let mut rt = AgentRuntimeBuilder::new()
        .llm(Arc::new(MockProvider::new(vec![])))
        .build()
        .expect("runtime build");

    assert!(!rt.retry_is_safe(), "an empty transcript is not replayable");

    Arc::make_mut(&mut rt.transcript).push(Message::user("go"));
    assert!(rt.retry_is_safe(), "a staged user prompt is replayable");

    Arc::make_mut(&mut rt.transcript).push(Message::tool_result("c1", "output"));
    assert!(
        rt.retry_is_safe(),
        "a tool-result tail is the failed attempt's resume point"
    );

    Arc::make_mut(&mut rt.transcript).push(Message::system("injected note"));
    assert!(
        rt.retry_is_safe(),
        "an injected system note (skill / compaction summary) is replayable"
    );

    Arc::make_mut(&mut rt.transcript).push(Message::assistant("done"));
    assert!(
        !rt.retry_is_safe(),
        "an assistant tail means the turn completed — replaying duplicates work"
    );
}

#[test]
fn runtime_builder_wires_the_loop_retry_policy_and_wakeup_store_dir() {
    let dir = std::path::PathBuf::from("/tmp/recursive-issue-99-test-dir");
    let policy = LoopRetryPolicy::new(
        7,
        std::time::Duration::from_secs(3),
        std::time::Duration::from_secs(9),
    );
    let rt = AgentRuntimeBuilder::new()
        .llm(Arc::new(MockProvider::new(vec![])))
        .loop_retry(policy)
        .wakeup_store_dir(&dir)
        .build()
        .expect("runtime build");

    assert_eq!(rt.loop_retry, policy);
    assert_eq!(rt.wakeup_store_dir.as_deref(), Some(dir.as_path()));
}

#[test]
fn loop_retry_and_wakeup_store_dir_default_to_off_and_bounded() {
    use crate::tools::WakeupRequest;
    let rt = AgentRuntimeBuilder::new()
        .llm(Arc::new(MockProvider::new(vec![])))
        .build()
        .expect("runtime build");

    assert_eq!(rt.loop_retry, LoopRetryPolicy::default());
    assert!(rt.wakeup_store_dir.is_none());

    // Persisting with no directory configured is a silent no-op.
    rt.persist_pending_wakeup(&WakeupRequest {
        delay: std::time::Duration::from_secs(1),
        reason: "r".into(),
        prompt: "p".into(),
    });
    rt.clear_pending_wakeup();
}
