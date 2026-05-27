# Goal 110 — Memory Layer 0: User & Project context injection

**Roadmap**: Phase 14.5 — Memory System (part 1/4)

**Design principle check**:
- Implemented as: extended system prompt loading in `src/config.rs`
- ❌ Does NOT touch `agent.rs::Agent::run`'s main loop
- Purely additive to existing `load_project_context` (Goal 36)

## Why

Currently only `AGENTS.md` at workspace root is auto-loaded. We need:
1. **User-global memory** (`~/.recursive/memory/user.md`) — personal prefs
   that apply across all projects (like Claude Code's `~/.claude/CLAUDE.md`)
2. **Project memory** (`<workspace>/.recursive/memory/project.md`) — agent-
   writable project context (like `.claude/CLAUDE.local.md`)

Together with existing AGENTS.md, this creates a 3-layer context hierarchy:
```
user.md (global) → AGENTS.md (team/repo) → project.md (local/agent-writable)
```

## Scope (do exactly this, no more)

### 1. New function: `load_memory_context`

In `src/config.rs`:

```rust
/// Memory context sources, loaded in priority order.
/// Each is capped at 8KB individually.
pub struct MemoryContext {
    pub user: Option<String>,      // ~/.recursive/memory/user.md
    pub project: Option<String>,   // AGENTS.md (existing)
    pub local: Option<String>,     // <workspace>/.recursive/memory/project.md
}

const MEMORY_FILE_MAX_BYTES: usize = 8 * 1024;

pub fn load_memory_context(workspace: &Path) -> MemoryContext;
```

Discovery:
- `user`: `~/.recursive/memory/user.md` (or `$RECURSIVE_HOME/memory/user.md`)
- `project`: `workspace.join("AGENTS.md")` — already handled by
  `load_project_context`, reuse it
- `local`: `workspace.join(".recursive/memory/project.md")`

### 2. System prompt assembly

Update `main.rs` where system prompt is built:

```
# User preferences
<user.md content>

# Project context (AGENTS.md)
<AGENTS.md content>

# Project memory
<project.md content>

---
<default system prompt>
```

Only include sections that have content. Existing `load_project_context`
behavior is preserved — this wraps around it.

### 3. Memory file templates

When `recursive init` is run (or first session in a workspace), offer
to create template files:

`~/.recursive/memory/user.md`:
```markdown
# User Preferences

<!-- Recursive reads this file at the start of every session. -->
<!-- Add your global preferences, coding style, conventions here. -->
```

`<workspace>/.recursive/memory/project.md`:
```markdown
# Project Memory

<!-- This file is read/written by Recursive agents. -->
<!-- It stores learned facts and context for this project. -->
```

### 4. Memory write tool (agent-writable project.md)

Add a new tool `update_project_memory` that allows the agent to append
to or rewrite sections of `project.md`:

```rust
pub struct UpdateProjectMemoryTool {
    workspace: PathBuf,
}

// Tool parameters:
// - action: "append" | "replace_section"
// - section: section heading (for replace_section)
// - content: text to append or replace with
```

This is how the agent "learns" — it writes observations to project.md
that persist across sessions.

### 5. Tests

- **Test A**: `load_memory_context` with all three files present
- **Test B**: `load_memory_context` with only AGENTS.md (backward compat)
- **Test C**: `load_memory_context` with oversized file truncates at 8KB
- **Test D**: `load_memory_context` with no files returns all None
- **Test E**: System prompt assembly includes all layers in correct order
- **Test F**: `update_project_memory` appends correctly
- **Test G**: `update_project_memory` replaces section correctly

## Acceptance

- `cargo build` green.
- `cargo test` green (7+ new tests).
- `cargo clippy --all-targets -- -D warnings` green.
- A workspace without any memory files behaves identically to before.
- `AGENTS.md` loading remains unchanged (cap stays at 16KB for it).

## Notes for the agent

- The `user.md` and `project.md` caps are 8KB each (smaller than AGENTS.md's
  16KB cap, because these are supplementary).
- Do NOT make `update_project_memory` available by default. Register it
  only when `--session` is active (since it's a persistence-related tool).
- The tool should create `.recursive/memory/project.md` if it doesn't
  exist on first write.
- Keep `load_project_context` working as-is for backward compatibility.
  The new `load_memory_context` calls it internally.
