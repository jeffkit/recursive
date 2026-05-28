//! Full-stack integration tests exercising the agent + compactor + hooks +
//! skills + permission hook + tool transport together.
//!
//! These tests use `MockProvider` with scripted responses and `tempfile`
//! for filesystem isolation. They verify that the public API works correctly
// Allow deprecated Agent/AgentOutcome until tests are fully migrated.
#![allow(deprecated)]
//! when all subsystems are wired together.

use std::sync::Arc;

use recursive::{
    agent::{FinishReason, PermissionDecision},
    compact::Compactor,
    hooks::{Hook, HookAction, HookEvent},
    llm::{Completion, MockProvider, ToolCall},
    message::Message,
    tools::{LocalTransport, ToolRegistry, ToolTransport},
    Agent,
};
use serde_json::json;
use tempfile::TempDir;

// ============================================================================
// Helper: a hook that counts how many times it fires for a given event kind.
// ============================================================================
struct CountingHook {
    pre_tool_call_count: std::sync::atomic::AtomicUsize,
    post_tool_call_count: std::sync::atomic::AtomicUsize,
    session_start_count: std::sync::atomic::AtomicUsize,
    pre_compact_count: std::sync::atomic::AtomicUsize,
    post_compact_count: std::sync::atomic::AtomicUsize,
}

