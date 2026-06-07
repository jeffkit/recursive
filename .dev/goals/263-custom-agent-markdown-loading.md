# Goal 263 — Custom agent markdown loading (Phase C)

**Roadmap**: Code quality — architecture review follow-up (P2 backlog,
Phase C of multi-agent unification)

**Design principle check**:
- Implemented as: a new `load_agents_dir.rs` module that reads
  `~/.claude/agents/*.md` and `.claude/agents/*.md` on demand, plus
  a unified `AgentDefinitions` registry that the unified `AgentTool`
  (from goal 262) reads from
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- ❌ Does NOT touch `src/tools/a2a.rs` (external agent protocol stays
  as-is)

## Why

The unification in goal 262 (the unified `Agent` tool with
`subagent_type` parameter) sets up a clean dispatch: any string in
`subagent_type` is looked up in an `AgentDefinitions` registry, and
the resolved definition provides the system prompt, allowed tools,
permission mode, and other per-agent settings. The two hard-coded
agent types (`explore`, `general_purpose`) cover the current use
cases, but a real product lets users define their own agents —
"a Rust reviewer that knows our crate layout", "a docs writer that
follows our style guide", etc. — without recompiling.

fake-cc solves this with a markdown-frontmatter loader:
`loadAgentsDir.ts::getAgentDefinitionsWithOverrides()` (lines
296-393). It reads `*.md` files from `~/.claude/agents/` (user
settings) and `<project>/.claude/agents/` (project settings), parses
the YAML frontmatter, validates the shape against a Zod schema, and
returns a `Vec<AgentDefinition>`. Three sources are unioned:
`BuiltInAgentDefinition` (code-defined in `built-in/*.ts`),
`CustomAgentDefinition` (markdown), and `PluginAgentDefinition`
(markdown from a plugin's `agents/` dir). The LLM-facing
`subagent_type` accepts any name from the union; the LLM doesn't
know whether the agent is built-in or user-defined.

The frontmatter schema (from `loadAgentsDir.ts:73-99`) is the
authoritative shape — we mirror it:

```yaml
---
name: rust-reviewer              # agentType (required, unique)
description: |                   # whenToUse (required, one-liner)
  Reviews Rust diffs against our crate layout and error-handling
  conventions. Strict on `unwrap()`, lenient on `?`-style propagation.
tools:                           # allowed tools (None = inherit all)
  - Read
  - Grep
  - Glob
disallowedTools: []              # removed from default set (None = none)
model: inherit                   # "inherit" = parent's model; else string
permissionMode: default          # PermissionMode variant
maxTurns: 20                     # positive int; None = parent's limit
memory: project                  # "user" | "project" | "local" | None
isolation: worktree              # "worktree" | None
background: false                # if true, always runs as background
initialPrompt: ""                # prepended to first user turn
mcpServers:                      # per-agent MCP server set (None = inherit)
  - slack
hooks: {}                        # per-agent hook bindings
color: blue                      # UI color hint (None = auto)
---

You are a Rust reviewer. ...
```

A `BaseAgentDefinition` Rust type backs all three sources. The
markdown loader produces `CustomAgentDefinition` variants; the
hard-coded `explore` / `general_purpose` from goal 262 produce
`BuiltInAgentDefinition` variants. Both are storable in the same
`AgentDefinitions` registry and resolvable by the unified `AgentTool`.

The key technical detail: YAML frontmatter parsing. Recursive
already has a small `frontmatter` reader in
`src/permissions/frontmatter.rs` (used by the `Read` tool's
frontmatter rules), but it's tied to a specific use case. We can
either reuse it or pull in a tiny `serde_yaml` dependency. The
goal picks `serde_yaml` because the schema is YAML-shaped and the
existing helper does not handle multi-line `description:` blocks
well.

## Scope (do exactly this, no more)

### 1. New file `src/tools/agent_defs.rs` — the registry

Create a new module that owns the agent-definition registry. The
goal introduces two new types (`AgentDefinition` enum, plus a
`CustomAgentDefinition` variant) and one new function
(`load_user_agents` / `load_project_agents` / `all_agents`).

Suggested shape (mirrors fake-cc's `BaseAgentDefinition`):

```rust
//! Agent definition registry — unified view over built-in and
//! custom-markdown agents. Reference: fake-cc
//! `src/tools/AgentTool/loadAgentsDir.ts`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentFrontmatter {
    /// Required. The `subagent_type` string the LLM uses to invoke
    /// this agent. Must be unique across all sources (built-in +
    /// custom).
    pub name: String,
    /// Required. One-line description shown in tool-call lists.
    pub description: String,
    /// Optional. List of allowed tool names. `None` = inherit
    /// parent's tool set.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    /// Optional. List of removed tool names.
    #[serde(default)]
    pub disallowed_tools: Option<Vec<String>>,
    /// Optional. `Some("inherit")` or `Some("claude-sonnet-4-5")` or
    /// `None` for parent's model.
    #[serde(default)]
    pub model: Option<String>,
    /// Optional. PermissionMode variant name.
    #[serde(default)]
    pub permission_mode: Option<String>,
    /// Optional. Positive integer; default = parent's max_turns.
    #[serde(default)]
    pub max_turns: Option<u32>,
    /// Optional. `"user"` | `"project"` | `"local"`.
    #[serde(default)]
    pub memory: Option<String>,
    /// Optional. `"worktree"` to run in a fresh worktree.
    #[serde(default)]
    pub isolation: Option<String>,
    /// Optional. If true, always runs as a background task.
    #[serde(default)]
    pub background: Option<bool>,
    /// Optional. Prepended to the first user turn.
    #[serde(default)]
    pub initial_prompt: Option<String>,
    /// Optional. List of MCP server names (None = inherit).
    #[serde(default)]
    pub mcp_servers: Option<Vec<String>>,
    /// Optional. Per-agent hooks.
    #[serde(default)]
    pub hooks: Option<serde_yaml::Value>,
    /// Optional. UI color hint.
    #[serde(default)]
    pub color: Option<String>,
}

#[derive(Debug, Clone)]
pub enum AgentDefinition {
    /// Code-defined (e.g., `explore`, `general_purpose`).
    BuiltIn(BuiltInAgentDefinition),
    /// Loaded from `~/.claude/agents/*.md` or
    /// `<project>/.claude/agents/*.md`.
    Custom(CustomAgentDefinition),
    /// Loaded from a plugin's `agents/*.md` (Phase E / future).
    Plugin(PluginAgentDefinition),
}

#[derive(Debug, Clone)]
pub struct BuiltInAgentDefinition {
    pub agent_type: String,
    pub when_to_use: String,
    pub system_prompt: String,
    pub allowed_tools: Option<Vec<String>>,
    pub max_turns: Option<u32>,
    pub model: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CustomAgentDefinition {
    pub agent_type: String,
    pub when_to_use: String,
    pub system_prompt: String, // body of the .md after frontmatter
    pub frontmatter: AgentFrontmatter,
    pub source: AgentSource,
    pub file_path: PathBuf,
    pub base_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentSource {
    UserSettings,    // ~/.claude/agents/*.md
    ProjectSettings, // <project>/.claude/agents/*.md
    Plugin,          // plugin/agents/*.md
}

#[derive(Clone)]
pub struct AgentDefinitions {
    by_name: Arc<std::collections::HashMap<String, AgentDefinition>>,
}

impl AgentDefinitions {
    /// Construct a registry from built-in defs (hard-coded) and
    /// custom defs (loaded from disk). Built-in wins on name
    /// collision; later sources override earlier ones for
    /// custom-vs-plugin.
    pub fn load(
        built_ins: Vec<BuiltInAgentDefinition>,
        user_dir: Option<PathBuf>,
        project_dir: Option<PathBuf>,
    ) -> Result<Self, Error> { ... }

    /// Resolve `subagent_type` to a definition. Returns
    /// `Error::UnknownAgentType` if not found.
    pub fn get(&self, name: &str) -> Result<&AgentDefinition, Error> { ... }

    /// All known agent types, sorted alphabetically. Used to
    /// populate the LLM-facing description of the Agent tool.
    pub fn all_names(&self) -> Vec<String> { ... }
}
```

**Note on union order** (per fake-cc's `getActiveAgentsFromList`,
lines 193-221): the priority is
`built_in > plugin > user > project > flag > managed`. The same
agent name in a project-level markdown **does not** override a
built-in. This is intentional: built-in names are stable contract;
custom names are user experiments.

### 2. Markdown loader functions

Add three module-level functions (private to `agent_defs.rs`):

```rust
/// Read all `*.md` files from `~/.claude/agents/` (user settings).
/// Returns parsed `CustomAgentDefinition`s; files that fail to
/// parse are logged at `tracing::warn!` and skipped (do not fail
/// the whole load).
fn load_user_agents() -> Result<Vec<CustomAgentDefinition>, Error> {
    let dir = home_dir()?.join(".claude").join("agents");
    load_agents_from_dir(&dir, AgentSource::UserSettings)
}

/// Read all `*.md` files from `<project>/.claude/agents/`.
fn load_project_agents(cwd: &Path) -> Result<Vec<CustomAgentDefinition>, Error> {
    let dir = cwd.join(".claude").join("agents");
    load_agents_from_dir(&dir, AgentSource::ProjectSettings)
}

fn load_agents_from_dir(
    dir: &Path,
    source: AgentSource,
) -> Result<Vec<CustomAgentDefinition>, Error> {
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        match parse_agent_md(&path, source.clone()) {
            Ok(def) => out.push(def),
            Err(e) => tracing::warn!(
                "Failed to load agent from {}: {e}",
                path.display()
            ),
        }
    }
    Ok(out)
}

/// Parse one .md file: split frontmatter from body, deserialize
/// frontmatter via `serde_yaml`, validate required fields, return
/// a `CustomAgentDefinition`.
fn parse_agent_md(
    path: &Path,
    source: AgentSource,
) -> Result<CustomAgentDefinition, Error> { ... }
```

The body of `parse_agent_md` is straightforward: read the file,
find the first `---\n` ... `---\n` block, hand the frontmatter to
`serde_yaml::from_str::<AgentFrontmatter>`, and the body
(everything after the second `---\n`) becomes `system_prompt`.
Trim leading/trailing whitespace from `system_prompt`. Validate:
- `name` non-empty, ASCII, no whitespace (the LLM passes it as
  `subagent_type`).
- `description` non-empty.
- `isolation`, `memory`, `permission_mode`, `model` are
  validated against a small allow-list (see the constant
  `VALID_ISOLATION_MODES`, etc., in fake-cc's
  `parseAgentFromMarkdown`).
- If `background: true` and `max_turns: 0`, that's a contradiction;
  warn and use the parent default.

### 3. Wire `AgentDefinitions` into the unified `AgentTool`

The unified `AgentTool` from goal 262 holds an
`Arc<AgentDefinitions>` field. Its `call()` resolves
`input.subagent_type` via `agent_defs.get(name)?`. The
`AgentDefinitions::load` is called once at CLI startup, **after**
`multi::AgentPool` is constructed, and the result is shared by
`Arc` into the `AgentTool` constructor and into the
`PermissionPipeline` (which may need to read each agent's
`permission_mode` for the policy stage).

In `src/cli/builder.rs`:

```rust
let agent_defs = AgentDefinitions::load(
    built_in::built_in_definitions(),  // moves the 2 hard-coded ones
    user_agents_dir(),                 // ~/.claude/agents/
    project_agents_dir(cwd),
)?;
let agent_tool = AgentTool::new(pool.clone(), Arc::new(agent_defs));
```

The `built_in::built_in_definitions()` function in
`src/tools/agent_tool.rs` returns the 2 existing types
(`explore`, `general_purpose`) as `BuiltInAgentDefinition`s. This
is the only change to `agent_tool.rs` from goal 262; the registry
plug-in point is `Arc<AgentDefinitions>`.

### 4. Tests for `agent_defs`

Add `#[cfg(test)] mod tests` to `agent_defs.rs`. Required cases:

- `parse_agent_md_extracts_frontmatter_and_body` — write a sample
  .md to a temp dir, call `parse_agent_md`, assert `name`,
  `description`, and `system_prompt` come back correctly with
  multiline `description: |` blocks handled.
- `parse_agent_md_rejects_missing_name` — write a .md with no
  `name:` in frontmatter, expect `Err(Error::InvalidAgentMd)`
  with a message naming the missing field.
- `parse_agent_md_rejects_missing_description` — same, missing
  `description:`.
- `parse_agent_md_rejects_invalid_isolation` — `isolation: bogus`,
  expect `Err` with a message naming the invalid value and the
  valid options.
- `parse_agent_md_rejects_invalid_memory_scope` —
  `memory: invalid_scope`, expect `Err`.
- `parse_agent_md_accepts_all_known_fields` — write a .md with
  every frontmatter field populated, expect `Ok` and assert each
  field deserialized correctly.
- `load_user_agents_reads_files_from_home_dir` — set
  `HOME=/tmp/test_home` (or use `tempfile::TempDir` to override
  the home path), drop 2 valid .md files, call `load_user_agents`
  via the public API, expect 2 defs in the result.
- `load_project_agents_reads_files_from_cwd` — set `cwd` to a
  temp dir with `.claude/agents/foo.md`, expect 1 def.
- `load_skips_non_md_files` — put `foo.txt` and `bar.md` in the
  agents dir, expect only `bar.md` to load.
- `load_warns_and_skips_invalid_files` — 1 valid + 1 invalid .md
  in the agents dir, expect 1 def and a `tracing::warn!` line in
  the captured log output.
- `agent_definitions_get_resolves_built_in_over_custom` — built-in
  `"general_purpose"` and a custom `general_purpose.md` in the
  user dir, expect `get("general_purpose")` to return the
  built-in (built-in wins per union order).
- `agent_definitions_get_resolves_custom_when_no_built_in` —
  custom `rust-reviewer.md`, expect
  `get("rust-reviewer")` to return the custom def.
- `agent_definitions_get_returns_error_for_unknown` —
  `get("nope")`, expect `Err(Error::UnknownAgentType("nope"))`.
- `agent_definitions_all_names_returns_sorted_unique` — load
  3 custom defs + 2 built-ins, expect 5 unique names sorted
  alphabetically.
- `agent_definitions_load_handles_missing_dirs` — `HOME` points
  to a non-existent path, expect `Ok(empty registry)` (not an
  error). This is the "no custom agents configured" baseline.

### 5. Update existing tests

Goal 262's `agent_tool.rs::tests` currently hard-codes
`subagent_type = "explore"`. No change needed — `"explore"` is in
the built-in list. The new test cases just exercise the resolver
behavior in the registry layer, not the tool layer.

The 2 existing tool tests in `src/tools/sub_agent.rs::tests`
(inherited from goal 262) used `subagent_type = "explore"` and
`"general_purpose"`; both still resolve via the built-in list.

### 6. Dependencies

Add `serde_yaml = "0.9"` to `Cargo.toml` (or use a `serde` feature
on a different crate — pick the smallest). Verify the build
isn't slowed by more than 5% on the workspace's incremental build
time. If it is, consider a vendored minimal YAML reader for the
small subset of YAML we need (frontmatter is just a map of
scalars and lists — no anchors, no tags).

### 7. `.gitignore` and doc

Add `~/.claude/agents/*.local.md` to the user's global gitignore
recommendation in `README.md` so personal agents don't get
accidentally committed. (The project's
`.claude/agents/*.md` files are intentional and should be
committed when they are team-shared.)

Add a section to `docs/` describing the agent-markdown format.
The doc should:
- Show one minimal example (just `name` + `description` + body).
- Show one full example (every field populated).
- Explain the priority order (built-in > plugin > user > project).
- Explain that the file is parsed once at startup; edits to
  `~/.claude/agents/foo.md` require a restart of `recursive`.
  (Caching is intentionally out of scope for this goal; the
  follow-up goal adds `clearAgentDefinitionsCache()` mirroring
  fake-cc's `loadAgentsDir.ts:395`.)

### 8. Verify

```bash
cargo test --workspace
cargo test --lib tools::agent_defs
cargo test --lib tools::agent_tool
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all
```

All must be clean. The new module passes its 14 tests; the
existing 7+ tests in `agent_tool.rs` continue to pass without
change.

## Acceptance

- A new `src/tools/agent_defs.rs` exists with
  `AgentDefinition`, `BuiltInAgentDefinition`,
  `CustomAgentDefinition`, `AgentDefinitions` (with `load` / `get`
  / `all_names`), and the 3 `load_user_agents` /
  `load_project_agents` / `load_agents_from_dir` private helpers.
- The `AgentTool` from goal 262 holds `Arc<AgentDefinitions>` and
  resolves `subagent_type` via it.
- Custom agents can be loaded from `~/.claude/agents/*.md` and
  `<project>/.claude/agents/*.md`. Skipped (not error) when the
  dirs don't exist.
- Frontmatter validation rejects: missing `name`, missing
  `description`, invalid `isolation` value, invalid `memory`
  scope, invalid `permission_mode` variant. Each rejection logs
  a `tracing::warn!` with the file path.
- The union order is `built_in > plugin > user > project`; a
  custom `general_purpose.md` does **not** override the built-in
  `general_purpose`.
- 14+ unit tests cover parse + load + resolve + error paths.
- A doc section in `docs/` describes the agent-markdown format.
- All quality gates clean.

## Notes for the agent

- **Read first**:
  - `~/Downloads/fake-cc/src/tools/AgentTool/loadAgentsDir.ts` —
    the authoritative schema (lines 73-99 for the YAML schema,
    lines 541-755 for the markdown parser).
  - `~/Downloads/fake-cc/src/utils/markdownConfigLoader.ts` —
    the file-system reader (the `loadMarkdownFilesForSubdir`
    helper at line 308 of `loadAgentsDir.ts` shows how it walks
    the agents dir).
  - `~/Downloads/fake-cc/src/tools/AgentTool/built-in/exploreAgent.ts` —
    the shape of a `BuiltInAgentDefinition`.
  - `src/permissions/frontmatter.rs` — Recursive's existing
    frontmatter reader; check if it can be reused for the
    validation step, but expect to add a new
    `parse_agent_frontmatter` that handles the agent-specific
    schema.
  - `src/cli/builder.rs` — the construction site where
    `AgentDefinitions::load` will be called.
- **Don't reach for `serde_yaml::from_str::<AgentFrontmatter>` on
  the full file**: split frontmatter from body first. A robust
  splitter looks for `---\n` at the start of the file, then the
  next `---\n` line. The body is everything after the second
  `---\n` (trimmed). frontmatter failures are recoverable: warn
  and skip the file; don't fail the registry load.
- **The `name` uniqueness invariant** is enforced at load time,
  not at `get()` time. If two .md files in the same dir have
  `name: rust-reviewer`, the second one wins and the first is
  silently dropped (with a `tracing::warn!` logging the
  collision). This is friendlier than hard-failing.
- **`serde_yaml` deprecation note**: as of 2024, `serde_yaml` is
  in maintenance mode; `serde_yml` is a fork that's actively
  maintained. If `cargo add serde_yaml` triggers a deprecation
  warning, prefer `serde_yml`. The API is identical for our use
  case.
- **Cache invalidation**: the goal loads once at startup. The
  follow-up goal (out of scope) adds a `clear_cache()` method
  that Recursive's `config_reload` handler calls when the user
  edits a .md file. Don't pre-build the cache-invalidation
  plumbing now; the registry is small enough that reloading on
  next session is fine.
- **No new public API on `ToolRegistry`** — the `AgentTool`'s
  constructor takes `Arc<AgentDefinitions>`, and that's the only
  new field on the tool. The registry is not exposed to the LLM
  directly; the LLM sees the `agent` tool with a
  `subagent_type` string.

## Out of scope (DO NOT do these)

- Don't add plugin-agent loading. fake-cc's third source
  (`PluginAgentDefinition`) requires a plugin manifest system we
  don't have. The enum has a `Plugin` variant for forward
  compatibility, but the loader is left as a stub that always
  returns `Ok(vec![])`. A future goal will add plugin support.
- Don't add the cache-invalidation plumbing. Reload on next
  session is fine for now.
- Don't change the `permission_mode` values. Reuse
  `PermissionMode` as-is from `src/permissions/`. The
  frontmatter's `permission_mode` field is parsed to a string and
  matched against the existing variants.
- Don't change the unified `AgentTool` signature (goal 262's
  `AgentTool::new(pool, agent_defs)`). The constructor is the
  only new parameter; the public method set is unchanged.
- Don't touch `src/tools/a2a.rs`. External agent protocol is
  out of scope.
- Don't branch inside `src/agent.rs::Agent::run`. All new logic
  lives in `src/tools/agent_defs.rs`.
- Don't add observability/metrics for agent-definition load
  times. If the user wants metrics, that's a separate goal.
