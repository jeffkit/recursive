# Agent Runtime Builder

The `AgentRuntimeBuilder` provides a fluent API for constructing an `AgentRuntime`.

## Builder options

```rust
let mut runtime = AgentRuntime::builder()
    .llm(llm_provider)           // required: Arc<dyn LlmProvider>
    .tools(tool_registry)        // optional: ToolRegistry
    .max_steps(20)               // optional: step budget (default 32)
    .system_prompt("...")        // optional: custom system prompt string
    .event_sink(my_sink)         // optional: Arc<dyn EventSink> observer
    .build()?;
```

## Running the agent

```rust
// Run to completion
let outcome = runtime.run("your goal here").await?;

// Access the result
match outcome.finish_reason {
    FinishReason::NoMoreToolCalls => {
        println!("{}", outcome.final_text.unwrap_or_default());
    }
    FinishReason::BudgetExceeded => {
        eprintln!("Agent hit step budget");
    }
    FinishReason::Stuck { repeated_call, repeats } => {
        eprintln!("Agent stuck: {repeated_call} repeated {repeats}x");
    }
    FinishReason::ProviderStop(reason) => {
        println!("Provider stopped: {reason}");
        println!("{}", outcome.final_text.unwrap_or_default());
    }
    _ => {}
}
```

## RuntimeOutcome

```rust
pub struct RuntimeOutcome {
    pub finish_reason: FinishReason,
    pub final_text: Option<String>,
    pub steps: usize,
}
```

## FinishReason

```rust
pub enum FinishReason {
    NoMoreToolCalls,                              // model stopped calling tools
    BudgetExceeded,                               // max_steps reached
    ProviderStop(String),                         // LLM returned a stop signal
    Stuck { repeated_call: String, repeats: usize }, // same tool call looping
    TranscriptLimit { chars: usize, limit: usize },  // transcript too large
    PlanPending,                                  // agent paused for plan approval
    Cancelled,                                    // run was cancelled externally
    PermissionDenialLimit,                        // too many permission denials
}
```

> **Note:** Errors during the run are returned as `Err(...)` from `runtime.run()`, not as a `FinishReason` variant. Use `?` or `match` on the `Result` to handle them.
