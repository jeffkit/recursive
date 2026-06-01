# Goal 169 — Slash Command System: Skill-Backed Custom Commands

## Summary

Extend the slash command system so that Markdown skill files in
`.recursive/skills/` (project-level) and `~/.recursive/skills/` (user-level)
are automatically registered as `/name` slash commands in the TUI, HTTP API,
and SDK — without requiring any Rust code changes.

Modeled after **Claude Code's `.claude/commands/` + skills** system (see
`fake-cc/src/commands.ts`, `fake-cc/src/skills/loadSkillsDir.ts`).

---

## Motivation

Currently:
- **TUI built-in commands** (e.g. `/help`, `/clear`, `/compact`) are
  hard-coded in `src/tui/commands.rs`.
- **Skills** exist (`~/.recursive/skills/*.md`) and can be loaded via
  the `load_skill` tool, but only when the agent explicitly calls the tool.
- **There is no user-defined slash command system** — users cannot type
  `/my-review` or `/standup` in the TUI to trigger a pre-written prompt.

Goal 169 bridges this gap: every `*.md` skill file becomes a `/command`.

---

## Design

### 1. Skill Markdown Format (compatible with existing skills)

A skill file at `.recursive/skills/refactor.md`:

```markdown
---
name: refactor
description: Refactor the selected code for clarity
aliases: [rf]
argument_hint: "<file-or-description>"
allowed_tools: [read_file, apply_patch, run_shell]
---

Refactor the following with these goals:
- Single responsibility
- No `unwrap()` in product code
- Clippy-clean

$ARGUMENTS
```

**Frontmatter fields** (all optional):

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | filename stem | Command name (no `/`) |
| `description` | string | first non-blank line of body | Shown in `/help` |
| `aliases` | string[] | `[]` | Alternative invocation names |
| `argument_hint` | string | `""` | Hint shown after command name |
| `allowed_tools` | string[] | all tools | Tools available during skill run |
| `model` | string | session default | Override LLM for this skill |

**Argument substitution**: `$ARGUMENTS` (or `{{args}}`) → replaced with
everything typed after the command name.

---

### 2. Skill Search Paths

Loaded in priority order (first match wins for name collisions):

1. `<workspace>/.recursive/skills/` — project-level (committed to repo)
2. `~/.recursive/skills/` — user-level (global)
3. Built-in commands (Rust `CommandRegistry::default_set()`)

MCP-sourced skills and bundled built-in skills come later (v2).

---

### 3. `SkillCommandLoader`

New struct in `src/tui/skill_commands.rs`:

```rust
pub struct SkillCommand {
    pub name: String,
    pub description: String,
    pub aliases: Vec<String>,
    pub argument_hint: String,
    pub allowed_tools: Option<Vec<String>>,
    pub prompt_template: String,  // body after frontmatter with $ARGUMENTS intact
    pub source_path: PathBuf,
}

impl SkillCommand {
    /// Expand the template with the provided argument string.
    pub fn expand(&self, args: &str) -> String {
        self.prompt_template
            .replace("$ARGUMENTS", args)
            .replace("{{args}}", args)
    }
}

pub struct SkillCommandLoader;

impl SkillCommandLoader {
    /// Load all skill files from the standard search paths.
    pub fn load(workspace: &Path) -> Vec<SkillCommand> { ... }
    
    /// Load from a single directory.
    pub fn load_dir(dir: &Path) -> Vec<SkillCommand> { ... }
}
```

---

### 4. Integration with `CommandRegistry`

`CommandRegistry` gains:

```rust
pub fn with_skill_commands(mut self, skills: Vec<SkillCommand>) -> Self {
    // For each SkillCommand, register an Async CommandSpec that pushes
    // UserAction::RunSkillPrompt { prompt: skill.expand(args) }
    ...
    self
}
```

The TUI `App::new()` calls:

```rust
let skills = SkillCommandLoader::load(&workspace);
let commands = CommandRegistry::default_set()
    .with_skill_commands(skills);
```

Skill commands appear in:
- `/help` listing (marked with a `📄` icon or `[skill]` suffix)
- Autocomplete popup (same prefix-search as built-in commands)

---

### 5. HTTP API

`GET /slash-commands` → list all registered slash commands (built-in + skills):

```json
[
  { "name": "clear", "description": "Clear conversation", "source": "builtin" },
  { "name": "refactor", "description": "Refactor code", "source": "skill",
    "aliases": ["rf"], "argument_hint": "<file>" }
]
```

`POST /sessions/:id/run` already accepts plain text prompts. Skills expand to
plain text, so no new endpoint needed — the HTTP client just calls
`POST /sessions/:id/run` with the expanded prompt.

The `GET /sessions/:id` SSE system message includes `slash_commands` (list of
available names), mirroring Claude Code SDK's `system/init` message.

---

### 6. SDK (Python)

```python
# List available slash commands
cmds = client.list_slash_commands()  # returns list of dicts

# Run a skill by name
session.send("/refactor src/lib.rs")  # expands and sends the prompt
```

---

### 7. Auto-reload

When a skill file is created/modified on disk, the next `/` prefix search
re-scans the skill directories (debounced, lazy). No need for file watchers
in v1; re-scan on `/` keypress is sufficient.

---

## Files to touch

| File | Change |
|------|--------|
| `src/tui/skill_commands.rs` | **New** — `SkillCommand`, `SkillCommandLoader` |
| `src/tui/commands.rs` | Add `CommandRegistry::with_skill_commands()` |
| `src/tui/app.rs` | Load skills in `App::new()` or on first `/` keypress |
| `src/tui/events.rs` | Add `UserAction::RunSkillPrompt { prompt }` |
| `src/tui/backend.rs` | Handle `RunSkillPrompt` → `runtime.run(prompt)` |
| `src/http.rs` | Add `GET /slash-commands`; include `slash_commands` in SSE init |
| `sdk/python/recursive_client/client.py` | `list_slash_commands()` |
| `tests/skill_commands.rs` | Unit tests for loader + expansion |

---

## Out of scope (defer)

- MCP-sourced slash commands
- `allowed_tools` enforcement during skill execution (honor in v2)
- `model` override per skill
- Namespace isolation per plugin (`/plugin:command`)
- Watching skill dirs for hot-reload

---

## Acceptance criteria

- [ ] `.recursive/skills/foo.md` auto-registers `/foo` in TUI
- [ ] `~/.recursive/skills/bar.md` auto-registers `/bar` globally
- [ ] Autocomplete shows skill commands alongside built-ins
- [ ] `/help` lists skill commands with descriptions
- [ ] `$ARGUMENTS` is substituted correctly
- [ ] `GET /slash-commands` returns all commands (built-in + skills)
- [ ] Python SDK `list_slash_commands()` works
- [ ] At least 8 unit tests (loader, expansion, alias resolution)
- [ ] `cargo test`, `clippy`, `fmt` all pass

---

## Effort

**M** — ~2 days. The loader is straightforward filesystem + frontmatter parsing.
Main work is wiring into TUI autocomplete and HTTP endpoint.
