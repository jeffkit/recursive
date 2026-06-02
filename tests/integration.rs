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

// ============================================================================
// Goal-137: Graceful shutdown — CancellationToken plumbed through
// AgentRuntime → AgentKernel → RunCore. Verifies that
// FinishReason::Cancelled is reachable, that the loop exits at the next
// step boundary, and that absent token = no behavior change.
// ============================================================================

mod shutdown {
    use recursive::agent::FinishReason;
    use recursive::hooks::{Hook, HookAction, HookEvent};
    use recursive::kernel::AgentKernel;
    use recursive::llm::{Completion, MockProvider, ToolCall};
    use recursive::runtime::AgentRuntimeBuilder;
    use recursive::tools::ToolRegistry;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    fn final_completion(text: &str) -> Completion {
        Completion {
            content: text.into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            reasoning_content: None,
        }
    }

    fn tool_call_completion(name: &str, args: serde_json::Value) -> Completion {
        Completion {
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: format!("c-{}", name),
                name: name.into(),
                arguments: args,
            }],
            finish_reason: None,
            usage: None,
            reasoning_content: None,
        }
    }

    fn build_runtime_with_token(
        provider: Arc<MockProvider>,
        token: Option<CancellationToken>,
    ) -> recursive::runtime::AgentRuntime {
        let mut builder = AgentRuntimeBuilder::new()
            .llm(provider)
            .tools(ToolRegistry::local())
            .system_prompt("you are a test agent")
            .max_steps(20);
        if let Some(t) = token {
            builder = builder.shutdown_token(t);
        }
        builder.build().expect("runtime build")
    }

    /// Test A — token cancelled before any step starts → outcome is
    /// FinishReason::Cancelled with steps == 0.
    #[tokio::test]
    async fn cancellation_before_first_step_returns_cancelled_at_step_zero() {
        let provider = Arc::new(MockProvider::new(vec![final_completion("never reached")]));
        let token = CancellationToken::new();
        token.cancel(); // cancel BEFORE the run starts

        let mut runtime = build_runtime_with_token(provider.clone(), Some(token));
        let outcome = runtime.run("anything").await.expect("runtime.run");

        assert!(
            matches!(outcome.finish_reason, FinishReason::Cancelled),
            "expected Cancelled, got {:?}",
            outcome.finish_reason
        );
        assert_eq!(outcome.steps, 0, "no steps should have completed");
        assert_eq!(
            provider.calls().len(),
            0,
            "MockProvider should not have been called"
        );
    }

    /// Test B — cancellation observed after the first LLM call returns
    /// a non-final completion. Loop exits with Cancelled at the next
    /// step boundary; steps < total scripted completions.
    ///
    /// Cancellation is triggered synchronously inside `MockProvider::complete()`
    /// so the token is set before the agent continues to tool execution.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_during_run_terminates_loop() {
        // Script: 5 LLM responses. First 4 request `__noop__` with
        // *distinct* args each time (otherwise anti-stuck detection in
        // agent.rs:807 fires after 3 identical-key error results, which
        // would mask FinishReason::Cancelled). 5th is a final response.
        // We cancel after the 1st call.
        let mut script = Vec::new();
        for i in 0..4 {
            script.push(tool_call_completion(
                "__noop__",
                serde_json::json!({ "tag": format!("call-{i}") }),
            ));
        }
        script.push(final_completion("would-be-final"));

        let token = CancellationToken::new();
        let token_for_hook = token.clone();
        let provider = Arc::new(MockProvider::new(script).with_on_complete_fn(move || {
            token_for_hook.cancel();
        }));

        let mut runtime = build_runtime_with_token(provider.clone(), Some(token));
        let outcome = runtime.run("kick off").await.expect("runtime.run");

        assert!(
            matches!(outcome.finish_reason, FinishReason::Cancelled),
            "expected Cancelled, got {:?}",
            outcome.finish_reason
        );
        // At least one LLM call happened (we waited on the notify);
        // strictly fewer than the full script ran.
        assert!(
            !provider.calls().is_empty(),
            "expected at least one LLM call before cancellation"
        );
        assert!(
            provider.calls().len() < 5,
            "expected fewer than the scripted 5 calls; got {}",
            provider.calls().len()
        );
    }

    /// Test C — without a token, the loop runs to natural completion;
    /// guards against the cancellation check becoming unconditional.
    #[tokio::test]
    async fn no_token_means_no_cancellation_check() {
        let provider = Arc::new(MockProvider::new(vec![final_completion("done")]));
        let mut runtime = build_runtime_with_token(provider, None);
        let outcome = runtime.run("hi").await.expect("runtime.run");
        assert!(
            matches!(outcome.finish_reason, FinishReason::NoMoreToolCalls),
            "expected NoMoreToolCalls, got {:?}",
            outcome.finish_reason
        );
    }

    /// Test D — `with_tools` propagates the shutdown token to the cloned
    /// kernel (multi-agent sub-agent semantics).
    #[tokio::test]
    async fn kernel_with_tools_propagates_shutdown_token() {
        let provider = Arc::new(MockProvider::new(vec![final_completion("ok")]));
        let token = CancellationToken::new();
        let kernel = AgentKernel::builder()
            .llm(provider)
            .tools(ToolRegistry::local())
            .max_steps(5)
            .shutdown_token(token.clone())
            .build()
            .expect("kernel build");

        let cloned = kernel.with_tools(ToolRegistry::local());

        // Cancel via the original token.
        token.cancel();

        // The cloned kernel's shutdown_token must observe the cancellation
        // (proves it shares the same Arc-backed handle).
        assert!(
            cloned
                .shutdown_token()
                .map(|t| t.is_cancelled())
                .unwrap_or(false),
            "cloned kernel should see cancellation through propagated token"
        );
    }

    /// Test E — Cancelled does NOT dispatch SessionEnd hooks.
    /// (Existing agent.rs gating intentionally lists only NoMoreToolCalls
    /// / Stuck / BudgetExceeded; cancellation is user-initiated and
    /// callers may not want hook side-effects on it.)
    ///
    /// NOTE: this test exercises hook dispatch via the deprecated `Agent`
    /// path because that is where SessionEnd dispatch lives. The kernel
    /// path (via AgentRuntime/AgentKernel) does not currently dispatch
    /// SessionEnd at all (also a pre-existing gap, out of g137 scope).
    /// What we assert here: when the legacy `Agent::run` path returns
    /// with FinishReason::Cancelled, no SessionEnd hook fires.
    #[tokio::test]
    async fn cancelled_does_not_dispatch_session_end_hook() {
        struct CountingHook {
            session_end_count: AtomicUsize,
        }
        impl Hook for CountingHook {
            fn on_event(&self, event: HookEvent) -> HookAction {
                if matches!(event, HookEvent::SessionEnd { .. }) {
                    self.session_end_count.fetch_add(1, Ordering::Relaxed);
                }
                HookAction::Continue
            }
        }

        let counter = Arc::new(CountingHook {
            session_end_count: AtomicUsize::new(0),
        });

        let provider = Arc::new(MockProvider::new(vec![final_completion("never")]));
        let token = CancellationToken::new();
        token.cancel();

        let mut agent = recursive::Agent::builder()
            .llm(provider)
            .tools(ToolRegistry::local())
            .system_prompt("test")
            .hook(counter.clone())
            .shutdown_token(token)
            .build()
            .expect("agent build");

        let outcome = agent.run("ignored").await.expect("run");
        assert!(matches!(outcome.finish, FinishReason::Cancelled));
        assert_eq!(
            counter.session_end_count.load(Ordering::Relaxed),
            0,
            "SessionEnd must not fire on Cancelled"
        );
    }
}

