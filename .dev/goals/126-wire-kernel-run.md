# Goal 126 — Wire AgentKernel::run() to use RunCore

**Roadmap**: Kernel Architecture Refactor — Phase 2b (expose kernel)

**Design principle check**:
- Implemented as: additions to `src/kernel.rs` + minor change to `src/agent.rs`
- Makes `RunCore` and `run_inner()` accessible from `AgentKernel`
- `Agent::run()` continues to work unchanged

## Why

Goal 125 extracted the core loop into `RunCore::run_inner()`. Now we need to
wire `AgentKernel::run()` (defined in `src/kernel.rs`) to actually execute a
turn using that logic. This makes the stateless kernel a real, usable API.

## Scope (do exactly this, no more)

### 1. Make RunCore and RunInnerOutcome pub(crate)

In `src/agent.rs`, change visibility:
```rust
pub(crate) struct RunInnerOutcome { ... }
pub(crate) struct RunCore<'a> { ... }
```

The `RunCore::run_inner()` method should also be `pub(crate)`.

### 2. Implement AgentKernel::run()

In `src/kernel.rs`, add:

```rust
impl AgentKernel {
    /// Execute one turn using the stateless core loop.
    ///
    /// This is the primary API for the Wrapper layer. It constructs a
    /// `RunCore` from the `TurnContext` and delegates to `run_inner()`.
    pub async fn run(&self, ctx: TurnContext) -> crate::error::Result<TurnOutcome> {
        use crate::agent::RunCore;

        // Build a HookRegistry — for now, empty (hooks will be injected
        // from the Wrapper in a later goal)
        let hooks = crate::hooks::HookRegistry::new();

        let core = RunCore {
            messages: ctx.messages,
            llm: self.llm.clone(),
            tools: self.tools.clone(),
            max_steps: self.max_steps,
            max_transcript_chars: None, // managed by Wrapper
            events: None,               // TODO: wire EventSink → mpsc bridge
            streaming: ctx.streaming,
            compactor: None,            // managed by Wrapper
            permission_hook: ctx.permission_hook,
            hooks: &hooks,
            planning_mode: ctx.planning_mode,
            on_message: &None,          // managed by Wrapper
            total_llm_latency_ms: 0,
            plan_buffer: None,
            plan_confirmed: false,
        };

        let inner = core.run_inner().await?;

        Ok(TurnOutcome {
            new_messages: extract_new_messages(&inner.messages, ???),
            // OR: just return all messages and let caller diff
            // Simplest for now: return the full transcript as new_messages
            // (the Wrapper knows what it sent in, so it can diff)
            new_messages: inner.messages,
            final_text: inner.final_message,
            finish_reason: inner.finish_reason,
            usage: inner.total_usage,
            llm_latency_ms: inner.total_llm_latency_ms,
            steps: inner.steps,
            side_effects: Vec::new(),
        })
    }
}
```

**Note**: The `new_messages` field is tricky. The simplest approach for now:
- `TurnContext.messages` has length N (input messages).
- After `run_inner()`, the transcript has length N + M (input + new).
- `new_messages` = `inner.messages[N..]` (skip the input portion).

To implement this, `AgentKernel::run()` should save `let input_len = ctx.messages.len()` before passing messages to RunCore, then slice the result.

### 3. Add a basic integration test

In `src/kernel.rs` tests:

```rust
#[tokio::test]
async fn kernel_run_basic() {
    // Use MockProvider to simulate a single-turn: model says "hello"
    let mock = Arc::new(MockProvider::new(vec![
        Completion {
            content: "Hello!".into(),
            tool_calls: vec![],
            usage: Some(TokenUsage { prompt_tokens: 10, completion_tokens: 5, ..Default::default() }),
            finish_reason: Some("stop".into()),
            reasoning_content: None,
        },
    ]));

    let kernel = AgentKernel::builder()
        .llm(mock)
        .tools(ToolRegistry::new(...))
        .max_steps(10)
        .build()
        .unwrap();

    let ctx = TurnContext {
        messages: vec![
            Message::system("You are helpful.".into()),
            Message::user("Hi".into()),
        ],
        event_sink: Box::new(crate::event::NullSink),
        tool_specs: vec![],
        streaming: false,
        permission_hook: None,
        planning_mode: PlanningMode::default(),
    };

    let outcome = kernel.run(ctx).await.unwrap();
    assert_eq!(outcome.final_text, Some("Hello!".to_string()));
    assert_eq!(outcome.steps, 1);
    assert!(matches!(outcome.finish_reason, FinishReason::NoMoreToolCalls));
}
```

### 4. Wire EventSink → mpsc bridge (optional, nice-to-have)

If time permits, create a small adapter that converts the `Box<dyn EventSink>`
in TurnContext into an `Option<mpsc::UnboundedSender<StepEvent>>` that RunCore
expects. This enables the Kernel to emit events through the new EventSink trait.

If this is too complex for one goal, skip it and document the TODO.

## Acceptance

- `cargo test` green (518+ tests)
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `AgentKernel::run(TurnContext) -> Result<TurnOutcome>` works
- At least one integration test proves the kernel executes a mock turn correctly
- `Agent::run()` still works unchanged (all existing tests pass)
- Only `src/kernel.rs` and `src/agent.rs` modified

## Notes for the agent

- Read `src/agent.rs` to find `struct RunCore` and `struct RunInnerOutcome` — you need to make them `pub(crate)`.
- Read `src/kernel.rs` to see the existing `AgentKernel` struct and its builder.
- Read `src/llm/mock.rs` for `MockProvider` — you'll need it for the test.
- The `ToolRegistry::new()` in tests requires a workspace path. Look at existing tests in `agent.rs` for how to construct one with `tempfile::tempdir()`.
- `RunCore` has a lifetime parameter `<'a>` because it borrows `hooks: &'a HookRegistry`. In `AgentKernel::run()`, create a local `HookRegistry` and borrow it.
- **DO NOT touch** main.rs, http.rs, runner.rs, multi.rs, event.rs.
- **DO NOT remove** the legacy Agent methods or change Agent's public API.
