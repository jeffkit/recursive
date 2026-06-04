# Goal 100 — Tool Rename + Glob Tool + Eager/Deferred Partition

## Why

Align Recursive's built-in tool names with fake-cc (Claude Code) conventions so
that prompts, skills, and documentation are consistent across projects. At the
same time, split the current `search_files` (content-only) into two tools that
mirror fake-cc's `Grep` (content search) + `Glob` (filename pattern match), and
introduce the eager/deferred partition so the `AnthropicProvider`'s
`ToolSearchTool` mechanism actually fires.

## Rename map

| Old name       | New name    | File                          |
|----------------|-------------|-------------------------------|
| `read_file`    | `Read`      | `src/tools/fs.rs`             |
| `write_file`   | `Write`     | `src/tools/fs.rs`             |
| `str_replace`  | `Edit`      | `src/tools/str_replace.rs`    |
| `run_shell`    | `Bash`      | `src/tools/shell.rs`          |
| `search_files` | `Grep`      | `src/tools/search.rs`         |
| `web_fetch`    | `WebFetch`  | `src/tools/web_fetch.rs`      |
| `todo_write`   | `TodoWrite` | `src/tools/todo.rs`           |
| `sub_agent`    | `Agent`     | `src/tools/sub_agent.rs`      |
| `load_skill`   | `Skill`     | `src/tools/load_skill.rs`     |

Also rename the Rust structs to match (e.g. `ReadFile` → `ReadFile` can stay,
but the `spec().name` string must change). Rename error messages and test
assertions that hardcode the old name strings.

## Remove `list_dir`

`ListDir` is redundant now that `Bash` can run `ls`/`find`. Steps:
- Delete `src/tools/fs.rs` `ListDir` struct and its `impl Tool` block.
- Remove `pub use fs::ListDir` from `src/tools/mod.rs`.
- Remove `register(Arc::new(ListDir::new(workspace)))` from
  `build_standard_tools`, `src/cli/builder.rs`, and `src/tools/sub_agent.rs`.
- Remove all string references `"list_dir"` (system prompt, TUI completion,
  skill_commands, permissions, render.rs verb_for_tool, etc.).
- Update tests that reference `list_dir`.

## Add `Glob` tool

New file `src/tools/glob.rs`. The tool:
- Name: `"Glob"`
- Input: `{ "pattern": string, "path"?: string }`  
  `pattern` is a glob pattern (e.g. `"**/*.rs"`); `path` is an optional
  workspace-relative subdirectory to scope the search.
- Behaviour: walk the scoped directory with `walkdir`, apply the pattern using
  the `glob` crate (already in Cargo.toml? check; if not, use `walkdir` +
  manual `fnmatch`-style matching). Return matching paths relative to workspace,
  one per line. Cap at 200 results.
- `side_effect_class`: `ReadOnly`
- Register in `src/tools/mod.rs` and `build_standard_tools`.
- Unit tests: match by extension, scope by path, no matches, cap enforced.

Check if the `glob` crate is already a dependency; if not, use the `wildmatch`
or implement simple glob matching (only `*`, `**`, `?` need work) to avoid
adding a dependency.

## Eager / deferred partition

### 1. Add `is_deferred()` to the `Tool` trait

In `src/tools/mod.rs`, add to the `Tool` trait:

```rust
/// Return `true` to send this tool as deferred (name-only) in the initial
/// prompt; the model must call `ToolSearch` to load its full schema.
/// Default is `false` (eager). Override in low-frequency tools.
fn is_deferred(&self) -> bool {
    false
}
```

### 2. Mark MCP tools as always-deferred

In `src/mcp.rs`, `McpTool`'s `impl Tool` block:

```rust
fn is_deferred(&self) -> bool {
    true
}
```

### 3. Mark all non-core built-in tools as deferred

Override `is_deferred() -> bool { true }` in:

- `src/tools/web_fetch.rs` (`WebFetch`)
- `src/tools/todo.rs` (`TodoWriteTool`)
- `src/tools/sub_agent.rs` (`SubAgent`)
- `src/tools/load_skill.rs` (`LoadSkill`, `RunSkillScript`)
- `src/tools/run_background.rs` (`RunBackground`, `CheckBackground`)
- `src/tools/memory.rs` (all memory tools: `Remember`, `Recall`, `Forget`,
  `WorkingMemoryTool`, `ScratchpadGet`, `ScratchpadDelete`, `ScratchpadList`)
