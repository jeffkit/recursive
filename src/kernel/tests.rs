use super::*;
use crate::llm::MockProvider;

// -- Builder tests ------------------------------------------------------

#[test]
fn kernel_builder_requires_llm() {
    let result = AgentKernel::builder().build();
    assert!(result.is_err());
    match result {
        Err(e) => assert!(e.to_string().contains("llm provider is required")),
        Ok(_) => panic!("expected Err"),
    }
}

#[test]
fn kernel_builder_happy_path() {
    let mock = MockProvider::default();
    let tools = ToolRegistry::local();
    let kernel = AgentKernel::builder()
        .llm(Arc::new(mock))
        .tools(tools)
        .max_steps(16)
        .build()
        .expect("build should succeed");
    assert_eq!(kernel.max_steps, 16);
}

#[test]
fn kernel_builder_default_max_steps() {
    let mock = MockProvider::default();
    let tools = ToolRegistry::local();
    let kernel = AgentKernel::builder()
        .llm(Arc::new(mock))
        .tools(tools)
        .build()
        .expect("build should succeed");
    assert_eq!(kernel.max_steps, 0);
}

// -- Clone / with_tools tests ------------------------------------------

#[test]
fn kernel_clone_is_independent() {
    let mock = MockProvider::default();
    let tools1 = ToolRegistry::local();
    let kernel = AgentKernel::builder()
        .llm(Arc::new(mock))
        .tools(tools1)
        .build()
        .expect("build should succeed");

    let mut cloned = kernel.clone();
    // Modify the clone's tools by creating a new registry
    let new_tools = ToolRegistry::local();
    cloned.tools = new_tools;

    // The original should still have its original tools
    // (we can't compare ToolRegistry directly, but we can check
    // that the clone's tools are different by checking the transport)
    assert!(!Arc::ptr_eq(
        kernel.tools().transport(),
        cloned.tools().transport()
    ));
}

#[test]
fn kernel_with_tools_preserves_llm() {
    let mock = MockProvider::default();
    let mock_arc = Arc::new(mock);
    let tools1 = ToolRegistry::local();
    let kernel = AgentKernel::builder()
        .llm(mock_arc.clone())
        .tools(tools1)
        .build()
        .expect("build should succeed");

    let tools2 = ToolRegistry::local();
    let new_kernel = kernel.with_tools(tools2);

    // LLM provider should be the same Arc
    assert!(Arc::ptr_eq(&kernel.llm, &new_kernel.llm));
    // max_steps should be preserved
    assert_eq!(new_kernel.max_steps, kernel.max_steps);
}

// -- AgentKernel::run() tests -------------------------------------------

fn make_minimal_ctx(messages: Vec<Message>) -> TurnContext {
    use std::sync::atomic::AtomicBool;
    TurnContext {
        messages: Arc::new(messages),
        step_events_tx: None,
        tool_specs: vec![],
        streaming: false,
        permission_hook: None,
        exploring_plan_mode: Arc::new(AtomicBool::new(false)),
        permission_mode: crate::permissions::PermissionMode::Default,
        mailbox: None,
        turn: 0,
        prompt_segments: None,
        wall_timeout_secs: 0,
    }
}

/// Kills: `replace > with ==` (and `replace > with <`) at line 313.
///
/// After a simple one-reply turn, `new_messages` must contain the reply.
/// With `== input_len`, the condition `inner.messages.len() == input_len`
/// is false when a reply was added (len > input_len), so `new_messages`
/// would be empty.
#[tokio::test]
async fn kernel_run_new_messages_contains_reply() {
    use crate::llm::Completion;
    let provider = Arc::new(MockProvider::new(vec![Completion {
        content: "done".to_string(),
        tool_calls: vec![],
        finish_reason: Some("stop".to_string()),
        usage: None,
        reasoning_content: None,
    }]));
    let kernel = AgentKernel::builder()
        .llm(provider)
        .max_steps(1)
        .build()
        .expect("build");

    let ctx = make_minimal_ctx(vec![Message::user("hello".to_string())]);
    let outcome = kernel.run(ctx).await.expect("run");

    assert_eq!(
        outcome.new_messages.len(),
        1,
        "new_messages must contain exactly the assistant reply; got {:?}",
        outcome.new_messages
    );
    assert_eq!(outcome.new_messages[0].content, "done");
}

