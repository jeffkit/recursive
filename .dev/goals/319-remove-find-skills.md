# Goal 319 — Remove `find_skills`; consolidate discovery into `skill_index`

## Why

Recursive currently exposes a `find_skills` tool that fuzzy-searches
locally installed skills by keyword. Reviewing the Claude Code reference
(`~/Downloads/fake-cc/src/tools/SkillTool/prompt.ts`) shows Claude Code
does **not** have a search tool: the skill catalog is injected into the
system prompt as a budget-controlled listing (1% of context window,
per-entry description truncated, degrade to names-only when over
budget), and the single `Skill` tool is a name-based invoker.

Recursive already injects a `skill_index` listing into the system prompt
on the CLI (`recursive-cli/src/cli/builder.rs`) and HTTP
(`src/http/handlers.rs`) channels. `find_skills` is therefore redundant:
it costs a tool slot and an LLM turn every time the agent wants to
browse, when the catalog is already in the system prompt. The one real
gap is that `skill_index` today has no budget control, so a large skill
library would blow up the system prompt. This goal closes that gap and
removes `find_skills`, converging on the single-`Skill`-tool discovery
model.

## Goal

1. Make `skill_index` budget-aware so it scales to many skills without
   unbounded system-prompt growth.
2. Remove the `find_skills` tool from every channel's registry.
3. Ensure all three channels (CLI, HTTP, TUI) inject `skill_index` so
   discovery works without a search tool. CLI and HTTP already do; TUI
   may not in the baseline — verify and add if missing.

## Design

### 1. Budget-aware `skill_index`

In `src/skills.rs`, change `skill_index` to honor a character budget:

```rust
pub fn skill_index(skills: &[Skill]) -> String {
    skill_index_with_budget(skills, default_skill_index_budget())
}

pub fn skill_index_with_budget(skills: &[Skill], char_budget: usize) -> String {
    // Same rendering as today, but:
    // - If total rendered length <= char_budget: return as-is (current behavior).
    // - Else: truncate each entry's description to a per-entry cap
    //   (char-boundary-safe via `crate::truncate_str`), recompute total.
    // - If still over budget: drop descriptions entirely, return
    //   names-only listing (`- <name>` per line, plus mode tag).
    // - Empty input → "" (unchanged).
}
```

Defaults:
- `default_skill_index_budget()` = 8000 chars. Override via env var
  `RECURSIVE_SKILL_INDEX_BUDGET` (parse with the same pattern used
  elsewhere for env-override defaults; read once, do not race — see
  AGENTS.md "Env-var tests must be ONE test").
- Per-entry description cap = 250 chars (matches Claude Code's
  `MAX_LISTING_DESC_CHARS`).

Keep the existing header line `Available skills (use ` + "`load_skill`" + ` to activate):` and the mode/ref/params/sections/depends_on/scripts suffixes; only the description field is subject to truncation. The names-only fallback still includes the mode tag (`[always]`/`[trigger]`/`[globs]`).

### 2. Remove `find_skills`

- Delete `src/tools/find_skills.rs`.
- In `crates/recursive-tui/src/runtime_builder.rs`, remove the
  `tools.register(Arc::new(recursive::tools::FindSkills::new(skills)))`
  line. Keep the `InstallSkill::new(skill_tx)` registration on the next
  line — `install_skill` stays.
- In `src/tools/mod.rs`, remove `pub mod find_skills;` and
  `pub use find_skills::FindSkills;`.
- In `src/lib.rs`, remove any `FindSkills` re-export.
- Search the workspace (`rg "FindSkills|find_skills"`) for remaining
  references in `tests/` and `crates/` and remove/update them.

### 3. TUI `skill_index` injection

In `crates/recursive-tui/src/runtime_builder.rs`, verify a
`tui_system_prompt` (or equivalent) helper injects `skill_index` into
the TUI system prompt. If the baseline already has it (added by the
prior manual TUI skill-injection work), leave it in place. If it does
NOT, add a helper mirroring `recursive-cli/src/cli/builder.rs`:

