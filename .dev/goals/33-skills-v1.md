# Goal 33 — Skills v1 (file-based capability extension)

**Roadmap**: 3.3 — Skill System (promoted from medium to high
priority by user direction, 2026-05-25)

**Design principle check**:
- Implemented as: **new module** `src/skills.rs` (loader + index) +
  **new Tool** `tools/load_skill.rs` + **system prompt source**
  (skill index gets folded into the prompt at agent start). The agent
  loop is untouched.
- ❌ Does NOT branch inside `agent.rs`. Skills enter the agent the
  same way any other tool + prompt content does.

## Why

Claude Code, Codex, and Hermes all support markdown-based skill files
that inject domain knowledge on demand. This is THE primary mechanism
for extending an agent's capabilities without editing kernel source.

A user wanting "explain my Rust crate's traits" doesn't need a new
tool — they write a SKILL.md saying *"When asked about Rust trait
design, walk the codebase, read trait definitions, and explain..."*
and Recursive matches that on intent.

## Scope

Touches: new `src/skills.rs`, new `src/tools/load_skill.rs`,
`src/tools/mod.rs` (pub use), `src/main.rs` (tool registration),
`src/config.rs` (skill index injection during system prompt build).

1. New module `src/skills.rs`:
   - `pub struct Skill { pub name: String, pub description: String,
     pub path: PathBuf }`.
   - `pub fn discover_skills(search_paths: &[PathBuf]) -> Vec<Skill>`:
     walks each path; for each `<name>/SKILL.md` file found, parses
     optional YAML frontmatter (`name`, `description`) — if absent,
     falls back to: name = parent directory name, description = first
     non-empty line of body.
   - `pub fn skill_index(skills: &[Skill]) -> String`: renders a
     compact "available skills" block for the system prompt:
     ```
     Available skills (use `load_skill` to activate):
     - <name>: <description>
     ...
     ```
     Returns empty string if no skills found.

2. New tool `src/tools/load_skill.rs`:
   - Parameters: `name: string` (required).
   - Behavior: find the skill by name (case-insensitive) in the
     discovered skill list, read its `SKILL.md` body, return the body
     as a string for the agent to consume into its working context.
   - Configured with the discovered skill list at registration time
     (closure captures the `Vec<Skill>`).
   - Tests: discovery on a tmp dir with 2 skill files, load_skill by
     name returns body content.

3. In `src/main.rs`:
   - Search paths default: `[~/.recursive/skills/, <workspace>/.recursive/skills/]`.
     Configurable via `RECURSIVE_SKILL_PATHS=path1:path2`.
   - At agent startup, `discover_skills(...)` → `skill_index(...)`
     gets appended to the system prompt (before the user goal, after
     the built-in prompt).
   - Register `load_skill` tool with the discovered list.

4. Tests in `src/skills.rs`:
   - **Test A**: discover skills in a tmp dir with one skill having
     YAML frontmatter and one having only body. Both parse correctly.
   - **Test B**: `skill_index([])` returns "".
   - **Test C**: `load_skill` tool returns the body for a known
     skill, errors for an unknown name.

## Acceptance

- `cargo build` green.
- `cargo test` green (140 baseline + 3 new = 143+).
- `cargo clippy --all-targets -- -D warnings` green.
- `cargo fmt --all` clean.
- No skills installed by default → agent behavior unchanged.

## Notes for the agent

- The YAML frontmatter parsing is simple: `^---\n(.+?)\n---\n` regex
  capture, then naive `key: value` line parsing. No `serde_yaml`
  dependency required. If a more robust parser is needed later, a
  follow-up goal can swap.
- The skill index in the system prompt should be **compact** — one
  line per skill, no body content. Bodies load on-demand via the
  tool.
- For now, skills are **read-only** (load_skill returns content; no
  write_skill / delete_skill). Discovery is at agent startup only;
  hot-reload is a future goal.
- Use `apply_patch`. `.to_string()` over `.into()` in tests.
- This goal touches `main.rs` and `config.rs` — coordinate with
  goal-34 (Anthropic Provider) which doesn't, and with goal-31/32
  which only touch `agent.rs`/`llm/`. No expected conflicts.
