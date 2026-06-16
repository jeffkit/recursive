# Goal 300 — Remove stale static tool list from default_system_prompt

**Roadmap**: Post-Phase (Documentation / system prompt accuracy)

**Design principle check**:
- Implemented as: editing `default_system_prompt()` in `src/config.rs`
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

`default_system_prompt()` in `src/config.rs` (line ~484) contains:

```
"Tools available: Read, Write, Edit, Bash, Grep, Glob.",
"Additional tools: estimate_tokens (estimate token count for text or file).",
```

This static list was written when the tool set was small. Today Recursive
has 30+ tools (web_fetch, web_search, glob, search, todo, plan_mode,
agent, team_create, schedule_wakeup, episodic_recall, etc.). The static
list misleads agents into thinking only 7 tools exist, causing them to
either:
1. Not use available tools they'd need (e.g., `WebFetch`, `WebSearch`)
2. Over-rely on `Bash` for tasks that have dedicated tools

The LLM receives the actual tool specs via the API's `tools` field —
the static list in the system prompt is redundant and actively harmful
when it's wrong.

## Scope (do exactly this, no more)

### 1. `src/config.rs` — update `default_system_prompt()`

Remove the two lines:
```rust
"Tools available: Read, Write, Edit, Bash, Grep, Glob.",
"Additional tools: estimate_tokens (estimate token count for text or file).",
```

Replace with a single accurate line:
```rust
"All tools registered for this session are provided via the API tool spec — use them freely.",
```

This keeps the section meaningful without enumerating a stale list.

### 2. Update the existing test in `src/config.rs`

There's a test `default_system_prompt_stable` (around line 625) that checks
the length of the prompt. After the edit, run it and update the length
assertion if it becomes a literal check that breaks.

Also check if any test looks for the string "Read, Write, Edit" and update it.

### 3. Verify no other hardcoded tool lists

Search `src/` for any other strings like `"Read, Write, Edit"` or
`"Bash, Grep, Glob"` that might be stale tool enumerations, and
if found, update them similarly.

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `grep "Tools available: Read" src/config.rs` returns no results
- `grep "Additional tools: estimate_tokens" src/config.rs` returns no results

## Notes for the agent

- Read `src/config.rs` starting at `pub fn default_system_prompt()` (around
  line 482) to see the full context.
- Also read the test section at the bottom of `src/config.rs` to see if any
  test will break.
- The replacement text must preserve the informational intent while being
  accurate: agents should know tools are available via the API spec.
- The `src/config.rs` test at line ~625 checks prompt length — if it uses
  a hardcoded bound, update it; if it uses `< 6144`, it should still pass.
- **DO NOT modify** `src/agent.rs`, `src/runtime.rs`, `src/kernel.rs`,
  `src/http/`, or any tool files.