```rust
fn tui_system_prompt(base: &str, skills: &[Skill]) -> String {
    let idx = recursive::skills::skill_index(skills);
    if idx.is_empty() { base.to_string() } else { format!("{base}\n{idx}") }
}
```

and route the TUI's system prompt through it. Add unit tests
`tui_system_prompt_appends_skill_index` and
`tui_system_prompt_unchanged_when_no_skills` if and only if they do not
already exist in the baseline (do not duplicate existing tests).

### 4. Docs

Update `docs/architecture/tools/skills-tools.md` to remove the
`find_skills` row. Update `docs/architecture/tools/index.md` and
`docs/architecture/skills.md` if they list `find_skills`. Add a one-line
note that skill discovery is via the system-prompt `skill_index`
listing. Skip any doc file that does not exist in the baseline (do not
create new docs files).

## Scope (do exactly this, no more)

Files to touch:
- `src/skills.rs` — `skill_index` budget variant + defaults.
- `src/tools/find_skills.rs` — DELETE.
- `src/tools/mod.rs` — remove module + re-export.
- `src/lib.rs` — remove re-export if present.
- `crates/recursive-tui/src/runtime_builder.rs` — remove `FindSkills`
  registration; ensure `skill_index` injection (add only if missing).
- `crates/recursive-cli/src/cli/builder.rs` — only if it references
  `find_skills` (it should not); otherwise do not touch.
- `tests/**`, `crates/**` — remove/update lingering `find_skills`
  references.
- `docs/architecture/**` — remove `find_skills` references.

Do NOT touch:
- `src/tools/load_skill.rs` (Goal 320 owns it).
- `src/tools/run_skill_script.rs` (Goal 321 owns it).
- `src/tools/install_skill.rs` (stays).
- `src/tools/registry.rs` (the core registry does not register
  `find_skills`; it lives in the TUI runtime builder).
- `src/skills_injector.rs` (Globs-mode injection unchanged).
- Any file under `.dev/` other than this goal file.

## Acceptance

1. `cargo test --workspace` green, including new unit tests:
   - `skill_index_under_budget_unchanged` — a small skill set renders
     identically to the pre-change behavior.
   - `skill_index_over_budget_truncates_descriptions` — a skill set
     whose full listing exceeds the budget has per-entry descriptions
     truncated (char-boundary-safe, ellipsis or clean cut) and total
     length ≤ budget.
   - `skill_index_severely_over_budget_falls_back_to_names_only` —
     when even truncated descriptions exceed the budget, the listing
     degrades to `- <name>` lines (with mode tag) and total ≤ budget.
   - `skill_index_multibyte_description_truncation_no_panic` —
     descriptions containing multi-byte (CJK) chars near the truncation
     boundary do not panic (regression for the byte-slice bug fixed
     in `find_skills`; use `truncate_str`).
   - `skill_index_env_budget_override` — `RECURSIVE_SKILL_INDEX_BUDGET`
     overrides the default (single test, see AGENTS.md env-var rule).
   - `rg "FindSkills|find_skills" src/ tests/ crates/` returns no
     matches.
2. `cargo clippy --all-targets --all-features -- -D warnings` clean.
3. `cargo fmt --all` no diff.
4. TUI system prompt includes the skill index when skills are present
   (covered by `tui_system_prompt_appends_skill_index` or the existing
   equivalent).

## Notes for the agent

- **Ordering:** this goal is independent of Goals 320 and 321 but
  touches `src/tools/mod.rs`, which 321 also touches. If both are in
  flight, they MUST run serially (different runs, not parallel). The
  orchestrator is expected to run them one at a time.
- Use `crate::truncate_str` for all description truncation — it is
  char-boundary-safe and already exists. Never byte-slice a `String`
  that may contain multi-byte UTF-8.
- The env-var default read must follow the "one test, not many" rule
  from AGENTS.md to avoid env-var races in `cargo test`.
- When removing `FindSkills` from `runtime_builder.rs`, preserve the
  `InstallSkill::new(skill_tx)` registration immediately following it.
- Do not change the `skill_index` rendering for the under-budget case —
  existing callers (CLI, HTTP) depend on the current format. Only add
  the budget-truncation path.
- **DO NOT modify files outside the scope list.**
