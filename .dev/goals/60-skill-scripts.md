# Goal 60 — Skill scripts/ executable scripts

**Roadmap**: Phase 5.2 — Skill scripts/ the agent can invoke

**Design principle check**:
- Implemented as: extension to `src/skills.rs` + new tool in `src/tools/`.
  No agent loop changes.
- Does NOT modify `agent.rs`.

## Why

Skills currently provide only markdown documents. Many real skills need to
execute scripts (linters, formatters, test runners, code generators). Adding
a `scripts/` directory to skills lets skills bundle executable utilities that
the agent can invoke by name, sandboxed to the workspace.

## Scope (do exactly this, no more)

### 1. `src/skills.rs` — extend Skill struct with scripts

Add a `scripts` field to `Skill`:

```rust
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub refs: Vec<SkillRef>,       // from g59 if landed, else Vec::new()
    pub scripts: Vec<SkillScript>,
}

#[derive(Debug, Clone)]
pub struct SkillScript {
    /// Script name (filename without extension), e.g. "lint"
    pub name: String,
    /// Absolute path to the script file
    pub path: PathBuf,
    /// Brief description from the first comment line (if present)
    pub description: String,
}
```

In `discover_skills`, scan `<skill_dir>/scripts/` for executable files
(any file with execute permission, or common script extensions: `.sh`,
`.py`, `.rb`, `.js`). For `description`, read the first line — if it
starts with `#` (shebang excluded) or `//`, use that as description;
otherwise use empty string.

**Important**: if g59 (skill refs) has NOT landed yet, just add an empty
`refs: Vec::new()` field or omit it. Don't depend on g59.

### 2. `src/tools/run_skill_script.rs` — new tool

Create a new tool `run_skill_script`:

```
Parameters:
  - skill: string (required) — name of the skill
  - script: string (required) — name of the script within the skill
  - args: string (optional) — arguments to pass to the script
```

Execution:
1. Find the skill by name (case-insensitive)
2. Find the script by name within that skill
3. Execute it with `tokio::process::Command`:
   - Working directory: the agent's workspace (from config/env)
   - Timeout: same as run_shell (from config, default 30s)
   - Capture stdout + stderr
   - Pass `args` as shell arguments (split on whitespace, or pass to `sh -c`
     with the script path prepended)
4. Return combined output (stdout + stderr), capped at 10000 chars

**Security**: The script path must be within the skill directory. Validate
with a simple `starts_with` check on the canonical path.

### 3. `src/tools/mod.rs` — register the new tool

Add `run_skill_script` to the tool registry alongside `load_skill`.

### 4. `src/skills.rs` — update skill_index to show script count

When rendering skill index, show script names if present:

```
- rust-traits: Explain Rust trait design [scripts: lint, format]
```

### 5. Tests

- Test: `discover_skills` populates `scripts` from `scripts/` directory
- Test: `discover_skills` extracts description from first comment line
- Test: `run_skill_script` executes a simple shell script and returns output
- Test: `run_skill_script` errors on unknown skill or script name
- Test: `run_skill_script` respects timeout (use a `sleep 999` script)
- Test: `skill_index` shows script names

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all -- --check` clean
- No regressions

## Notes for the agent

- Read `src/skills.rs` for the Skill struct.
- Read `src/tools/shell.rs` for how `run_shell` handles timeout + output
  capture. Reuse the same pattern.
- Read `src/tools/mod.rs` for how tools are registered.
- For the "executable" check: on Unix, check file permissions with
  `std::os::unix::fs::PermissionsExt` (mode & 0o111 != 0). Alternatively,
  just check for known script extensions (.sh, .py, .rb, .js).
- Create the new tool file as `src/tools/run_skill_script.rs`.
- Use `tokio::process::Command` (already used in shell.rs).
- The workspace path can be obtained from `arguments["workspace"]` if you add
  it as a tool parameter, OR you can store it in the tool struct at
  construction time (preferred — see how LoadSkill stores skills).
