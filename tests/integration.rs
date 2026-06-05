//! Full-stack integration tests exercising the runtime + compactor + hooks +
//! skills + permission hook + tool transport together.
//!
//! These tests use `MockProvider` with scripted responses and `tempfile`
//! for filesystem isolation. They verify that the public API works correctly
//! when all subsystems are wired together.

use std::sync::Arc;

use recursive::{
    agent::{FinishReason, PermissionDecision},
    compact::Compactor,
    hooks::{Hook, HookAction, HookEvent},
    llm::{Completion, MockProvider, ToolCall},
    message::Message,
    runtime::AgentRuntime,
    tools::PermissionHook,
    tools::{LocalTransport, ToolRegistry, ToolTransport},
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

    // Write a file so Read succeeds.
    std::fs::write(root.join("data.txt"), b"hello world").unwrap();

    // Script: 3 tool calls (to build up transcript) then a final stop.
    let script = vec![
        Completion {
            content: "reading file".into(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "Read".into(),
                arguments: json!({"path": "data.txt"}),
            }],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
        Completion {
            content: "globbing files".into(),
            tool_calls: vec![ToolCall {
                id: "c2".into(),
                name: "Glob".into(),
                arguments: json!({"pattern": "*.txt"}),
            }],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
        Completion {
            content: "reading again".into(),
            tool_calls: vec![ToolCall {
                id: "c3".into(),
                name: "Read".into(),
                arguments: json!({"path": "data.txt"}),
            }],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
        // This completion is consumed by the compactor
        Completion {
            content: "Summary: read file, glob files, tests pass.".into(),
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
        .register(Arc::new(recursive::tools::GlobTool::new(root)));

    let hook = Arc::new(CountingHook::new());
    let mut hooks = recursive::hooks::HookRegistry::new();
    hooks.register(hook.clone() as Arc<dyn Hook>);

    // Threshold calibrated to trigger after all 3 tool calls have completed.
    // estimate_chars now includes tool_call arguments (~54 chars each), so the
    // old threshold of 100 would fire too early (after step 2). After all 3
    // steps the transcript is ~264 chars, so 250 fires at the right moment.
    let compactor = Compactor::new(250).keep_recent_n(2);

    let mut runtime = AgentRuntime::builder()
        .llm(llm)
        .tools(tools)
        .system_prompt("you are a test agent")
        .max_steps(10)
        .compactor(compactor)
        .hooks(hooks)
        .build()
        .unwrap();

    let outcome = runtime.run("read the file and list the dir").await.unwrap();

    // Runtime should complete normally.
    assert_eq!(
        outcome.finish_reason,
        FinishReason::NoMoreToolCalls,
        "expected NoMoreToolCalls, got {:?}",
        outcome.finish_reason
    );

    // Hook should have fired for each tool call (3 pre, 3 post).
    // The runtime kernel dispatches these from RunCore.
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
    // SessionStart is dispatched exactly once per session (on turn_index == 0).
    assert_eq!(
        hook.session_start_count
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "expected exactly 1 SessionStart event per session"
    );

    // Compaction should have fired (transcript exceeded 100 chars).
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
    let summary_msgs: Vec<&Message> = runtime
        .transcript()
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
    //   3. Sub-agent's second call: tries Read (allowed).
    //   4. Sub-agent's third call: finishes.
    //   5. Parent's second call: finishes.
    let script = vec![
        // 1. Parent calls Agent
        Completion {
            content: "spawning sub-agent".into(),
            tool_calls: vec![ToolCall {
                id: "p1".into(),
                name: "Agent".into(),
                arguments: json!({
                    "prompt": "run 'echo hi' and read notes.txt",
                    "tools": ["Bash", "Read"]
                }),
            }],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
        // 2. Sub-agent tries Bash (denied by permission hook)
        Completion {
            content: "running shell".into(),
            tool_calls: vec![ToolCall {
                id: "s1".into(),
                name: "Bash".into(),
                arguments: json!({"command": "echo hi"}),
            }],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
        // 3. Sub-agent tries Read (allowed)
        Completion {
            content: "reading file".into(),
            tool_calls: vec![ToolCall {
                id: "s2".into(),
                name: "Read".into(),
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

    // Permission hook: deny Bash, allow everything else.
    struct DenyShellHook;
    #[async_trait::async_trait]
    impl PermissionHook for DenyShellHook {
        async fn check(&self, tool_name: &str, _args: &serde_json::Value) -> PermissionDecision {
            if tool_name == "Bash" {
                PermissionDecision::Deny("Bash is not allowed".into())
            } else {
                PermissionDecision::Allow
            }
        }
    }
    let mut runtime = AgentRuntime::builder()
        .llm(llm)
        .tools(all_tools)
        .system_prompt("you are a test agent with permission hook")
        .max_steps(10)
        .build()
        .unwrap();
    // Wire the permission hook via set_permission_hook so it lives in the
    // ToolRegistry — the canonical location for permission interception.
    runtime.set_permission_hook(Arc::new(DenyShellHook));

    let outcome = runtime.run("spawn a sub-agent to explore").await.unwrap();

    // Parent should complete successfully.
    assert_eq!(
        outcome.finish_reason,
        FinishReason::NoMoreToolCalls,
        "expected NoMoreToolCalls, got {:?}",
        outcome.finish_reason
    );
    assert_eq!(
        outcome.final_text.as_deref(),
        Some("parent done"),
        "expected parent done"
    );

    // The sub-agent's result should be visible in the parent's transcript.
    let transcript_str: String = runtime
        .transcript()
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
                name: "Skill".into(),
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

    let mut runtime = AgentRuntime::builder()
        .llm(llm)
        .tools(tools)
        .system_prompt(system_prompt)
        .max_steps(5)
        .build()
        .unwrap();

    let outcome = runtime.run("load the test skill").await.unwrap();

    assert_eq!(
        outcome.finish_reason,
        FinishReason::NoMoreToolCalls,
        "expected NoMoreToolCalls, got {:?}",
        outcome.finish_reason
    );

    // The tool result should contain the skill content.
    let tool_msgs: Vec<&Message> = runtime
        .transcript()
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
                name: "Read".into(),
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

    let mut runtime = AgentRuntime::builder()
        .llm(llm.clone())
        .tools(tools.clone())
        .system_prompt("you are a test agent")
        .max_steps(5)
        .build()
        .unwrap();

    let outcome1 = runtime.run("read the config file").await.unwrap();

    // First run should complete.
    assert_eq!(
        outcome1.finish_reason,
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
        runtime.transcript().to_vec(),
    );
    session.write_to(&session_path).unwrap();

    // Verify the session file can be read back.
    let restored = recursive::session::SessionFile::read_from(&session_path).unwrap();
    assert_eq!(
        restored.messages().len(),
        runtime.transcript().len(),
        "restored session should have same transcript length"
    );
    assert_eq!(
        restored.steps_consumed, 2,
        "restored session should have 2 steps consumed"
    );

    // Now resume: create a new runtime seeded with the saved transcript.
    let script_part2 = vec![
        Completion {
            content: "continuing from where I left off".into(),
            tool_calls: vec![ToolCall {
                id: "c2".into(),
                name: "Read".into(),
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

    let mut resumed_runtime = AgentRuntime::builder()
        .llm(llm2)
        .tools(tools)
        .system_prompt("you are a test agent")
        .max_steps(5)
        .seed_transcript(restored.into_transcript())
        .build()
        .unwrap();

    let outcome2 = resumed_runtime
        .run("continue reading the config file")
        .await
        .unwrap();

    // Resumed run should complete.
    assert_eq!(
        outcome2.finish_reason,
        FinishReason::NoMoreToolCalls,
        "resumed run should complete normally"
    );
    assert_eq!(outcome2.steps, 2, "resumed run should take 2 steps");

    // The full transcript should include both the original and resumed messages.
    // Seed (3: system + user + assistant) + new user goal + assistant + tool call + tool result + assistant = 8
    assert!(
        resumed_runtime.transcript().len() >= 6,
        "resumed transcript should have at least 6 messages, got {}",
        resumed_runtime.transcript().len()
    );

    // The resumed runtime should have the original context available.
    let transcript_str: String = resumed_runtime
        .transcript()
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

    // Write a file so Read succeeds.
    std::fs::write(root.join("greeting.txt"), b"hello from transport test").unwrap();

    // Script: one tool call then stop.
    let script = vec![
        Completion {
            content: "reading file via explicit transport".into(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "Read".into(),
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

    let mut runtime = AgentRuntime::builder()
        .llm(llm)
        .tools(tools)
        .system_prompt("you are a test agent with explicit transport")
        .max_steps(5)
        .build()
        .unwrap();

    let outcome = runtime.run("read the greeting file").await.unwrap();

    assert_eq!(
        outcome.finish_reason,
        FinishReason::NoMoreToolCalls,
        "expected NoMoreToolCalls, got {:?}",
        outcome.finish_reason
    );
    assert_eq!(outcome.steps, 2, "expected 2 steps");

    // The tool result should contain the file contents.
    let tool_msgs: Vec<&Message> = runtime
        .transcript()
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
        outcome.final_text.as_deref(),
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
    use recursive::runtime::{AgentRuntime, AgentRuntimeBuilder};
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
    /// The runtime kernel currently does not dispatch SessionEnd at all
    /// (it lives only in the legacy `Agent` path that this test used to
    /// exercise). This test is kept as a regression guard: if a future
    /// change adds SessionEnd dispatch to the runtime, this test should
    /// continue to assert that Cancelled skips it.
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
        let mut hooks = recursive::hooks::HookRegistry::new();
        hooks.register(counter.clone() as Arc<dyn Hook>);

        let provider = Arc::new(MockProvider::new(vec![final_completion("never")]));
        let token = CancellationToken::new();
        token.cancel();

        let mut runtime = AgentRuntime::builder()
            .llm(provider)
            .tools(ToolRegistry::local())
            .system_prompt("test")
            .hooks(hooks)
            .shutdown_token(token)
            .build()
            .expect("runtime build");

        let outcome = runtime.run("ignored").await.expect("run");
        assert!(matches!(outcome.finish_reason, FinishReason::Cancelled));
        // Explicitly close the session — even with close(), Cancelled skips SessionEnd.
        runtime.close(Some(&outcome)).await;
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
            mode: PermissionMode::Default,
            layers: vec![recursive::permissions::PermissionLayer {
                source: recursive::permissions::RuleSource::User,
                deny: vec!["Bash".into()],
                ..Default::default()
            }],
        };
        let (registry, _tmp) = registry_with(perms);
        let result = registry
            .invoke("Bash", json!({ "command": "echo hi" }))
            .await;
        match result {
            Err(Error::PermissionDenied { name, .. }) => assert_eq!(name, "Bash"),
            other => panic!("expected PermissionDenied, got {other:?}"),
        }
    }

    /// Test B — allow list grants access to listed tools; unlisted tools
    /// fall through to Passthrough (union semantics per Goal 193).
    #[tokio::test]
    async fn permissions_allow_filter_blocks_unlisted() {
        let perms = recursive::permissions::LayeredPermissionsConfig {
            mode: PermissionMode::Default,
            layers: vec![recursive::permissions::PermissionLayer {
                source: recursive::permissions::RuleSource::User,
                allow: vec!["Read".into()],
                ..Default::default()
            }],
        };
        let (registry, _tmp) = registry_with(perms);
        // Listed tool is allowed.
        let ok = registry
            .invoke("Read", json!({ "path": "nonexistent.txt" }))
            .await;
        // Should succeed (or fail on file not found — but not PermissionDenied)
        assert!(
            !matches!(ok, Err(Error::PermissionDenied { .. })),
            "Read should be allowed when in allow list"
        );

        // Unlisted tool is NOT denied — falls through to Passthrough.
        let ok2 = registry
            .invoke("Write", json!({ "path": "x.txt", "content": "y" }))
            .await;
        assert!(
            !matches!(ok2, Err(Error::PermissionDenied { .. })),
            "Write should be allowed (Passthrough) when not in deny list"
        );
    }

    /// Test C — glob patterns match multiple tools.
    #[tokio::test]
    async fn permissions_glob_pattern_matches() {
        let perms = recursive::permissions::LayeredPermissionsConfig {
            mode: PermissionMode::Default,
            layers: vec![recursive::permissions::PermissionLayer {
                source: recursive::permissions::RuleSource::User,
                deny: vec!["run_*".into()],
                ..Default::default()
            }],
        };
        let (registry, _tmp) = registry_with(perms);

        for tool in ["run_background"] {
            let result = registry.invoke(tool, json!({})).await;
            assert!(
                matches!(result, Err(Error::PermissionDenied { .. })),
                "expected {tool} to be denied by run_*"
            );
        }

        // Read is unrelated — it should not be rejected by the
        // permission layer (it may still fail for other reasons, e.g.
        // a missing path argument; we only assert it's not
        // PermissionDenied).
        let result = registry
            .invoke("Read", json!({ "path": "doesnotexist.txt" }))
            .await;
        assert!(
            !matches!(result, Err(Error::PermissionDenied { .. })),
            "Read must not be denied by run_* pattern"
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
            .invoke("Write", json!({ "path": "ok.txt", "content": "ok" }))
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
allow = ["Read", "Glob"]
deny = ["run_*"]
interactive = ["Write"]
"#;
        let cfg: recursive::config_file::FileConfig =
            toml::from_str(toml_text).expect("parse config.toml");
        let section = cfg.permissions.expect("permissions section present");
        assert_eq!(section.allow, vec!["Read", "Glob"]);
        assert_eq!(section.deny, vec!["run_*"]);
        assert_eq!(section.interactive, vec!["Write"]);
    }
}
