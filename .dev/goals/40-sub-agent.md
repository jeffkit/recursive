# Goal 40 — Sub-Agent Tool (Phase 3.1)

> **Roadmap**: feature 3.1, M size, High impact.
> **Design principle check**: orthogonal — adds a new `Tool` that
> internally spawns a fresh `Agent` instance with restricted tools.
> Pluggable — opt-in registration in `main`. Testable — uses
> `MockProvider` to exercise the loop without network. **Recursive
> safety** is the only non-trivial concern: we must prevent
> unbounded sub-agent depth (zero-cost recursion bomb).

## What

A `sub_agent` tool that lets the parent agent dispatch a focused
sub-task to a fresh agent loop with:

- **Its own goal**: passed as `prompt` arg.
- **Its own transcript**: starts empty (this is the whole point —
  separate context window).
- **A restricted tool subset**: by default, read-only tools
  (`read_file`, `list_dir`, `search_files`, `web_fetch` if
  registered). The parent can opt-in additional tools via the
  `tools` arg.
- **A depth limit**: env `RECURSIVE_SUBAGENT_MAX_DEPTH` (default 2).
- **A step budget**: parent passes `max_steps` (default 30,
  capped at parent's remaining budget).

Returns a string with the sub-agent's final assistant message,
prefixed with its `FinishReason`.

## Why

The single biggest token-cost driver is transcript size — every step
re-sends the full conversation. Sub-agents let the parent delegate
research/scan tasks (e.g., "summarize what AGENTS.md says about
patch discipline") and only ingest the **result**, not the
intermediate exploration. This is the key SOTA primitive that makes
Cursor's research-mode and Claude's `Task` tool work.

## API sketch

```rust
// src/tools/sub_agent.rs
pub struct SubAgent {
    workspace: PathBuf,
    provider: Arc<dyn LlmProvider>,
    available_tools: Vec<Arc<dyn Tool>>,  // pool the sub can draw from
    max_depth: usize,
    current_depth: usize,
}
```

The current-depth counter must travel via the input args so each
nested invocation knows where it is. Implementation: when
constructing the sub `Agent`, register a fresh `SubAgent` instance
with `current_depth + 1`. When `current_depth >= max_depth`, return
an error string instead of spawning.

JSON args:
```json
{
  "prompt": "string (required)",
  "max_steps": "uint (optional, default 30)",
  "tools": "string[] (optional, names of tools to allow)"
}
```

## Tests

- `sub_agent_basic_dispatch` — MockProvider that returns one assistant
  message; assert sub-agent runs to NoMoreToolCalls and returns text.
- `sub_agent_depth_limit_enforced` — depth=0 sub-agent tries to call
  itself; should be denied at depth=2 default.
- `sub_agent_tool_subset_respected` — parent passes `tools: ["read_file"]`;
  sub-agent's registry must NOT include `apply_patch`.
- `sub_agent_max_steps_capped` — pass `max_steps: 5`; assert sub
  returns `BudgetExceeded` after 5 steps when MockProvider keeps
  asking for more tool calls.

## Wiring

- `src/tools/mod.rs`: `pub mod sub_agent;` + `pub use sub_agent::SubAgent;`.
- `src/main.rs::build_tools`: conditionally register `SubAgent`
  behind env `RECURSIVE_SUBAGENT_ENABLED` (default off for v1) so
  this doesn't change baseline behavior.
- `src/config.rs::default_system_prompt`: ONE line — when sub-agent
  is enabled, say "use sub_agent for focused research/scans".

## Acceptance

- `cargo build` green.
- `cargo test` green; +4 new tests minimum.
- `cargo clippy --all-targets -- -D warnings` clean.
- `cargo fmt --all -- --check` clean.
- Existing behavior preserved when env flag is off.

## Out of scope (defer to follow-up)

- Streaming sub-agent partial results back through to the parent.
- Sub-agent using a different provider/model than the parent.
- Caching identical sub-agent prompts.