/// Kills: `replace && with ||` at line 318.
///
/// When there is NO compaction summary, the first input message must NOT
/// be prepended to `new_messages`.  With `||`, the condition becomes
/// `!inner.messages.is_empty() || ...` which is true for any non-empty
/// messages list, causing the first message to ALWAYS be prepended.
#[tokio::test]
async fn kernel_run_does_not_prepend_input_to_new_messages() {
    use crate::llm::Completion;
    let provider = Arc::new(MockProvider::new(vec![Completion {
        content: "answer".to_string(),
        tool_calls: vec![],
        finish_reason: Some("stop".to_string()),
        usage: None,
        reasoning_content: None,
    }]));
    let kernel = AgentKernel::builder()
        .llm(provider)
        .max_steps(1)
        .build()
        .expect("build");

    let input = Message::user("question".to_string());
    let ctx = make_minimal_ctx(vec![input.clone()]);
    let outcome = kernel.run(ctx).await.expect("run");

    // new_messages must contain ONLY the assistant reply, not the input.
    assert_eq!(outcome.new_messages.len(), 1, "only reply expected");
    assert_eq!(
        outcome.new_messages[0].content, "answer",
        "first new message must be the reply, not the input"
    );
    assert!(
        outcome.new_messages[0].content != "question",
        "input must not appear in new_messages"
    );
}

/// Kills: `delete !` on the compaction-summary prepend guard — when the
/// first message is NOT a compaction summary, it must not be prepended.
#[tokio::test]
async fn kernel_run_does_not_prepend_non_summary_first_message() {
    use crate::llm::Completion;
    let provider = Arc::new(MockProvider::new(vec![Completion {
        content: "reply".to_string(),
        tool_calls: vec![],
        finish_reason: Some("stop".to_string()),
        usage: None,
        reasoning_content: None,
    }]));
    let kernel = AgentKernel::builder()
        .llm(provider)
        .max_steps(1)
        .build()
        .expect("build");

    let system = Message::system("sys".to_string());
    assert!(
        !system.is_compaction_summary,
        "fixture must not be a compaction summary"
    );
    let ctx = make_minimal_ctx(vec![system, Message::user("q".to_string())]);
    let outcome = kernel.run(ctx).await.expect("run");
    assert_eq!(outcome.new_messages.len(), 1);
    assert_eq!(outcome.new_messages[0].content, "reply");
    assert!(
        outcome.new_messages.iter().all(|m| m.content != "sys"),
        "non-summary first message must not be prepended; got {:?}",
        outcome.new_messages
    );
}

#[tokio::test]
async fn kernel_run_prepends_compaction_summary_to_new_messages() {
    use crate::compact::Compactor;
    use crate::llm::Completion;
    let messages = vec![
        Message::system("sys".to_string()),
        Message::user("u1".to_string()),
        Message::assistant("a1".to_string()),
        Message::user("u2".to_string()),
        Message::assistant("a2".to_string()),
        Message::user("u3".to_string()),
    ];
    let provider = Arc::new(MockProvider::new(vec![
        Completion {
            content: "compact summary".to_string(),
            tool_calls: vec![],
            finish_reason: Some("stop".to_string()),
            usage: None,
            reasoning_content: None,
        },
        Completion {
            content: "final".to_string(),
            tool_calls: vec![],
            finish_reason: Some("stop".to_string()),
            usage: None,
            reasoning_content: None,
        },
    ]));
    let kernel = AgentKernel::builder()
        .llm(provider)
        .compactor(Compactor::new(0).keep_recent_n(2))
        .max_steps(1)
        .build()
        .expect("build");

    let ctx = make_minimal_ctx(messages);
    let outcome = kernel.run(ctx).await.expect("run");

    assert!(
        !outcome.new_messages.is_empty(),
        "expected new messages after compaction + reply"
    );
    assert!(
        outcome.new_messages[0].is_compaction_summary,
        "compaction summary must be prepended to new_messages; got {:?}",
        outcome.new_messages
    );
}

#[test]
fn kernel_builder_max_transcript_chars_stored_and_chains() {
    let mock = Arc::new(MockProvider::default());
    let kernel = AgentKernel::builder()
        .llm(mock)
        .max_transcript_chars(12_345)
        .max_steps(3)
        .build()
        .expect("build");
    assert_eq!(
        kernel.max_transcript_chars,
        Some(12_345),
        "max_transcript_chars must survive builder chaining"
    );
    assert_eq!(
        kernel.max_steps, 3,
        "builder chain must not reset max_steps"
    );
}

// -- AgentKernelBuilder Debug fmt test ----------------------------------