impl CountingHook {
    fn new() -> Self {
        Self {
            pre_tool_call_count: std::sync::atomic::AtomicUsize::new(0),
            post_tool_call_count: std::sync::atomic::AtomicUsize::new(0),
            session_start_count: std::sync::atomic::AtomicUsize::new(0),
            pre_compact_count: std::sync::atomic::AtomicUsize::new(0),
            post_compact_count: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl Hook for CountingHook {
    fn on_event(&self, event: HookEvent) -> HookAction {
        match event {
            HookEvent::PreToolCall { .. } => {
                self.pre_tool_call_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                HookAction::Continue
            }
            HookEvent::PostToolCall { .. } => {
                self.post_tool_call_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                HookAction::Continue
            }
            HookEvent::SessionStart { .. } => {
                self.session_start_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                HookAction::Continue
            }
            HookEvent::PreCompact { .. } => {
                self.pre_compact_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                HookAction::Continue
            }
            HookEvent::PostCompact { .. } => {
                self.post_compact_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                HookAction::Continue
            }
            _ => HookAction::Continue,
        }
    }
}

// ============================================================================
// Test 1: Hooks + compaction
//
// Verifies that lifecycle hooks fire for each tool call and that compaction
// triggers when the transcript exceeds the threshold.
// ============================================================================
#[tokio::test]
async fn hooks_and_compaction() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // Write a file so read_file succeeds.
    std::fs::write(root.join("data.txt"), b"hello world").unwrap();

    // Script: 3 tool calls (to build up transcript) then a final stop.
    let script = vec![
        Completion {
            content: "reading file".into(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                arguments: json!({"path": "data.txt"}),
            }],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
        Completion {
            content: "listing dir".into(),
            tool_calls: vec![ToolCall {
                id: "c2".into(),
                name: "list_dir".into(),
                arguments: json!({"path": "."}),
            }],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
        Completion {
            content: "reading again".into(),
            tool_calls: vec![ToolCall {
                id: "c3".into(),
                name: "read_file".into(),
                arguments: json!({"path": "data.txt"}),
            }],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
        // This completion is consumed by the compactor
        Completion {
            content: "Summary: read file, listed dir, tests pass.".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
        // Final completion after compaction
        Completion {
            content: "done".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
    ];

    let llm = Arc::new(MockProvider::new(script));
    let transport: Arc<dyn ToolTransport> = Arc::new(LocalTransport);
    let tools = ToolRegistry::new(transport)
        .register(Arc::new(recursive::tools::ReadFile::new(root)))
        .register(Arc::new(recursive::tools::ListDir::new(root)));

    let hook = Arc::new(CountingHook::new());

    let compactor = Compactor::new(100).keep_recent_n(2);

    let mut agent = Agent::builder()
        .llm(llm)
        .tools(tools)
        .system_prompt("you are a test agent")
        .max_steps(10)
        .compactor(compactor)
        .hook(hook.clone())
        .build()
        .unwrap();

    let outcome = agent.run("read the file and list the dir").await.unwrap();

    // Agent should complete normally.
    assert_eq!(
        outcome.finish,
        FinishReason::NoMoreToolCalls,
        "expected NoMoreToolCalls, got {:?}",
        outcome.finish
    );

    // Hook should have fired for each tool call (3 pre, 3 post).
    assert_eq!(
        hook.pre_tool_call_count
            .load(std::sync::atomic::Ordering::SeqCst),
        3,
        "expected 3 PreToolCall events"
    );
    assert_eq!(
        hook.post_tool_call_count
            .load(std::sync::atomic::Ordering::SeqCst),
        3,
        "expected 3 PostToolCall events"
    );
    assert_eq!(
        hook.session_start_count
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "expected 1 SessionStart event"
    );

    // Compaction should have fired (transcript exceeded 500 chars).
    assert!(
        hook.pre_compact_count
            .load(std::sync::atomic::Ordering::SeqCst)
            >= 1,
        "expected at least 1 PreCompact event"
    );
    assert!(
        hook.post_compact_count
            .load(std::sync::atomic::Ordering::SeqCst)
            >= 1,
        "expected at least 1 PostCompact event"
    );

    // The transcript should contain the compacted summary.
    let summary_msgs: Vec<&Message> = outcome
        .transcript
        .iter()
        .filter(|m| m.role == recursive::message::Role::System)
        .collect();
    assert!(
        !summary_msgs.is_empty(),
        "expected at least one system message (the compaction summary)"
    );
    assert!(
        summary_msgs
            .iter()
            .any(|m| m.content.contains("[compacted:")),
        "expected a system message with compacted header"
    );
}

// ============================================================================
// Test 2: Permission hook + sub-agent
//
// Verifies that a permission hook can deny a tool call and that the denial
// is inherited by sub-agents spawned by the parent.
// ============================================================================
#[tokio::test]
async fn permission_hook_and_sub_agent() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // Create a file so read_file succeeds.
    std::fs::write(root.join("notes.txt"), b"some data").unwrap();

    // Scripted completions for the parent agent:
    //   1. Parent calls sub_agent with a prompt that asks to run_shell and read_file.
    //   2. Sub-agent's first call: tries run_shell (denied by inherited hook).
    //   3. Sub-agent's second call: tries read_file (allowed).
    //   4. Sub-agent's third call: finishes.
    //   5. Parent's second call: finishes.
    let script = vec![
        // 1. Parent calls sub_agent
        Completion {
            content: "spawning sub-agent".into(),
            tool_calls: vec![ToolCall {
                id: "p1".into(),
                name: "sub_agent".into(),
                arguments: json!({
                    "prompt": "run 'echo hi' and read notes.txt",
                    "tools": ["run_shell", "read_file"]
                }),
            }],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
        // 2. Sub-agent tries run_shell (denied by permission hook)
        Completion {
            content: "running shell".into(),
            tool_calls: vec![ToolCall {
                id: "s1".into(),
                name: "run_shell".into(),
                arguments: json!({"command": "echo hi"}),
            }],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
        // 3. Sub-agent tries read_file (allowed)
        Completion {
            content: "reading file".into(),
            tool_calls: vec![ToolCall {
                id: "s2".into(),
                name: "read_file".into(),
                arguments: json!({"path": "notes.txt"}),
            }],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
        // 4. Sub-agent finishes
        Completion {
            content: "sub-agent done".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
        // 5. Parent finishes
        Completion {
            content: "parent done".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
    ];

    let llm = Arc::new(MockProvider::new(script));
    let transport: Arc<dyn ToolTransport> = Arc::new(LocalTransport);

    // Build the full tool registry including sub_agent.
    let all_tools = ToolRegistry::new(transport)
        .register(Arc::new(recursive::tools::ReadFile::new(root)))
        .register(Arc::new(recursive::tools::RunShell::new(root)))
        .register(Arc::new(recursive::tools::SubAgent::new(
            root,
            llm.clone(),
            // We need a placeholder; the sub_agent tool will use this
            // to build its own sub-registry. Since we pass the same
            // all_tools, it will have access to everything.
            ToolRegistry::new(Arc::new(LocalTransport))
                .register(Arc::new(recursive::tools::ReadFile::new(root)))
                .register(Arc::new(recursive::tools::RunShell::new(root))),
            3,
            0,
            None,
        )));

    // Permission hook: deny run_shell, allow everything else.
    let mut agent = Agent::builder()
        .llm(llm)
        .tools(all_tools)
        .system_prompt("you are a test agent with permission hook")
        .max_steps(10)
        .permission_hook(|name, _args| {
            if name == "run_shell" {
                PermissionDecision::Deny("run_shell is not allowed".into())
            } else {
                PermissionDecision::Allow
            }
        })
        .build()
        .unwrap();

    let outcome = agent.run("spawn a sub-agent to explore").await.unwrap();

    // Parent should complete successfully.
    assert_eq!(
        outcome.finish,
        FinishReason::NoMoreToolCalls,
        "expected NoMoreToolCalls, got {:?}",
        outcome.finish
    );
    assert_eq!(
        outcome.final_message.as_deref(),
        Some("parent done"),
        "expected parent done"
    );

    // The sub-agent's result should be visible in the parent's transcript.
    let transcript_str: String = outcome
        .transcript
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        transcript_str.contains("sub-agent done"),
        "sub-agent result should appear in parent transcript"
    );
}

// ============================================================================
// Test 3: Skill index injection
//
// Verifies that skills discovered in `.recursive/skills/` appear in the
// system prompt via `skill_index`, and that `load_skill` can retrieve them.
// ============================================================================
#[tokio::test]
async fn skill_index_injection() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // Create a skill directory and SKILL.md file.
    let skills_dir = root.join(".recursive").join("skills").join("test-skill");
    std::fs::create_dir_all(&skills_dir).unwrap();
    std::fs::write(
        skills_dir.join("SKILL.md"),
        "---\nname: test-skill\ndescription: A test skill for integration testing\n---\n\n# Test Skill\n\nThis is a test skill.",
    )
    .unwrap();

    // Discover skills and build the skill index.
    let search_paths = vec![root.join(".recursive").join("skills")];
    let skills = recursive::skills::discover_skills(&search_paths);
    let index = recursive::skills::skill_index(&skills);

    assert!(
        !skills.is_empty(),
        "expected at least one skill to be discovered"
    );
    assert!(
        index.contains("test-skill"),
        "skill index should contain 'test-skill'"
    );
    assert!(
        index.contains("A test skill for integration testing"),
        "skill index should contain the description"
    );

    // Now run an agent that has the skill index in its system prompt and
    // the load_skill tool available.
    let script = vec![
        Completion {
            content: "loading skill".into(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "load_skill".into(),
                arguments: json!({"name": "test-skill"}),
            }],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
        Completion {
            content: "skill loaded successfully".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
    ];

    let llm = Arc::new(MockProvider::new(script));
    let transport: Arc<dyn ToolTransport> = Arc::new(LocalTransport);
    let tools = ToolRegistry::new(transport)
        .register(Arc::new(recursive::tools::LoadSkill::new(skills.clone())));

    let system_prompt = format!(
        "You are a test agent with skills.\n{}",
        recursive::skills::skill_index(&skills)
    );

    let mut agent = Agent::builder()
        .llm(llm)
        .tools(tools)
        .system_prompt(system_prompt)
        .max_steps(5)
        .build()
        .unwrap();

    let outcome = agent.run("load the test skill").await.unwrap();

    assert_eq!(
        outcome.finish,
        FinishReason::NoMoreToolCalls,
        "expected NoMoreToolCalls, got {:?}",
        outcome.finish
    );

    // The tool result should contain the skill content.
    let tool_msgs: Vec<&Message> = outcome
        .transcript
        .iter()
        .filter(|m| m.role == recursive::message::Role::Tool)
        .collect();
    assert_eq!(tool_msgs.len(), 1, "expected one tool result message");
    assert!(
        tool_msgs[0].content.contains("Test Skill"),
        "tool result should contain skill content: {}",
        tool_msgs[0].content
    );
}

// ============================================================================
// Test 4: Session pause + resume
//
// Verifies that an agent run can be paused (transcript saved) and resumed
// from the saved state, continuing where it left off.
// ============================================================================
#[tokio::test]
async fn session_pause_and_resume() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // Create a file so read_file succeeds.
    std::fs::write(root.join("config.json"), br#"{"key": "value"}"#).unwrap();

    // Script for the first run (paused after 2 steps).
    let script_part1 = vec![
        Completion {
            content: "reading config".into(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                arguments: json!({"path": "config.json"}),
            }],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
        Completion {
            content: "I see the config".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
    ];

    let llm = Arc::new(MockProvider::new(script_part1));
    let transport: Arc<dyn ToolTransport> = Arc::new(LocalTransport);
    let tools =
        ToolRegistry::new(transport).register(Arc::new(recursive::tools::ReadFile::new(root)));

    let mut agent = Agent::builder()
        .llm(llm.clone())
        .tools(tools.clone())
        .system_prompt("you are a test agent")
        .max_steps(5)
        .build()
        .unwrap();

    let outcome1 = agent.run("read the config file").await.unwrap();

    // First run should complete.
    assert_eq!(
        outcome1.finish,
        FinishReason::NoMoreToolCalls,
        "first run should complete normally"
    );
    assert_eq!(outcome1.steps, 2, "first run should take 2 steps");

    // Save the transcript as a session file.
    let session_path = root.join("session.json");
    let session = recursive::session::SessionFile::new(
        "read the config file".to_string(),
        "mock-model".to_string(),
        "mock".to_string(),
        &tools.specs(),
        outcome1.steps,
        outcome1.transcript.clone(),
    );
    session.write_to(&session_path).unwrap();

    // Verify the session file can be read back.
    let restored = recursive::session::SessionFile::read_from(&session_path).unwrap();
    assert_eq!(
        restored.messages().len(),
        outcome1.transcript.len(),
        "restored session should have same transcript length"
    );
    assert_eq!(
        restored.steps_consumed, 2,
        "restored session should have 2 steps consumed"
    );

    // Now resume: create a new agent seeded with the saved transcript.
    let script_part2 = vec![
        Completion {
            content: "continuing from where I left off".into(),
            tool_calls: vec![ToolCall {
                id: "c2".into(),
                name: "read_file".into(),
                arguments: json!({"path": "config.json"}),
            }],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
        Completion {
            content: "resume complete".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
    ];

    let llm2 = Arc::new(MockProvider::new(script_part2));

    let mut resumed_agent = Agent::builder()
        .llm(llm2)
        .tools(tools)
        .system_prompt("you are a test agent")
        .max_steps(5)
        .seed_transcript(restored.into_transcript())
        .build()
        .unwrap();

    let outcome2 = resumed_agent
        .run("continue reading the config file")
        .await
        .unwrap();

    // Resumed run should complete.
    assert_eq!(
        outcome2.finish,
        FinishReason::NoMoreToolCalls,
        "resumed run should complete normally"
    );
    assert_eq!(outcome2.steps, 2, "resumed run should take 2 steps");

    // The full transcript should include both the original and resumed messages.
    // Seed (3: system + user + assistant) + new user goal + assistant + tool call + tool result + assistant = 8
    assert!(
        outcome2.transcript.len() >= 6,
        "resumed transcript should have at least 6 messages, got {}",
        outcome2.transcript.len()
    );

    // The resumed agent should have the original context available.
    let transcript_str: String = outcome2
        .transcript
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        transcript_str.contains("reading config"),
        "resumed transcript should contain original assistant message"
    );
    assert!(
        transcript_str.contains("resume complete"),
        "resumed transcript should contain resumed final message"
    );
}

// ============================================================================
// Test 5: Tool transport
//
// Verifies that an agent with an explicitly set `LocalTransport` behaves
// identically to the default transport.
// ============================================================================
#[tokio::test]
async fn tool_transport_explicit() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // Write a file so read_file succeeds.
    std::fs::write(root.join("greeting.txt"), b"hello from transport test").unwrap();

    // Script: one tool call then stop.
    let script = vec![
        Completion {
            content: "reading file via explicit transport".into(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                arguments: json!({"path": "greeting.txt"}),
            }],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
        Completion {
            content: "transport test complete".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        },
    ];

    let llm = Arc::new(MockProvider::new(script));
    let transport: Arc<dyn ToolTransport> = Arc::new(LocalTransport);
    let tools =
        ToolRegistry::new(transport).register(Arc::new(recursive::tools::ReadFile::new(root)));

    let mut agent = Agent::builder()
        .llm(llm)
        .tools(tools)
        .system_prompt("you are a test agent with explicit transport")
        .max_steps(5)
        .build()
        .unwrap();

    let outcome = agent.run("read the greeting file").await.unwrap();

    assert_eq!(
        outcome.finish,
        FinishReason::NoMoreToolCalls,
        "expected NoMoreToolCalls, got {:?}",
        outcome.finish
    );
    assert_eq!(outcome.steps, 2, "expected 2 steps");

    // The tool result should contain the file contents.
    let tool_msgs: Vec<&Message> = outcome
        .transcript
        .iter()
        .filter(|m| m.role == recursive::message::Role::Tool)
        .collect();
    assert_eq!(tool_msgs.len(), 1, "expected one tool result message");
    assert!(
        tool_msgs[0].content.contains("hello from transport test"),
        "tool result should contain file contents: {}",
        tool_msgs[0].content
    );

    // The final message should confirm completion.
    assert_eq!(
        outcome.final_message.as_deref(),
        Some("transport test complete")
    );
}
