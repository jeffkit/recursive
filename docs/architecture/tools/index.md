---
type: Index
title: Tools Overview
description: All tools registered in build_standard_tools(), grouped by category. Each tool is sandboxed to the workspace via resolve_within.
tags: [tools, architecture]
timestamp: 2026-06-18T10:00:00Z
---

# Tools Overview

All tools go through `ToolRegistry::invoke`. File/path tools are
sandbox-enforced via `resolve_within` (Invariant #3 — see
[Invariants](../invariants.md)).

## Filesystem

* [Filesystem Tools](filesystem.md) — `Read`, `Write`, `Edit`

| Tool | Rust struct | Source |
|------|------------|--------|
| `Read` | `ReadFile` | `src/tools/fs.rs` |
| `Write` | `WriteFile` | `src/tools/fs.rs` |
| `Edit` | `EditTool` | `src/tools/edit.rs` |
| `count_lines` | `CountLines` | `src/tools/count_lines.rs` |

## Shell & Search

* [Shell Tool](shell.md) — `Bash`
* [Search Tools](search.md) — `Grep`, `Glob`

| Tool | Rust struct | Source |
|------|------------|--------|
| `Bash` | `RunShell` | `src/tools/shell.rs` |
| `Grep` | `SearchFiles` | `src/tools/search.rs` |
| `Glob` | `GlobTool` | `src/tools/glob.rs` |
| `run_background` | `RunBackground` | `src/tools/run_background.rs` |
| `check_background` | `CheckBackground` | `src/tools/run_background.rs` |

## Memory

* [Memory Tools](memory-tools.md) — `remember`, `recall`, `forget`, scratchpad
* [Facts Tools](facts-tools.md) — `remember_fact`, `recall_fact`, `forget_fact`, `update_fact`
* [Episodic Tool](episodic-tool.md) — `episodic_recall`

## Skills

* [Skills Tools](skills-tools.md) — `load_skill`, `find_skills`, `install_skill`

## Multi-Agent

* [Multi-Agent Tools](multi-agent.md) — `AgentTool`, `send_message`, teams
* [Task Tools](task-tools.md) — `task_create`, `task_get`, `task_list`, `task_update`, `task_stop`, `task_output`
* [A2A Tools](a2a-tools.md) — `a2a_call`, `a2a_card`, `a2a_task_check`

## Web (feature-gated)

* [Web Tools](web-tools.md) — `web_fetch`, `web_search`

## Utilities

| Tool | Rust struct | Source |
|------|------------|--------|
| `estimate_tokens` | `EstimateTokens` | `src/tools/estimate_tokens.rs` |
| `TodoWrite` | `TodoWriteTool` | `src/tools/todo.rs` |
| `tool_search` | `ToolSearchTool` | `src/tools/tool_search.rs` |

## Adding a New Tool

1. Create `src/tools/<name>.rs`, implement the `Tool` trait.
2. Export from `src/tools/mod.rs`.
3. Register with `.register(Arc::new(MyTool::new(workspace)))` in `build_standard_tools`.
4. Add unit tests in the same file.
5. No changes to `AgentRuntime::run` (Invariant #1).

## Related Concepts

- [Invariants](../invariants.md) — sandbox rule (Invariant #3)
- [Agent Loop](../agent-loop.md) — how tool calls are dispatched
- [Overview](../overview.md) — component map
