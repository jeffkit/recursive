# Goal TEMPLATE — TUI feature (copy to `.dev/goals/<NN>-<tag>.md`)

> Copy this file for any goal that touches `crates/recursive-tui/`.
> The Acceptance + Notes sections embed the `tui-acceptance` workflow
> (`.dev/skills/tui-acceptance.md`) so the self-improve loop follows it
> automatically. Replace every `<…>` placeholder.

# Goal NN — <title>

**Roadmap**: Phase X.Y — <phase name> (part N/M)

**Design principle check**:
- Implemented as: <how it fits the architecture — new module / new file
  under `src/` / handler in `commands.rs` / etc.>
- ❌ Does NOT branch inside any main agent loop.

## Why
<motivation, 2–4 sentences. Name the user-visible symptom if it's a fix.>

## Scope (do exactly this, no more)

**Touches**: `crates/recursive-tui/src/<module>.rs`, …

### 1. <file or module>
<what to do, with a code sketch if helpful>

### 2. Tests — written at the rendered layer (REQUIRED, same change)
You MUST land a test covering the new/changed behaviour in the SAME
commit — this is a contract, not a suggestion. The `tui-presence` flow
gate fails the run if `crates/recursive-tui/src/` changes with no
test-bearing addition, and `tui-mutants` rejects tests that pass but
don't pin behaviour. Add a `#[cfg(test)]` test driving the change
through `Harness` (`use crate::harness::Harness;`). Prefer visual
assertions (`Screen::find_row` / `row_has_bg_color` / `text()` /
`numbered()`) over internal-state peeks. For highlight/marker alignment,
assert on the **specific** colour via `row_has_bg_color`, not a coarse
"any bg" check (panel base fills every row; the old `has_bg` helper was
removed for this). For terminal-IO behaviour (raw mode, mouse, alternate
screen) the in-process harness can't reach, add a PTY case in
`crates/tui-pty-harness/` or `crates/recursive-tui/tests/`.

<sketch the exact assertions the test should make>

## Acceptance

- `cargo test -p recursive-tui` green.
- `cargo clippy -p recursive-tui --all-targets -- -D warnings` clean.
- `cargo fmt --all --check` clean.
- **Presence gate**: `.dev/scripts/tui-test-presence.sh` exits 0 — a
  TUI src change shipped with a test-bearing addition. (Set
  `RECURSIVE_TUI_TEST_PRESENCE=0` only for a pure refactor with no
  behaviour change, and document why here.)
- **Effectiveness gate**: `.dev/scripts/tui-mutants.sh` (auto-detect on
  the touched files) exits 0 — no surviving mutants in the touched
  files. If survivors are in `lib.rs` terminal-IO code (raw mode, mouse,
  alternate screen), document them here as expected (covered by PTY
  tour, not in-process) rather than chasing them.
- **PTY acceptance**: after `cargo build -p recursive-tui`,
  ```
  cargo run -q -p tui-pty-harness -- run \
    --bin "$PWD/target/debug/recursive-tui" \
    --keys "<script>" --wait-ms <N> --snap numbered
  ```
  shows <the exact screen content the user should see>. Note: `--bin`
  takes an absolute path.
- <any additional functional criteria>

## Notes for the agent

- **Follow `.dev/skills/tui-acceptance.md`** — it is the canonical SOP
  for TUI work (in-process harness → mutation gate → PTY tour → gates).
- `Harness` is `#[cfg(test)]`-only; `cargo test -p recursive-tui` needs
  no `--features` flag (the `recursive` test-utils dev-dep is active in
  test builds).
- Use `apply_patch`; `.to_string()` over `.into()` in tests.
- **DO NOT modify files outside Scope.**
