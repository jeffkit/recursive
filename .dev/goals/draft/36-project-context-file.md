# Goal 36 — Project Context File auto-load

**Roadmap**: 1.2 — Project Context File (High / S)

**Design principle check**:
- Implemented as: **new system prompt source** — at agent startup,
  look for a project-context markdown file in the workspace root and
  prepend its content to the system prompt.
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop. This
  is purely a system-prompt augmentation at construction time.

## Why

Every project that gets seriously used with this agent ends up
needing the user to manually paste "here's the codebase layout, key
conventions, where the entry point is" into every fresh session.
Claude Code reads `CLAUDE.md`, Codex reads `AGENTS.md`, Cursor reads
`.cursor/rules`. We need the same.

For us, the convention is **`AGENTS.md` at workspace root** (matches
.dev/AGENTS.md naming convention we already use, and matches the
emerging cross-tool convention). Auto-loaded if present, ignored if
absent.

## Scope

Touches: `src/config.rs` (new helper) or `src/main.rs` (loading
logic), `src/lib.rs` (export).

### 1. New helper

Add a function in `src/config.rs` (or new `src/project_context.rs`,
your call):

```rust
pub fn load_project_context(workspace: &Path) -> Option<String>
```

- Looks for `workspace.join("AGENTS.md")`. If exists and ≤ 16 KB,
  read it and return as `Some(content)`.
- If file is larger than 16 KB, read first 16 KB and append
  `\n\n[…truncated, AGENTS.md is N KB; consider trimming for fresh
  agent sessions]`. (The cap exists to prevent a single huge
  AGENTS.md from blowing the context window before any real work
  starts.)
- Return `None` if file absent.

### 2. Wire into agent startup

In `src/main.rs`, when building the agent:

1. Call `load_project_context(&config.workspace)`.
2. If `Some(content)`, prepend a header + the content to the system
   prompt:
   ```
   # Project context (AGENTS.md)
   
   <content>
   
   ---
   
   <existing default_system_prompt>
   ```

(Alternatively, inject as a separate leading system message — pick
whichever is mechanically cleaner.)

### 3. Tests

- **Test A** in `src/config.rs`: `load_project_context` with a
  tmpdir containing a small AGENTS.md returns its content.
- **Test B**: same, but file is 20 KB → returns 16 KB + truncation
  marker.
- **Test C**: tmpdir with no AGENTS.md returns `None`.

## Acceptance

- `cargo build` green.
- `cargo test` green (3 new tests).
- `cargo clippy --all-targets -- -D warnings` green.
- `cargo fmt --all` clean.
- A repo without `AGENTS.md` behaves identically to before this goal.

## Notes for the agent

- Filename is **`AGENTS.md`** at workspace root (not CLAUDE.md /
  PROJECT.md / etc). This matches the cross-tool convention.
- This repo has `.dev/AGENTS.md` for the self-improve loop — that
  one is NOT loaded; only the root-level `AGENTS.md` is. The two are
  distinct documents.
- The 16 KB cap is empirical: large enough for a reasonably detailed
  project context, small enough that one bad doc doesn't kneecap
  every session.
- This is a small, isolated goal. Don't over-engineer. No YAML
  frontmatter parsing, no include directives, no globs. Just read
  one file.
- Coordinate with g35 (MCP Client) — both touch `main.rs`. Section
  edits should be in different parts (g35 adds MCP registration
  block; this goal adds system-prompt augmentation block).
- Use `apply_patch`. `.to_string()` over `.into()` in tests.
