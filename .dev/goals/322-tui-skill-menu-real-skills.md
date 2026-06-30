# Goal 322 — TUI slash menu: load & surface real skills

## Why

Goal 169 wired skill-backed `/` slash commands into the TUI, but in
practice the menu shows **zero** skills and search finds none. Two
mismatches between the loader and the actual skill layout cause this:

1. **Format mismatch.** Real skills are directory-based:
   `<name>/SKILL.md` (e.g. `.recursive/skills/arch-sync/SKILL.md`,
   `~/.claude/skills/a2ui-render/SKILL.md`). But
   `SkillCommandLoader::load_dir`
   (`crates/recursive-tui/src/skill_commands.rs`) only reads **flat
   top-level `*.md` files** — it filters entries by the `.md` extension,
   so every `<name>/` directory is dropped. Even the project's own
   `.recursive/skills/arch-sync` and `.recursive/skills/rust-patch-discipline`
   never load. `search_skills` always returns empty, so the popup never
   renders skill rows and `/skill-name` dispatch is unreachable except by
   typing the full name blind.

2. **Path mismatch.** The loader scans `.recursive/skills/` and
   `~/.recursive/skills/`. The user's actual skill library lives in
   `~/.claude/skills/` (≈130 skills) and `.claude/skills/` (project).
   None of those paths are scanned, so the bulk of available skills are
   invisible to the TUI.

Meanwhile `src/skills.rs::discover_skills` (used by the agent's
`load_skill` tool) already parses the `<name>/SKILL.md` directory format
correctly. The TUI has its own second loader that drifted to a different
format.

Separately, even if skills loaded, the popup keyboard interaction is
broken for skill rows: `handle_command_menu_key` computes
`matches_count` from `CommandRegistry::search` alone (built-ins only),
so Down cannot move the highlight past the built-in block onto skill
rows; Enter on a selected skill row reads `registry.search(...).get(idx)`
(built-ins only) and silently no-ops; Tab completion never completes
skill names. The render path already merges built-in + skill entries,
so the index spaces between renderer and key handler are out of sync.

## Goal

Make the TUI `/` menu actually discover, display, search, keyboard-
navigate, Tab-complete, and dispatch the user's real skills — including
the directory-based `<name>/SKILL.md` format and the `.claude/skills/`
search paths — so that typing `/` surfaces skills, `/skill-name <args>`
expands the skill template and runs it, and arrow/Tab/Enter all work on
skill rows.

## Design

### 1. Teach the loader the directory format

In `crates/recursive-tui/src/skill_commands.rs`:

- Extend `SkillCommandLoader::load_dir` so that for each entry in a
  skill directory:
  - if the entry is a **file** ending in `.md`, parse it as today
    (flat-skill compatibility, keeps existing tests green);
  - if the entry is a **directory** containing `SKILL.md`, parse
    `<dir>/SKILL.md` and use the directory name as the default `name`
    (when frontmatter omits `name`).
- Reuse the existing `parse_content` for the actual frontmatter + body
  parse so behaviour stays consistent. The `source_path` for a
  directory skill is `<dir>/SKILL.md`.

Prefer reusing `recursive::skills::discover_skills` for the directory
walk + frontmatter parse rather than duplicating the format logic, if
it can be adapted without changing its public signature. If reuse would
require a non-trivial refactor, keep the TUI loader self-contained but
make its frontmatter parsing match `discover_skills` semantics (name
from frontmatter `name` or dir name; description from frontmatter or
first non-blank body line). State the chosen approach in the journal
entry.

### 2. Expand search paths