#[test]
fn agent_kernel_builder_debug_contains_field_names() {
    // kills `replace <impl std::fmt::Debug for AgentKernelBuilder>::fmt
    //         -> std::fmt::Result with Ok(Default::default())`
    // With the mutant the formatter produces no output; the assertion fails.
    let builder = AgentKernel::builder().max_steps(42);
    let dbg = format!("{:?}", builder);
    assert!(
        dbg.contains("AgentKernelBuilder"),
        "Debug output must contain struct name; got: {dbg}"
    );
    assert!(
        dbg.contains("max_steps"),
        "Debug output must contain max_steps field; got: {dbg}"
    );
}

#[test]
fn kernel_builder_stuck_window_and_error_rate() {
    // kills mutations to `unwrap_or(10)` and `unwrap_or(0.8)` defaults
    let mock = MockProvider::default();
    let kernel_defaults = AgentKernel::builder().llm(Arc::new(mock)).build().unwrap();
    assert_eq!(
        kernel_defaults.stuck_window, 10,
        "default stuck_window must be 10"
    );
    assert!(
        (kernel_defaults.stuck_error_rate - 0.8).abs() < 1e-10,
        "default stuck_error_rate must be 0.8"
    );

    let mock2 = MockProvider::default();
    let kernel_custom = AgentKernel::builder()
        .llm(Arc::new(mock2))
        .stuck_window(5)
        .stuck_error_rate(0.5)
        .build()
        .unwrap();
    assert_eq!(kernel_custom.stuck_window, 5);
    assert!((kernel_custom.stuck_error_rate - 0.5).abs() < 1e-10);
}

#[test]
fn kernel_accessor_methods() {
    // kills accessor method-replacement mutations
    let mock = Arc::new(MockProvider::default());
    let kernel = AgentKernel::builder().llm(mock.clone()).build().unwrap();
    // llm() returns the same Arc
    assert!(
        Arc::ptr_eq(&kernel.llm, kernel.llm()),
        "llm() must return &self.llm"
    );
    // tools() returns the registry
    let _ = kernel.tools();
    // hooks() returns hook registry
    let _ = kernel.hooks();
    // storage() returns storage backend
    let _ = kernel.storage();
    // session_store() returns session store
    let _ = kernel.session_store();
    // shutdown_token is None when not set
    assert!(
        kernel.shutdown_token().is_none(),
        "no token must be set by default"
    );
}

#[test]
fn kernel_max_steps_zero_by_default() {
    // kills `self.max_steps.unwrap_or(0)` → `unwrap_or(1)` mutation
    let mock = Arc::new(MockProvider::default());
    let kernel = AgentKernel::builder().llm(mock).build().unwrap();
    assert_eq!(
        kernel.max_steps, 0,
        "default max_steps must be 0 (unlimited)"
    );
}

#[test]
fn kernel_max_steps_custom_value() {
    // kills `max_steps.unwrap_or(...)` mutation
    let mock = Arc::new(MockProvider::default());
    let kernel = AgentKernel::builder()
        .llm(mock)
        .max_steps(25)
        .build()
        .unwrap();
    assert_eq!(kernel.max_steps, 25, "custom max_steps must be stored");
}

#[test]
fn with_tools_replaces_registry() {
    // kills `fn with_tools` function-replacement mutation
    use crate::tools::transport::LocalTransport;
    let mock = Arc::new(MockProvider::default());
    let kernel = AgentKernel::builder().llm(mock.clone()).build().unwrap();
    // The local registry has tools; create an empty one to swap in.
    let empty_reg = ToolRegistry::new(Arc::new(LocalTransport));
    let replaced = kernel.with_tools(empty_reg);
    // The replaced kernel must use the new (empty) registry.
    // ToolRegistry::local() registers many tools; our empty one has none.
    assert_eq!(
        replaced.tools().names().len(),
        0,
        "with_tools must swap in the empty registry"
    );
}

// -- TurnOutcome tests --------------------------------------------------

#[test]
fn turn_outcome_default_values() {
    let outcome = TurnOutcome {
        new_messages: vec![],
        final_text: None,
        finish_reason: FinishReason::NoMoreToolCalls,
        usage: TokenUsage::default(),
        last_prompt_tokens: 0,
        llm_latency_ms: 0,
        steps: 0,
        tool_audits: std::collections::HashMap::new(),
        turn: 0,
    };
    assert!(outcome.new_messages.is_empty());
    assert!(outcome.final_text.is_none());
    assert_eq!(outcome.finish_reason, FinishReason::NoMoreToolCalls);
    assert_eq!(outcome.usage, TokenUsage::default());
    assert_eq!(outcome.llm_latency_ms, 0);
    assert_eq!(outcome.steps, 0);
}

// -- Goal 399: wall_timeout_secs wiring --------------------------------------