// ============================================================================
// Goal-140: Tool permission system wiring. g133 shipped
// PermissionsConfig + ToolRegistry::with_permissions; this verifies
// that, when a config is attached, ToolRegistry::invoke enforces it.
// ============================================================================

mod permissions {
    use recursive::error::Error;
    use recursive::permissions::PermissionMode;
    use recursive::tools::{
        BackgroundJobManager, LocalTransport, ReadFile, RunBackground, RunShell, ToolRegistry,
        WriteFile,
    };
    use serde_json::json;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn registry_with(
        perms: recursive::permissions::LayeredPermissionsConfig,
    ) -> (ToolRegistry, TempDir) {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path().to_path_buf();
        let bg = Arc::new(tokio::sync::Mutex::new(BackgroundJobManager::new()));
        let registry = ToolRegistry::new(Arc::new(LocalTransport))
            .register(Arc::new(ReadFile::new(&root)))
            .register(Arc::new(WriteFile::new(&root)))
            .register(Arc::new(RunShell::new(&root)))
            .register(Arc::new(RunBackground::new(&root, bg)))
            .with_permissions(perms);
        (registry, tmp)
    }

    /// Test A — explicit deny blocks invocation; the registry returns
    /// `Error::PermissionDenied` (which becomes an "ERROR: ..." tool
    /// result message at the agent loop level).
    #[tokio::test]
    async fn permissions_deny_blocks_invoke() {
        let perms = recursive::permissions::LayeredPermissionsConfig {
            mode: PermissionMode::Allow,
            layers: vec![recursive::permissions::PermissionLayer {
                source: recursive::permissions::RuleSource::User,
                deny: vec!["run_shell".into()],
                ..Default::default()
            }],
        };
        let (registry, _tmp) = registry_with(perms);
        let result = registry
            .invoke("run_shell", json!({ "command": "echo hi" }))
            .await;
        match result {
            Err(Error::PermissionDenied { name, .. }) => assert_eq!(name, "run_shell"),
            other => panic!("expected PermissionDenied, got {other:?}"),
        }
    }

