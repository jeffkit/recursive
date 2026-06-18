# Goal 318 — File-glob Rules System (context injection by file path)

## Why

Cursor and Claude Code both support **file-glob-scoped rules** (`.cursor/rules/*.mdc`
and `.claude/rules/` respectively): when a file matching a glob is opened or
edited, the corresponding rule/context is automatically injected into the
agent's context window.

Recursive's skill system only supports three injection modes today:
- `always` — injected at every session start
- `trigger` — injected when the user message contains a keyword
- `manual` — loaded on demand via `load_skill`

There is **no `globs` mode** that activates a skill (or any structured
context fragment) when a tool call touches a matching file path. This means
Recursive cannot automatically remind itself "update `docs/architecture/` when
you change `src/tools/`" — a key requirement for keeping the new
`docs/architecture/` OKF bundle in sync with code changes.

## Goal

Add a `globs` injection mode to Recursive's skill system. When a tool call
produces a result referencing a path that matches one of the skill's globs,
the skill is injected into the next turn's context (as a system-message
addendum or a tool-result annotation).

## Design

### 1. Skill frontmatter extension

`SKILL.md` files gain an optional `globs` field alongside the existing `mode`:

```yaml
---
type: Skill
name: architecture-sync-reminder
description: "Remind agent to update docs/architecture/ when editing core source files"
mode: globs
globs:
  - "src/tools/**"
  - "src/llm/**"
  - "src/runtime.rs"
  - "src/config.rs"
---
Whenever you modify files matching the patterns above, update the
corresponding document in `docs/architecture/`:

| Changed file area   | Doc to update                            |
|---------------------|------------------------------------------|
| `src/tools/**`      | `docs/architecture/tools/*.md`           |
| `src/llm/**`        | `docs/architecture/providers/*.md`       |
| `src/runtime.rs`    | `docs/architecture/agent-loop.md`        |
| `src/config.rs`     | `docs/architecture/overview.md`          |

After updating code, run a quick check:
```
ls docs/architecture/
```
and verify the relevant doc's "Last updated" section is current.
```

### 2. `Skill` struct extension

Add `globs: Option<Vec<String>>` to `src/skills.rs::Skill`:

```rust
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    pub mode: SkillMode,
    pub globs: Option<Vec<String>>,   // NEW: path patterns for globs mode
    pub triggers: Option<Vec<String>>,
}
```

### 3. `SkillMode` enum extension

```rust
pub enum SkillMode {
    Always,
    Trigger,
    Manual,
    Globs,   // NEW
}
```

### 4. Glob matching in the agent kernel

In `src/agent.rs` (or a new `src/skills_injector.rs` to keep the loop small),
after each tool call completes:

1. Collect all file paths referenced in the tool result string (extract
   anything that looks like a relative path, e.g. matches `[^\s"'<>]+\.[a-zA-Z]+`
   or starts with `src/` / `docs/` etc.).
2. For each `Globs`-mode skill, check whether any extracted path matches
   any of the skill's `globs` patterns using standard glob matching
   (`glob` crate already in `Cargo.toml` if available, or implement a
   minimal prefix/suffix matcher with `**` support to avoid new deps —
   see Invariant #6).
3. If matched and the skill hasn't been injected in this turn yet, append
   the skill body as an additional system message (or a special
   `Role::System` message before the next user turn).

**Invariant #1 constraint**: the matching logic MUST NOT be a branch inside
`agent.rs::Agent::run`. Place it in a helper function or a new
`SkillInjector` struct that `run` calls as a single opaque step.

### 5. Skill file for architecture sync

Create `.recursive/skills/arch-sync/SKILL.md` with the content shown in
section 1 above. This is the first consumer of the new `globs` mode and
serves as an acceptance-test fixture.

## Acceptance criteria

1. `cargo test --workspace` green, including:
   - Unit test: `globs_skill_matches_path` — a skill with
     `globs: ["src/tools/**"]` matches `"src/tools/fs.rs"` and does NOT
     match `"src/agent.rs"`.
   - Unit test: `globs_skill_no_match_returns_empty` — verify no injection
     when no tool result path matches.
   - Integration test: a scripted run where the agent calls `Write` on
     `"src/tools/new_tool.rs"` produces a subsequent `Role::System` message
     containing the arch-sync skill body.
2. `cargo clippy --all-targets --all-features -- -D warnings` clean.
3. `cargo fmt --all` no diff.
4. `.recursive/skills/arch-sync/SKILL.md` exists and is valid.
5. Parsing of `mode: globs` in SKILL.md frontmatter round-trips correctly
   (serialize → deserialize → same struct).

## Scope

Files to touch:
- `src/skills.rs` — `Skill` struct + `SkillMode` enum + `parse_skill` frontmatter parser
- `src/agent.rs` — call new injector after each tool result (one line, Invariant #1 safe)
- `src/skills_injector.rs` (NEW) — `SkillInjector::check_and_inject(tool_result, skills)` logic
- `src/tools/mod.rs` — re-export if needed
- `tests/integration.rs` — new integration test for globs injection
- `.recursive/skills/arch-sync/SKILL.md` (NEW) — first consumer skill

Do NOT touch `Cargo.toml` for a new dependency. Implement glob matching
with a minimal inline helper (e.g. split on `**`, match prefix/suffix).

## Notes for the agent

- The `glob` path-matching helper must handle at minimum:
  - Exact match: `"src/runtime.rs"` matches `"src/runtime.rs"` only
  - `**` wildcard: `"src/tools/**"` matches `"src/tools/fs.rs"` and
    `"src/tools/sub/dir/file.rs"` but NOT `"src/agent.rs"`
  - No need for `?` or `[...]` character-class support in this goal.
- Injection timing: inject BEFORE the LLM sees the next turn, i.e., push
  a `Role::System` message at the end of the tool-result batch, before
  constructing the next API call.
- If the same skill would be injected twice in one session (e.g. agent edits
  two matching files), inject only once (track injected skill names in a
  `HashSet<String>` in `AgentKernel` or `SkillInjector`).
- The `SkillInjector` struct is stateless except for the
  `already_injected: HashSet<String>` set; initialize it fresh per
  `AgentRuntime::run` invocation.
