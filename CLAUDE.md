# CLAUDE.md — Recursive Project

This file governs how Claude Code (you) behaves when directly editing
the Recursive codebase. These rules override defaults.

## What this project is

Recursive is a self-improving Rust coding agent. The source in `src/`
is the product. `.dev/` is the development meta-tooling (goals, scripts,
roadmap). You are editing the product, not the meta-tooling, unless
explicitly asked otherwise.

## Before touching any code

1. Read `.dev/AGENTS.md` — the full invariant list. Especially:
   - Invariant #1: Agent loop stays small. Don't branch inside `agent.rs::Agent::run`.
   - Invariant #3: Sandbox. All fs/shell tools go through `tools::resolve_within`.
   - Invariant #5: No `unwrap()`/`expect()` in non-test code.
   - Invariant #7: Finish reasons are data, not errors.
   - Invariant #8: Tool-call ↔ tool-result pairing must be preserved.

2. Check which files your change touches. If you're touching `src/agent.rs`
   main loop, reconsider — new capabilities belong in tools or providers.

## Mandatory quality gates (run before declaring done)

```bash
cargo test --workspace
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all
```

All three must be clean. If clippy has warnings, fix them — the self-improve
script treats clippy failures as rollback triggers.

## Code conventions

- **Prefer `apply_patch` discipline mentally**: when editing existing files,
  make minimal, surgical changes. Don't rewrite a whole file to fix one thing.
- **New tool** → new file under `src/tools/`, register in `src/tools/mod.rs`.
- **New provider** → new file under `src/llm/`, implement `LlmProvider` trait.
- **New capability** → never add it as a branch inside `agent.rs::Agent::run`.
- **Error variants** → add to `src/error.rs`. Never `unwrap()` in product code.
- **Tests** → `#[cfg(test)] mod tests` in the same file. Every new public
  function/tool/provider gets unit tests.

## After making changes

Write a brief journal entry under `.dev/journal/` in the format:
`manual-<YYYYMMDD>-<short-tag>.md`

```markdown
# Manual edit: <tag>

**Date**: YYYY-MM-DD
**Goal**: <what you changed and why>
**Files touched**: <list>
**Tests added**: <list or "none">
**Notes**: <anything non-obvious>
```

This keeps the observation history coherent with the self-improve loop runs.

## Parallel workflow context

This project also uses Recursive's **self-improve loop** (orchestrated via
`.dev/scripts/self-improve.sh`) where Recursive edits its own source.
If you're about to do work that could conflict with an in-flight self-improve
run, check first:

```bash
ls .dev/runs/ 2>/dev/null
ls .worktree/ 2>/dev/null
```

Don't edit files that a live worktree run is working on.

## Worktree workflow

All feature development happens in a dedicated worktree, not on the main
checkout at the project root. The main checkout (the project root itself)
is reserved for the `main` branch — it is the stable, non-bare working
tree used for shared admin tasks (fetch, merge, housekeeping). Each
feature worktree lives at `<project-root>/.worktree/<name>/`, and
`.worktree/` is git-ignored so worktrees never get accidentally
committed.

This separation keeps the main checkout clean, makes parallel feature
work safe, and prevents in-flight changes from colliding with the
stable branch. A worktree is a full working tree on a different branch,
so editing one does not touch the other.

## Skills available in this project

- `/recursive-loop` — act as the loop orchestrator: read roadmap, pick goals,
  launch `self-improve.sh`, handle results. Use this when the user wants
  Recursive to self-improve rather than you directly editing code.