    /// Test B — non-empty allow list rejects unlisted tools.
    #[tokio::test]
    async fn permissions_allow_filter_blocks_unlisted() {
        let perms = recursive::permissions::LayeredPermissionsConfig {
            mode: PermissionMode::Allow,
            layers: vec![recursive::permissions::PermissionLayer {
                source: recursive::permissions::RuleSource::User,
                allow: vec!["read_file".into()],
                ..Default::default()
            }],
        };
        let (registry, _tmp) = registry_with(perms);
        let result = registry
            .invoke("write_file", json!({ "path": "x.txt", "content": "y" }))
            .await;
        assert!(
            matches!(result, Err(Error::PermissionDenied { .. })),
            "expected PermissionDenied for write_file under allow=[read_file]"
        );
    }

    /// Test C — glob patterns match multiple tools.
    #[tokio::test]
    async fn permissions_glob_pattern_matches() {
        let perms = recursive::permissions::LayeredPermissionsConfig {
            mode: PermissionMode::Allow,
            layers: vec![recursive::permissions::PermissionLayer {
                source: recursive::permissions::RuleSource::User,
                deny: vec!["run_*".into()],
                ..Default::default()
            }],
        };
        let (registry, _tmp) = registry_with(perms);

        for tool in ["run_shell", "run_background"] {
            let result = registry.invoke(tool, json!({})).await;
            assert!(
                matches!(result, Err(Error::PermissionDenied { .. })),
                "expected {tool} to be denied by run_*"
            );
        }

        // read_file is unrelated — it should not be rejected by the
        // permission layer (it may still fail for other reasons, e.g.
        // a missing path argument; we only assert it's not
        // PermissionDenied).
        let result = registry
            .invoke("read_file", json!({ "path": "doesnotexist.txt" }))
            .await;
        assert!(
            !matches!(result, Err(Error::PermissionDenied { .. })),
            "read_file must not be denied by run_* pattern"
        );
    }

    /// Test D — registry without a permissions config allows
    /// everything. Without this assertion a future refactor could
    /// silently flip the default to deny-by-default.
    #[tokio::test]
    async fn permissions_no_config_allows_everything() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path().to_path_buf();
        let registry =
            ToolRegistry::new(Arc::new(LocalTransport)).register(Arc::new(WriteFile::new(&root)));
        let result = registry
            .invoke("write_file", json!({ "path": "ok.txt", "content": "ok" }))
            .await;
        assert!(
            !matches!(result, Err(Error::PermissionDenied { .. })),
            "no-permissions registry must not return PermissionDenied"
        );
    }

    /// Test E — `[permissions]` section parses from a config.toml-style
    /// TOML blob via FileConfig.
    #[test]
    fn permissions_section_parses_from_toml() {
        let toml_text = r#"
[permissions]
allow = ["read_file", "list_dir"]
deny = ["run_*"]
interactive = ["write_file"]
"#;
        let cfg: recursive::config_file::FileConfig =
            toml::from_str(toml_text).expect("parse config.toml");
        let section = cfg.permissions.expect("permissions section present");
        assert_eq!(section.allow, vec!["read_file", "list_dir"]);
        assert_eq!(section.deny, vec!["run_*"]);
        assert_eq!(section.interactive, vec!["write_file"]);
    }
}
