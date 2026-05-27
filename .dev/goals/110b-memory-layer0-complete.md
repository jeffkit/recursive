# Goal 110b — Memory Layer 0: Complete user.md + project.md loading

**Roadmap**: Phase 14.5 — Memory System (part 1b/4, follow-up to 110)

**Design principle check**:
- Implemented as: extended system prompt loading in `src/config.rs` + `src/main.rs`
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- Purely additive

## Why

Goal 110 was partially implemented: only the `memory_summary_limit`
config and the summary injection relocation were done. The core scope
(loading `user.md` and `project.md` as memory context layers) was not
implemented. This goal completes the remaining work.

## What Goal 110 already did

- Moved `memory_summary()` call from `main.rs::build_agent()` into
  `config.rs::Config::from_env()`
- Added `memory_summary_limit` config field
- Fixed a format string bug in `memory_summary()`

## Scope (do exactly this, no more)

### 1. Load `~/.recursive/memory/user.md`

In `src/config.rs`, add a new function:

```rust
/// Load user-global memory from ~/.recursive/memory/user.md.
/// Returns None if the file doesn't exist. Caps at 8KB.
pub fn load_user_memory() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let path = PathBuf::from(home).join(".recursive/memory/user.md");
    load_memory_file(&path)
}

/// Load a memory file with an 8KB cap.
fn load_memory_file(path: &Path) -> Option<String> {
    if !path.is_file() { return None; }
    let content = std::fs::read_to_string(path).ok()?;
    if content.is_empty() { return None; }
    if content.len() > 8192 {
        Some(format!("{}\n\n[…truncated at 8KB]", &content[..8192]))
    } else {
        Some(content)
    }
}
```

### 2. Load `<workspace>/.recursive/memory/project.md`

```rust
/// Load project-local memory (agent-writable).
pub fn load_project_memory(workspace: &Path) -> Option<String> {
    let path = workspace.join(".recursive/memory/project.md");
    load_memory_file(&path)
}
```

### 3. Assemble system prompt with layers

Update the system prompt assembly in `config.rs` (where memory_summary
is currently injected) to prepend context layers:

```
# User preferences
<user.md content>

# Project context (AGENTS.md)
<AGENTS.md content — already loaded by load_project_context>

# Project memory
<project.md content>

---
<default system prompt + memory summary>
```

Only include sections that have content.

### 4. Tests

- **Test A**: `load_user_memory` returns content from a test file
- **Test B**: `load_project_memory` returns content from workspace
- **Test C**: Files exceeding 8KB are truncated
- **Test D**: Missing files return None (no error)
- **Test E**: System prompt assembly includes all available layers

## Acceptance

- `cargo build` green.
- `cargo test` green (5+ new tests).
- `cargo clippy --all-targets -- -D warnings` green.
- A workspace without any memory files behaves identically to before.
- Existing AGENTS.md loading remains unchanged.

## Notes for the agent

- The existing `load_project_context` in config.rs handles AGENTS.md
  with a 16KB cap. Don't touch it. The new functions are separate.
- Use `std::env::var("HOME")` for home directory. If HOME is unset,
  return None (don't crash).
- The prompt assembly order matters for LLM prefix caching: put the
  most stable content (user.md) first, most volatile (memory_summary)
  last.
- Keep all new functions in `src/config.rs` alongside the existing
  `load_project_context`.
