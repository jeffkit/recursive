# Agent Builder

The `AgentBuilder` provides a fluent API for constructing an `Agent`.

## Builder options

```rust
let agent = Agent::builder()
    .llm(llm_provider)           // required: Arc<dyn LlmProvider>
    .tools(tool_registry)        // optional: ToolRegistry
    .max_steps(20)               // optional: step budget (default 32)
    .system_prompt("...")        // optional: custom system prompt string
    .system_prompt_file("path")  // optional: load system prompt from file
    .workspace("./my-project")   // optional: sandbox root
    .temperature(0.2)            // optional: override temperature
    .on_event(|e| { ... })       // optional: StepEvent observer closure
    .build()?;
```

## Running the agent

```rust
// Run to completion
let outcome = agent.run("your goal here").await?;

// Access the result
match outcome.finish_reason {
    FinishReason::Done => {
        println!("{}", outcome.final_message.unwrap_or_default());
    }
    FinishReason::BudgetExceeded => {
        eprintln!("Agent hit step budget");
    }
    FinishReason::Error(e) => {
        eprintln!("Agent error: {e}");
    }
    _ => {}
}
```

## AgentOutcome

```rust
pub struct AgentOutcome {
    pub finish_reason: FinishReason,
    pub final_message: Option<String>,
    pub steps: usize,
    pub token_usage: Option<TokenUsage>,
    pub cost_usd: Option<f64>,
}
```

## FinishReason

```rust
pub enum FinishReason {
    Done,                    // model produced a final text answer
    BudgetExceeded,          // max_steps reached
    Stuck,                   // same tool call repeated 3×
    NoMoreToolCalls,         // model stopped calling tools
    TranscriptLimit,         // transcript exceeded compaction limit
    Error(RecursiveError),   // unrecoverable error
}
```
