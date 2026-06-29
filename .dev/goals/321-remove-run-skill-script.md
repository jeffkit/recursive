# Goal 321 — Remove `run_skill_script`; use `${SKILL_DIR}` + `Bash`

## Why

Recursive currently exposes a dedicated `run_skill_script` tool to
execute scripts bundled in a skill's `scripts/` directory. Reviewing
the Claude Code reference (`~/Downloads/fake-cc/src/skills/loadSkillsDir.ts`)
shows that Claude Code does **not** have a dedicated script-execution
tool: skill authors write `bash ${CLAUDE_SKILL_DIR}/scripts/foo.sh` in
the skill body, the `${CLAUDE_SKILL_DIR}` placeholder is substituted
with the skill's directory at load time, and the agent runs the
resolved command through the existing Bash tool.

Goal 320 added the equivalent `${SKILL_DIR}` substitution to Recursive's
`Skill` tool. With that in place, `run_skill_script` is redundant: it
duplicates the Bash tool's sandbox, permission pipeline, and timeout
machinery, and it occupies a tool slot in the registry that the LLM
must consider every turn. Removing it converges Recursive on the
single-`Skill`-tool model and shrinks the tool surface.

The security properties `run_skill_script` provided (script path stays
inside the skill directory, per-call timeout, shell-words argv safety)
are preserved by the existing `Bash` tool's sandbox + permission
pipeline + timeout; skill authors are responsible for referencing
scripts via `${SKILL_DIR}/scripts/<name>` rather than arbitrary paths.

## Goal

Delete the `run_skill_script` tool and remove it from every channel's
tool registry. The documented mechanism for running a skill's bundled
script becomes: load the skill via `Skill` (which substitutes
`${SKILL_DIR}`), then let the agent invoke `Bash` with the resolved
absolute path.

## Design

### 1. Delete the tool module

Delete `src/tools/run_skill_script.rs`.

### 2. Remove registration

In `src/tools/registry.rs`, remove the
`RunSkillScript::new(skills.to_vec(), workspace.to_path_buf(),
Duration::from_secs(shell_timeout_secs))` registration block inside the
`if !skills.is_empty() { … }` arm. Keep the `LoadSkill::new(...)`
registration in that same arm — the `Skill` tool stays.

If removing `RunSkillScript` leaves `shell_timeout_secs` or `workspace`
unused in that function, do **not** delete the function parameters
(other tools may still use them); only remove the now-unused binding if
the compiler/clippy flags it as dead. Prefer leaving the parameter in
place over changing the function signature (signature changes ripple to
callers and are out of scope).

### 3. Remove exports

In `src/tools/mod.rs`:
- Remove `pub mod run_skill_script;`.
- Remove `pub use run_skill_script::RunSkillScript;`.

In `src/lib.rs`:
- Remove any `pub use … RunSkillScript` re-export if present. Search
  for `RunSkillScript` across the workspace and remove remaining
  references.

### 4. Tests

The deleted file's unit tests go away with it. Check for any
**integration** test (under `tests/`) or other crate (e.g.
`crates/recursive-tui`, `crates/recursive-cli`) that references
`run_skill_script` or `RunSkillScript` and remove/update those
references. Notably:
- `run_skill_script_respects_permission_pipeline` (in the deleted file)
  is gone — it does not need to be re-homed; it tested a property of a
  tool that no longer exists.
- If any TUI/CLI test asserts on the tool list containing
  `run_skill_script`, update the assertion to drop it.

### 5. Docs

Update `docs/architecture/tools/skills-tools.md` to remove the
`run_skill_script` row and add a one-line note that skill scripts are
run via `${SKILL_DIR}` + `Bash` (cross-reference Goal 320's
`${SKILL_DIR}` documentation). Update `docs/architecture/tools/index.md`
and `docs/architecture/skills.md` if they list `run_skill_script`. If
any of these doc files do not exist in the baseline, skip them (do not
create new docs files).

## Scope (do exactly this, no more)

Files to touch:
- `src/tools/run_skill_script.rs` — DELETE.
- `src/tools/registry.rs` — remove the `RunSkillScript` registration.
- `src/tools/mod.rs` — remove module + re-export.
- `src/lib.rs` — remove re-export if present.
- `tests/**` and `crates/**` — remove/update any lingering
  `run_skill_script` / `RunSkillScript` references.
- `docs/architecture/**` — remove `run_skill_script` references.

Do NOT touch:
- `src/tools/load_skill.rs` (Goal 320 owns it; its `${SKILL_DIR}`
  support is what replaces this tool).
- `src/tools/find_skills.rs` (Goal 319 owns it).
- `src/tools/install_skill.rs` (stays).
- `src/skills.rs`, `src/skills_injector.rs` (unchanged).
- `src/run_core.rs`, `src/kernel.rs`, `src/runtime.rs` (the
  `globs_skills` plumbing stays — it powers Globs-mode injection, not
  script execution).
- Any file under `.dev/` other than this goal file.

## Acceptance

1. `cargo test --workspace` green.
2. `cargo clippy --all-targets --all-features -- -D warnings` clean
   (no dead-code warnings from leftover `RunSkillScript` references;
   no unused imports).
3. `cargo fmt --all` no diff.
4. `rg "run_skill_script|RunSkillScript" src/ tests/ crates/` returns
   no matches (the symbol is fully removed from product code and
   tests).
5. The `Skill` tool is still registered when skills are present; the
   `if !skills.is_empty()` arm in `registry.rs` still registers
   `LoadSkill`.

## Notes for the agent

- **Depends on Goal 320 being landed first.** If Goal 320 is not yet on
  your baseline (check `src/tools/load_skill.rs` for a
  `substitute_skill_dir` function), STOP and write a final message
  saying "Goal 321 prerequisite Goal 320 not yet landed" — do not
  attempt to implement Goal 320 yourself.
- Do not change the `build_standard_tools` (or
  `build_standard_tools_with_roots`) function signature. Only remove the
  one registration line and its immediately surrounding now-dead code.
- When removing the registration, preserve the `if !skills.is_empty()`
  guard structure — `LoadSkill` still depends on it.
- The `shell_words` and `tokio::process` imports in the deleted file
  disappear with it; do not remove those crates from `Cargo.toml` (other
  code may use them; dep removal is out of scope and risks breaking
  other crates).
- **DO NOT modify files outside the scope list.**
