---
type: Architecture
title: Layer 0 — System Prompt Assembly
description: How the system prompt is assembled from multiple memory sources before each agent run. Injection order, size limits, and the role of each source.
tags: [layer0, system-prompt, injection, memory, config]
timestamp: 2026-06-30T12:15:00Z
---

# Layer 0 — System Prompt Assembly

`src/system_prompt.rs::assemble_system_prompt` is the single maintenance point
for the "common" system-prompt structure. Every agent-loop entry point — CLI
`run` / `do` (`cli/builder.rs::build_runtime`), CLI `loop`
(`main.rs::run_loop`), HTTP API (`src/http/handlers.rs`), and TUI
(`recursive-tui/src/runtime_builder.rs`) — calls it with a channel-prepared
base and gets back the full prompt. Channels only differ in how they source
the base (e.g. CLI `--append-system-prompt`, HTTP `append_system_prompt`
request field), which they fold into `base` before calling. MCP server mode
has no agent loop and uses no system prompt.

The six memory layers below are assembled inside `Config::from_env()` and
already live in `config.system_prompt` (the `base` passed to
`assemble_system_prompt`). The order is intentionally **stable → volatile**
to maximise prefix-cache hits.

## Injection Order

```
1. # User preferences       ← ~/.recursive/memory/user.md        (≤ 8 KB)
2. # Project memory         ← <workspace>/.recursive/memory/project.md (≤ 8 KB)
3. # Memory                 ← .recursive/memory.json notes (legacy)
4. # Scratchpad             ← working memory KV entries
5. # Facts                  ← facts.jsonl entries (global + workspace merged)
6. # Episodic recall        ← recent session summaries
   ─── config.system_prompt ends here (the "base") ───
7. # Project context        ← AGENTS.md + CLAUDE.md at workspace root (≤ 16 KB each)
8. # Available skills       ← skill_index() string
9. ## Coordinator workflow  ← coordinator_system_prompt(), only when sub-agent enabled
   + sub_agent usage note
```

`assemble_system_prompt(base, workspace, skills, sub_agent_enabled)` lays down
7–9 on top of `base`. Project context is prepended (via
`prepend_project_context`) so a user-supplied `--system-prompt` / HTTP
`system_prompt` still gets the project context in front of it. The
coordinator workflow + `sub_agent` note (9) appear only when
`config.subagent_enabled` is true, in lockstep with the `Agent` tool
registered by `multi::register_subagent_if_enabled` (also called by every
channel) — so the prompt never advertises `sub_agent` to a surface that
lacks the tool.

CLI `run` additionally appends goal-matched skill *bodies* after assembly
(the only channel with a `goal` at prompt-build time); this is a
channel-specific suffix, not part of the common assembly.

## Size Limits

| Source | Cap | Location |
|--------|-----|----------|
| `user.md` | 8 KB | `MAX_MEMORY_FILE_SIZE` in `src/config.rs` |
| `project.md` | 8 KB | `MAX_MEMORY_FILE_SIZE` |
| `AGENTS.md` | 16 KB | `MAX_PROJECT_CONTEXT_SIZE` |
| `CLAUDE.md` | 16 KB | `MAX_PROJECT_CONTEXT_SIZE` |
| Facts summary | token budget | `src/tools/facts.rs::facts_summary()` |

Files that exceed their cap are truncated with a `[…truncated]` marker.

## Key Functions

| Function | File | Purpose |
|----------|------|---------|
| `load_user_memory()` | `src/config.rs` | Read `~/.recursive/memory/user.md` |
| `load_project_memory()` | `src/config.rs` | Read workspace `project.md` |
| `load_project_context()` | `src/config.rs` | Read `AGENTS.md` + `CLAUDE.md`, merge under `## ` sub-headers |
| `prepend_project_context()` | `src/config.rs` | Prepend the project context block to a base prompt |
| `assemble_system_prompt()` | `src/system_prompt.rs` | Single common assembly entry point called by every channel |
| `register_subagent_if_enabled()` | `src/multi.rs` | Register the `Agent` tool when `config.subagent_enabled` (called by every channel) |
| `default_system_prompt()` | `src/config.rs` | Hardcoded working principles |
| `skill_index()` | `src/skills.rs` | One-line summary per available skill |
| `facts_summary()` | `src/tools/facts.rs` | Recent facts as bullet list |
| `episodic_recall_summary()` | `src/tools/episodic_recall.rs` | Last session summary |

## Adding a New Layer 0 Source

1. Write a `load_*()` function in `src/config.rs` that returns `Option<String>`.
2. If it is a base/memory source, append it inside `Config::from_env()`'s
   `layers` vec — it then reaches every channel automatically via
   `config.system_prompt`. If it is a post-base source (like project context
   or skill index), add it inside `assemble_system_prompt` in
   `src/system_prompt.rs` — again, every channel picks it up via the single
   call site.
3. Add a section header so the agent knows which section it's reading.

Do NOT add a new source by editing each channel separately — that recreates
the per-channel duplication this layout exists to avoid.

**Do not branch inside `AgentRuntime::run`** to add context — use the system
prompt builder (Invariant #1).

## Architecture Knowledge Bundle as Layer 0

This architecture bundle (`.dev/architecture/`) can itself be injected as
Layer 0 by referencing `index.md` in `project.md`:

```markdown
## Architecture Reference
See `.dev/architecture/index.md` for the full architecture knowledge bundle.
Load individual concept docs with `Read` on demand.
```

## Related Concepts

- [Memory Overview](memory/index.md) — full four-layer memory system
- [Layer 0 — Injected Context](memory/layer0-injected-context.md) — user.md and project.md details
- [Layer 1 — Scratchpad](memory/layer1-scratchpad.md) — working memory injected at position 4
- [Layer 2 — Facts](memory/layer2-facts.md) — facts injected at position 5
- [Layer 3 — Episodic](memory/layer3-episodic.md) — episodic recall at position 6
- [Skills System](skills.md) — skill_index() at position 9
- [Agent Loop](agent-loop.md) — how the assembled system prompt is used
