# Goal 317 — Integration test: memory + skill loading pipeline

## Why

The `remember`/`recall`/`forget` tools and the `load_skill` tool are
registered in `build_standard_tools()` and are part of the agent's core
"advanced features" set. However, there are **zero end-to-end integration
tests** that exercise these tools together in a scripted multi-step workflow.

Without integration tests, regressions in:
- `memory::Remember` → `Recall` round-trips (file persistence)
- `load_skill::LoadSkill` discovering and returning skill content
- The `skill_index` → `load_skill` → use-skill pipeline

...will only surface in E2E runs or user reports, not in CI.

## Scope

Add two integration tests in `tests/integration.rs`:

### Test 1: `remember_recall_roundtrip_in_scripted_run`

Use a `MockProvider` script to simulate an agent that:
1. Calls `remember` with key `"project-language"` and value `"Rust"`.
2. Calls `recall` with key `"project-language"`.
3. Returns a final answer that includes the recalled value.

Assert:
- The run finishes with `FinishReason::NoMoreToolCalls` (or `Stop`).
- The transcript contains exactly one `Role::Tool` result for `remember`
  that indicates success.
- The transcript contains exactly one `Role::Tool` result for `recall`
  that contains the string `"Rust"`.

Use a `TempDir` as the workspace so memory files are isolated per test.
Wire `Remember::new(workspace)` and `Recall::new(workspace)` into the tool
registry.

```rust
#[tokio::test]
async fn remember_recall_roundtrip_in_scripted_run() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path();

    let script = vec![
        // Step 1: agent calls remember
        Completion { tool_calls: vec![ToolCall {
            id: "r1".into(), name: "remember".into(),
            arguments: json!({"key": "project-language", "value": "Rust"}),
        }], ..default_completion() },
        // Step 2: agent calls recall
        Completion { tool_calls: vec![ToolCall {
            id: "r2".into(), name: "recall".into(),
            arguments: json!({"key": "project-language"}),
        }], ..default_completion() },
        // Step 3: agent finishes
        Completion { content: "The project uses Rust.".into(), ..stop_completion() },
    ];

    let tools = ToolRegistry::new(LocalTransport)
        .register(Arc::new(Remember::new(ws)))
        .register(Arc::new(Recall::new(ws)));

    let mut runtime = AgentRuntime::builder()
        .llm(MockProvider::new(script))
        .tools(tools)
        .build().unwrap();

    let outcome = runtime.run("remember and recall the project language").await.unwrap();

    // recall tool result must contain "Rust"
    let recall_result = runtime.transcript().iter()
        .filter(|m| m.role == Role::Tool)
        .nth(1).unwrap();
    assert!(recall_result.content.contains("Rust"));
}
```

### Test 2: `load_skill_then_act_in_scripted_run`

Simulate an agent that:
1. Is given a system prompt with a `skill_index()` listing one skill:
   `"test-task"` — "How to run Rust tests".
2. Calls `load_skill` with `name: "test-task"`.
3. Uses the returned skill content to produce a final answer containing
   the keyword `"cargo test"`.

Assert:
- `load_skill` result contains the skill body.
- The final assistant text mentions `"cargo test"` (meaning the agent
  integrated the skill into its reasoning).

Create the skill as a `Skill` struct in the test setup, passing it to
`LoadSkill::new(skills)`.

```rust
#[tokio::test]
async fn load_skill_then_act_in_scripted_run() {
    let skills = vec![Skill {
        name: "test-task".into(),
        description: "How to run Rust tests".into(),
        body: "Run `cargo test --workspace` to execute all tests.".into(),
        // ... other fields as required
    }];

    let idx = skill_index(&skills);
    let script = vec![
        Completion { tool_calls: vec![ToolCall {
            id: "ls1".into(), name: "load_skill".into(),
            arguments: json!({"name": "test-task"}),
        }], ..default_completion() },
        Completion {
            content: "To run tests use cargo test".into(),
            ..stop_completion()
        },
    ];

    let tools = ToolRegistry::new(LocalTransport)
        .register(Arc::new(LoadSkill::new(skills.clone())));

    let system_prompt = format!("You are a test agent.\n{idx}");
    let mut runtime = AgentRuntime::builder()
        .llm(MockProvider::new(script))
        .tools(tools)
        .system_prompt(system_prompt)
        .build().unwrap();

    let outcome = runtime.run("how do I run tests?").await.unwrap();
    
    let load_result = runtime.transcript().iter()
        .filter(|m| m.role == Role::Tool)
        .next().unwrap();
    assert!(load_result.content.contains("cargo test"));
}
```

## Tests

The tests themselves ARE the deliverable. No production code changes are
needed — these are pure integration tests against existing tool implementations.

## Acceptance criteria

- `cargo test --workspace` green including both new tests
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` no diff
- `tests/integration.rs` contains both:
  - `remember_recall_roundtrip_in_scripted_run`
  - `load_skill_then_act_in_scripted_run`

## Notes for the agent

- Look at `tests/integration.rs` near line 420 (`load_skill_is_called_by_scripted_agent`)
  as a reference for how to set up a skill-loading scripted test.
- Look at the existing `Completion`/`ToolCall` helpers in the same file.
- `Remember::new(ws)`, `Recall::new(ws)`, `Forget::new(ws)` take a `&Path`
  (the workspace directory) as their argument.
- `LoadSkill::new(skills)` takes a `Vec<Skill>` — construct it from scratch
  with the fields: `name`, `description`, `body`, plus any fields `Skill`
  requires. Check `src/skills.rs` for the `Skill` struct definition.
- Use `Arc::new(LocalTransport)` for the `ToolRegistry` transport.
- These are unit-level scripted tests — no real LLM calls, no tempfile
  for skills (skills live in-memory as `Vec<Skill>`). Only `Remember`/`Recall`
  need a tempdir workspace for the filesystem persistence.
- Imports to add at the top of `tests/integration.rs`:
  `use recursive::tools::{Remember, Recall, LoadSkill};`
  `use recursive::skills::{skill_index, Skill};`
  Check if they're already imported before adding.