/// Provider that stalls before its first N responses (delegates to a
/// scripted `MockProvider` afterwards). Lets tests drive the wall-clock
/// deadline deterministically without real time budgets beyond ~2s.
struct SlowFirstCallProvider {
    inner: MockProvider,
    delay: std::time::Duration,
    remaining_slow_calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl crate::llm::ChatProvider for SlowFirstCallProvider {
    async fn complete(
        &self,
        messages: &[crate::message::Message],
        tools: &[crate::llm::ToolSpec],
    ) -> crate::error::Result<crate::llm::Completion> {
        use std::sync::atomic::Ordering;
        if self.remaining_slow_calls.fetch_sub(1, Ordering::SeqCst) > 0 {
            tokio::time::sleep(self.delay).await;
        }
        self.inner.complete(messages, tools).await
    }
}

/// Trivial registered tool so a scripted completion can request a second
/// step (the wall deadline is checked at the top of each step).
struct NoopTool;

#[async_trait::async_trait]
impl crate::tools::Tool for NoopTool {
    fn spec(&self) -> crate::llm::ToolSpec {
        crate::llm::ToolSpec {
            name: "noop".into(),
            description: "does nothing".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }
    }
    async fn execute(&self, _args: serde_json::Value) -> crate::error::Result<String> {
        Ok("ok".into())
    }
}

fn slow_provider(
    script: Vec<crate::llm::Completion>,
    slow_calls: usize,
) -> Arc<SlowFirstCallProvider> {
    Arc::new(SlowFirstCallProvider {
        inner: MockProvider::new(script),
        delay: std::time::Duration::from_secs(2),
        remaining_slow_calls: std::sync::atomic::AtomicUsize::new(slow_calls),
    })
}

fn two_tool_call_script() -> Vec<crate::llm::Completion> {
    let tool_call = |id: &str| crate::llm::ToolCall {
        id: id.into(),
        name: "noop".into(),
        arguments: serde_json::json!({}),
    };
    vec![
        crate::llm::Completion {
            content: "step".into(),
            tool_calls: vec![tool_call("c1")],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
        crate::llm::Completion {
            content: "again".into(),
            tool_calls: vec![tool_call("c2")],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            reasoning_content: None,
        },
    ]
}

/// Goal 399: the kernel-level budget is the DEFAULT for turns whose context
/// leaves `wall_timeout_secs` at 0 — the exact shape the runtime wrapper
/// produces after `AgentRuntimeBuilder::wall_timeout_secs`.
#[tokio::test]
async fn kernel_wall_budget_fires_when_ctx_leaves_it_zero() {
    let kernel = AgentKernel::builder()
        .llm(slow_provider(two_tool_call_script(), 1) as Arc<dyn ChatProvider>)
        .tools(ToolRegistry::local().register(Arc::new(NoopTool)))
        .max_steps(10)
        .wall_timeout_secs(1)
        .build()
        .expect("build");

    let ctx = make_minimal_ctx(vec![Message::user("go".to_string())]);
    assert_eq!(ctx.wall_timeout_secs, 0, "precondition: ctx unset");
    let outcome = kernel.run(ctx).await.expect("wall finish is Ok data");
    assert!(matches!(
        outcome.finish_reason,
        crate::agent::FinishReason::WallClockExceeded { secs: 1 }
    ));
}

/// Goal 399: an explicit per-turn `ctx.wall_timeout_secs` wins over the
/// kernel-level default (1s ctx budget fires even though kernel allows 60s).
#[tokio::test]
async fn kernel_ctx_budget_overrides_kernel_default() {
    let kernel = AgentKernel::builder()
        .llm(slow_provider(two_tool_call_script(), 1) as Arc<dyn ChatProvider>)
        .tools(ToolRegistry::local().register(Arc::new(NoopTool)))
        .max_steps(10)
        .wall_timeout_secs(60)
        .build()
        .expect("build");

    let mut ctx = make_minimal_ctx(vec![Message::user("go".to_string())]);
    ctx.wall_timeout_secs = 1;
    let outcome = kernel.run(ctx).await.expect("wall finish is Ok data");
    assert!(matches!(
        outcome.finish_reason,
        crate::agent::FinishReason::WallClockExceeded { secs: 1 }
    ));
}

/// Goal 399: builder default is 0 (unlimited) — the legacy contract.
#[test]
fn kernel_builder_wall_timeout_default_is_zero() {
    let builder = AgentKernelBuilder::default();
    assert_eq!(builder.wall_timeout_secs, 0);
    let kernel = builder
        .llm(Arc::new(MockProvider::new(vec![])) as Arc<dyn ChatProvider>)
        .build()
        .expect("build");
    assert_eq!(kernel.wall_timeout_secs, 0);
}
