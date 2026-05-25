# Goal 74 — Parallel Tool Execution

**Roadmap**: Phase 8.1 — Parallel tool execution

**Design principle check**:
- Implemented as: change to agent loop's tool execution phase in `agent.rs`.
  This IS a loop change but minimal — only the execution dispatch, not
  the decision logic.

## Why

When the LLM requests multiple tool calls in a single response (common
with capable models), they currently execute sequentially. Independent
tool calls (e.g., reading two files) can safely run in parallel, reducing
wall-clock latency significantly.

## Scope (do exactly this, no more)

### 1. `src/agent.rs` — parallel execution of tool calls

When a completion returns multiple tool calls:
1. Check if they can run in parallel (see safety rules below)
2. If yes: spawn all via `tokio::join!` or `futures::join_all`
3. If no: execute sequentially (current behavior)

**Safety rules for parallel execution**:
- Tools that WRITE (`write_file`, `apply_patch`, `run_shell`) must run
  sequentially — they have side effects that may conflict
- Tools that only READ (`read_file`, `list_dir`, `search_files`,
  `estimate_tokens`, `load_skill`) can run in parallel
- Default: sequential (safe fallback)

### 2. Tool trait — add `is_readonly` method

```rust
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn execute(&self, arguments: Value) -> Result<String>;

    /// Whether this tool only reads data without side effects.
    /// Default: false (conservative). Override to true for read-only tools.
    fn is_readonly(&self) -> bool { false }
}
```

Mark these tools as `is_readonly() -> true`:
- `read_file`, `list_dir`, `search_files`, `estimate_tokens`, `load_skill`

### 3. Parallel dispatch logic

```rust
// In the agent loop, after getting tool_calls from LLM:
let all_readonly = tool_calls.iter().all(|tc| registry.get(&tc.name).map_or(false, |t| t.is_readonly()));

let results = if all_readonly && tool_calls.len() > 1 {
    // Parallel execution
    let futures: Vec<_> = tool_calls.iter().map(|tc| {
        let tool = registry.get(&tc.name).unwrap();
        tool.execute(tc.arguments.clone())
    }).collect();
    futures::future::join_all(futures).await
} else {
    // Sequential (current behavior)
    let mut results = Vec::new();
    for tc in &tool_calls {
        results.push(registry.get(&tc.name).unwrap().execute(tc.arguments.clone()).await);
    }
    results
};
```

### 4. Tests

- Test: single tool call executes normally (regression)
- Test: multiple read-only calls execute (verify all complete)
- Test: mixed read/write calls execute sequentially
- Test: `is_readonly` returns correct values for built-in tools
- Test: parallel execution is actually parallel (use timing assertions
  with a mock tool that sleeps)

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- Multiple read-only tool calls execute concurrently
- Write tools still execute sequentially (safety preserved)
- No regressions

## Notes for the agent

- Read `src/agent.rs` for the tool execution loop. Find where tool_calls
  are iterated and results collected.
- Read `src/tools/mod.rs` for the `Tool` trait definition.
- You may need to add `futures` crate if not already in deps. Check
  `Cargo.toml` first. If `tokio` is available, `tokio::join!` works
  for a fixed number of futures; for dynamic count, use
  `futures::future::join_all` or `tokio::task::JoinSet`.
- The key insight: `is_readonly` is a HINT from the tool. If wrong
  (a tool claims readonly but has side effects), the worst case is
  a race condition — but we control all built-in tools, so we know.
- For the timing test: create a `SlowReadTool` that sleeps 100ms.
  Two parallel calls should complete in ~100ms, not ~200ms.