- `src/tools/facts.rs` (`RememberFact`, `RecallFact`, `ForgetFact`, `UpdateFact`)
- `src/tools/episodic_recall.rs` (`EpisodicRecall`)
- `src/tools/estimate_tokens.rs` (`EstimateTokens`)
- `src/tools/plan_mode.rs` (`EnterPlanModeTool`, `ExitPlanModeTool`,
  `RequestPlanModeTool`)
- `src/tools/schedule_wakeup.rs` (`ScheduleWakeup`)
- `src/tools/send_message.rs` (`SendMessageTool`)
- `src/tools/spawn_worker.rs` (`SpawnWorkerTool`)
- `src/tools/team_manage.rs` (all team tools)
- `src/tools/a2a.rs` (all A2A tools)
- `src/tools/todo.rs` (`TodoWriteTool`)
- `src/tools/checkpoint.rs` (all checkpoint tools)

Eager (no override needed, default `false`):
- `Read`, `Write`, `Edit`, `Bash`, `Grep`, `Glob`, `Agent`, `Skill`

`ToolSearch` is injected by `AnthropicProvider` itself (not in the registry),
so no `is_deferred` needed there.

### 4. Wire `complete_with_search` into the agent loop

In `src/run_core.rs`, `call_llm_with_retry` currently calls
`self.llm.complete(...)` and `self.llm.stream(...)` with a flat `&[ToolSpec]`.

Change it to:

1. Split the registry's tool list into `eager` and `deferred` using
   `is_deferred()`:
   ```rust
   let (eager, deferred): (Vec<_>, Vec<_>) = specs
       .iter()
       .cloned()
       .partition(|s| !registry.is_deferred_spec(s));
   ```
   Add a helper `ToolRegistry::split_eager_deferred() -> (Vec<SpecWithHint>, Vec<SpecWithHint>)`
   that returns `(ToolSpec, Option<search_hint>)` pairs. The `search_hint` comes
   from the tool's `spec().description` first sentence for now (can be refined
   later).

2. Call `self.llm.complete_with_search(&messages, &eager, &deferred)` instead
   of `self.llm.complete(&messages, &specs)`.

3. For the streaming path, `AnthropicProvider` does not yet support streaming
   with search. Keep the current `self.llm.stream(...)` call for the streaming
   path but pass all tools as eager (no regression).

The `run_core` changes must not touch `agent.rs::Agent::run` — all changes stay
in `run_core.rs` (Invariant #1).

## String references to update

Search for every occurrence of the old names as strings and update them.
Key locations beyond the tool files themselves:

- `src/config.rs` — system prompt text and test assertions
- `src/permissions/mod.rs` — `check_static`, `safety_content_for_tool`
- `src/tools/mod.rs` — `record_touched`, `safety_content_for_tool`,
  `auto_classify` match arms
- `src/tui/app/render.rs` — `verb_for_tool` match arm
- `src/tui/app/event_loop.rs` — `str_replace` / `write_file` branch names
- `src/tui/completion.rs` — offline tool catalog
- `src/tui/skill_commands.rs` — allowed_tools in test fixture
- `src/tools/plan_mode.rs` — allowed-tools description strings
- `src/tools/sub_agent.rs` — default_tool_names, test assertions
- `src/cli/builder.rs` — imports and register calls
- `.dev/AGENTS.md` — if it mentions old tool names

## Acceptance criteria

```bash
cargo test --workspace                                          # all green
cargo clippy --all-targets --all-features -- -D warnings       # zero warnings
cargo fmt --all                                                 # clean
```

Additionally:
- `grep -r '"read_file"\|"write_file"\|"run_shell"\|"str_replace"\|"search_files"\|"list_dir"\|"web_fetch"\|"todo_write"\|"sub_agent"\|"load_skill"' src/` returns **zero hits** (old names fully replaced).
- A new `Glob` tool is present and passes its unit tests.
- `McpTool::is_deferred()` returns `true`.
- `AnthropicProvider::complete_with_search` is called from `run_core.rs` (grep for `complete_with_search` in `run_core.rs`).

## Notes for the agent

- Read `src/tools/mod.rs` fully before starting — it is the central registry.
- Read `.dev/AGENTS.md` for project invariants before touching any file.
- The rename is mechanical but wide. Use `search_files` / `grep` to find every
  string occurrence before editing.
- For `Glob`: check `Cargo.toml` for existing glob/wildmatch dependency before
  adding one. `walkdir` is already present.
- Do NOT modify `src/agent.rs` or `src/agent/mod.rs` — changes go in
  `src/run_core.rs` only.
- Write a journal entry in `.dev/journal/` when done.
