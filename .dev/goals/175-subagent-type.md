# Goal 175 — `sub_agent` typed agents: `explore` / `general_purpose`

**Roadmap**: Phase 18 — Advanced Agent Patterns  
**Design principle check**:
- Implemented as: Tool trait extension + SubAgent override; no agent loop branching.
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop.
- ✅ Additive: new optional `subagent_type` parameter; existing callers unaffected.

## Why

Currently `sub_agent` always runs sequentially (marked `External` side-effect).
When multiple sub-agents perform read-only exploration, they could safely run in
parallel — but the dispatch layer cannot distinguish them from mutating agents.

Reference: fake-cc's `AgentTool` uses a `subagent_type` parameter to select
built-in agent definitions (`explore`, `generalPurpose`, `plan`, etc.).
`explore` agents restrict themselves to read-only tools and can run concurrently.

## What this goal does

### 1. Tool trait — `is_readonly_for_args`

`src/tools/mod.rs`: Add one default method to `Tool`:

```rust
/// Like `is_readonly` but can inspect call-time arguments.
/// Override when the read-only-ness depends on parameters (e.g. `sub_agent`
/// with `subagent_type: "explore"` vs `"general_purpose"`).
/// Default delegates to `is_readonly()`.
fn is_readonly_for_args(&self, _arguments: &Value) -> bool {
    self.is_readonly()
}
```

### 2. ToolRegistry — `is_readonly_for_call`

`src/tools/mod.rs`: Add one method to `ToolRegistry`:

```rust
/// Like `is_readonly` but passes the call arguments to the tool so it can
/// make an argument-specific decision (e.g. SubAgent checking subagent_type).
pub fn is_readonly_for_call(&self, name: &str, args: &Value) -> bool {
    self.tools
        .get(name)
        .map(|t| t.is_readonly_for_args(args))
        .unwrap_or(false)
}
```

### 3. `SubAgent` — override `is_readonly_for_args` + `subagent_type` parameter

`src/tools/sub_agent.rs`:

#### a. Built-in agent definitions

Add a private enum and associated data at the top of the file:

```rust
/// Named sub-agent personality — aligns with fake-cc's `subagent_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentType {
    /// Read-only exploration agent. Uses only read-only tools.
    /// Declared ReadOnly so the dispatch layer can run it in parallel.
    Explore,
    /// General-purpose agent with access to the full tool registry.
    /// Declared External (default).
    GeneralPurpose,
}

impl AgentType {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "explore" => Some(Self::Explore),
            "general_purpose" => Some(Self::GeneralPurpose),
            _ => None,
        }
    }

    fn is_read_only(self) -> bool {
        matches!(self, Self::Explore)
    }

    fn system_prompt_hint(self) -> &'static str {
        match self {
            Self::Explore => "You are an exploration sub-agent. Use only read tools to gather information. Do NOT write or modify files. Be thorough and concise.",
            Self::GeneralPurpose => "You are a general-purpose sub-agent. Complete the given task using the available tools. Be concise.",
        }
    }

    /// Tool names available to this agent type.
    /// `None` means "use whatever was passed in the tools parameter or defaults".
    fn allowed_tool_names(self) -> Option<Vec<String>> {
        match self {
            Self::Explore => Some(vec![
                "read_file".to_string(),
                "list_dir".to_string(),
                "search_files".to_string(),
                "recall".to_string(),
                "web_fetch".to_string(),
                "sub_agent".to_string(),  // allow nesting (depth-limited)
            ]),
            Self::GeneralPurpose => None,
        }
    }
}
```

#### b. Override `is_readonly_for_args`

```rust
fn is_readonly_for_args(&self, arguments: &Value) -> bool {
    if let Some(t) = arguments.get("subagent_type").and_then(|v| v.as_str()) {
        if let Some(at) = AgentType::from_str(t) {
            return at.is_read_only();
        }
    }
    false
}
```

#### c. Update `spec()` — add `subagent_type` to schema

Add `subagent_type` to the parameters JSON schema:

```json
"subagent_type": {
    "type": "string",
    "enum": ["explore", "general_purpose"],
    "description": "Agent personality. 'explore': read-only tools, runs in parallel. 'general_purpose': full tool access (default)."
}
```

#### d. Update `execute()` — respect `subagent_type`

In `execute()`, parse `subagent_type` early and use it to:
1. Override the system prompt hint
2. Override the tool list if `AgentType::Explore`

### 4. `agent.rs` — use `is_readonly_for_call` in both `execute_tool_calls`

There are two `execute_tool_calls` implementations (one in `RunCore`, one in `Agent`).
Both have a line like:

```rust
if self.tools.is_readonly(&pending[i].name) {
```

Change this to:

```rust
if self.tools.is_readonly_for_call(&pending[i].name, &pending[i].args) {
```

### 5. Tests

`src/tools/sub_agent.rs` — add tests:

- `explore_agent_is_readonly_for_args`: `is_readonly_for_args(json!({"subagent_type":"explore"}))` returns `true`
- `general_purpose_agent_is_not_readonly`: `is_readonly_for_args(json!({"subagent_type":"general_purpose"}))` returns `false`
- `unknown_subagent_type_is_not_readonly`: invalid type returns `false`
- `no_subagent_type_is_not_readonly`: missing field returns `false`
- `explore_agent_restricts_tools`: explore agent only gets read-only tools
- `explore_agent_dispatch`: sub-agent with `subagent_type: "explore"` runs successfully

`src/tools/mod.rs` — regression check:
- Existing `is_readonly` still returns same values as before

## Files to change

| File | Change |
|------|--------|
| `src/tools/mod.rs` | Add `is_readonly_for_args` to Tool trait; add `is_readonly_for_call` to ToolRegistry |
| `src/tools/sub_agent.rs` | Add `AgentType` enum; override `is_readonly_for_args`; update `spec()` and `execute()` |
| `src/agent.rs` | Change both `execute_tool_calls` to use `is_readonly_for_call` |

## Out of scope

- `run_in_background` (async background execution) — separate goal
- `plan` and `verification` agent types — separate goal
- Per-agent `permissionMode` — separate goal
- Changing `TeamOrchestrator` specialist concurrency — orthogonal

## Acceptance

1. `cargo test --workspace` green (including 6 new sub_agent tests)
2. `cargo clippy --all-targets --all-features -- -D warnings` clean
3. `cargo fmt --all` clean
4. `explore` type sub-agents report `is_readonly_for_args == true`
5. `explore` sub-agents restrict tool list to read-only tools
6. No regression on existing `sub_agent` calls without `subagent_type`