In `SkillCommandLoader::load`, scan in this priority order (first
name wins on collision, mirroring today's semantics):

1. `<workspace>/.recursive/skills/`
2. `<workspace>/.claude/skills/`
3. `~/.recursive/skills/`
4. `~/.claude/skills/`

Do **not** scan `~/.cursor/skills-cursor/` (explicitly out of scope —
those are Cursor-IDE skills, not Recursive skills). Built-in commands
still shadow skill commands on name collision via `lookup_skill`'s
existing guard.

### 3. Fix popup keyboard interaction

In `crates/recursive-tui/src/app/commands.rs::handle_command_menu_key`:

- Replace the three built-in-only lookups (`matches_count` on line ~548,
  the Tab branch's `registry.search(...)` on ~573, and the Enter
  branch's `registry.search(...)` on ~589) with a single merged list
  that matches the renderer's order exactly: built-ins first (from
  `search`), then skills (from `search_skills`), truncated to
  `command_menu::MAX_VISIBLE`. Introduce a small helper (e.g.
  `fn command_menu_entries(&self) -> Vec<CommandMenuEntry>` or reuse
  `ui::command_menu::MenuEntry` if it can be made public) so the
  renderer and the key handler share one source of truth for ordering
  and length.
- Down: clamp to `combined.len().min(MAX_VISIBLE)`, not
  `matches_count` of built-ins only.
- Enter on a selected row: set `prompt.buffer` to the chosen entry's
  name (skill or built-in), then fall through to the regular submit
  path so `/skill-name <typed args>` dispatches via
  `dispatch_slash_command` → `lookup_skill` → `expand`.
- Tab: complete to the selected (or first) entry's name when unique;
  keep existing behaviour when no skill is involved.

### 4. Show argument_hint in the popup

In `crates/recursive-tui/src/ui/command_menu.rs`, for `MenuEntry::Skill`,
append the skill's `argument_hint` (dim colour) after the description
when non-empty, so users see `/refactor <file>` style usage inline.
Leave built-in rows unchanged (their `usage` is already visible in
`/help`).

### 5. Lazy reload on `/`

Skills are added/edited on disk while the TUI runs. Add a cheap reload:
when the input transitions into `InputMode::Command` (the `/` keypress),
compare a cached `(dir, mtime)` pair for each of the four search paths
against the current mtimes; if any changed, re-run
`SkillCommandLoader::load(&workspace)` and replace `app.commands`'s
skill list via `with_skill_commands` (or a new
`set_skill_commands(&mut self, …)` method on `App` that rebuilds the
registry in place). No file watcher; re-scan is debounced to the `/`
keypress. If a full registry rebuild risks dropping runtime-registered
built-ins, prefer a method that swaps only the `skill_commands` field.

### 6. Tests

Add unit tests in the touched files:

- `skill_commands.rs`: a tempdir with `<name>/SKILL.md` (directory
  format) is loaded with the correct name, description, and
  `prompt_template`; a flat `name.md` still loads (regression); a
  directory without `SKILL.md` is skipped; `.claude/skills` path is
  scanned when present.
- `commands.rs` (or `app/commands.rs` test module): with a fake skill
  registered, `handle_command_menu_key` Down moves the highlight onto
  the skill row (index ≥ built-in count); Enter on that row sets
  `prompt.buffer` to the skill name; Tab completes the skill name from
  a prefix.
- `command_menu.rs` tests: the rendered lines for a skill with a
  non-empty `argument_hint` contain the hint text.

Follow `.dev/AGENTS.md` rules: env-var tests collapsed into one;
network tests carry explicit timeouts; `Message::user("x".to_string())`
not `.into()` in tests.

## Scope (do exactly this, no more)

Files to touch:
- `crates/recursive-tui/src/skill_commands.rs` — directory format +
  expanded paths + tests.
- `crates/recursive-tui/src/app/commands.rs` — merged menu entries,
  keyboard interaction fixes + tests.
- `crates/recursive-tui/src/ui/command_menu.rs` — `argument_hint`
  rendering; expose a shared entries helper if needed + tests.
- `crates/recursive-tui/src/app/state.rs` (or `app.rs`) — lazy reload
  hook on entering `InputMode::Command`, if a clean attach point
  exists; otherwise wire from the `/` key handler.

Do NOT touch:
- `src/skills.rs`, `src/skills_injector.rs` (agent-side skill system;
  unchanged unless step 1 reuses `discover_skills` without a signature
  change).
- `src/tools/load_skill.rs`, `src/tools/find_skills.rs` (Goals 320/319
  own them).
- HTTP `/slash-commands` endpoint and Python SDK
  `list_slash_commands()` (separate channels; out of scope — this goal
  is TUI-only).
- Any file under `.dev/` other than this goal file.
- `Cargo.toml` (no new deps).

## Acceptance

1. With `.recursive/skills/arch-sync/` and `~/.claude/skills/` present,
   starting the TUI and pressing `/` shows skill rows in the popup
   (verifiable via a unit test that constructs an `App` with a temp
   workspace containing a `<name>/SKILL.md` and asserts
   `search_skills` / the merged menu entries are non-empty).
2. `SkillCommandLoader::load_dir` loads both directory-based
   `<name>/SKILL.md` and flat `name.md` skills (unit test).
3. `handle_command_menu_key` Down moves the highlight onto a skill row
   and Enter selects it (unit test).
4. Tab completes a skill name from a prefix (unit test).
5. Skill rows with a non-empty `argument_hint` render the hint (unit
   test on the rendered lines).
6. Editing/adding a skill file and pressing `/` again surfaces it
   without restarting the TUI (unit test covering the mtime-cached
   reload path).
7. `cargo test --workspace` green.
8. `cargo clippy --all-targets --all-features -- -D warnings` clean.
9. `cargo fmt --all` no diff.
10. E2E smoke (`cd e2e && argusai -c e2e.yaml run -s smoke`) green.

## Notes for the agent

- The single most important fix is step 1 + step 2: without them the
  menu is empty regardless of interaction fixes. Land and test the
  loader first, then the interaction.
- Keep `parse_content` as the single frontmatter parser; do not fork
  parsing logic between flat and directory formats.
- Built-ins always shadow skills on name collision — preserve
  `lookup_skill`'s `if self.lookup(name).is_some() { return None; }`
  guard.
- The merged menu entries helper must use the **same ordering and
  truncation** as `ui::command_menu::render` / `render_command_panel`,
  or the highlight index will desync from the drawn rows (this is the
  existing bug). Share one function between renderer and key handler.
- `~/.cursor/skills-cursor/` is deliberately excluded — do not scan it.
- If `App::new` (in `app/state.rs`) is the only place `commands` is
  built, the lazy reload must mutate the existing `App.commands` rather
  than constructing a fresh `CommandRegistry::default_set()` (which
  would be fine but verify no runtime-registered built-ins exist that
  would be lost).
- DO NOT modify files outside the scope list.
