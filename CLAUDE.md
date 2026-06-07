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
ls .worktrees/ 2>/dev/null
```

Don't edit files that a live worktree run is working on.

## Worktree workflow

All feature development happens in a dedicated worktree, not on the main
checkout at the project root. The main checkout (the project root itself)
is reserved for the `main` branch — it is the stable, non-bare working
tree used for shared admin tasks (fetch, merge, housekeeping). Each
feature worktree lives at `<project-root>/.worktrees/<name>/`, and
`.worktrees/` is git-ignored so worktrees never get accidentally
committed.

This separation keeps the main checkout clean, makes parallel feature
work safe, and prevents in-flight changes from colliding with the
stable branch. A worktree is a full working tree on a different branch,
so editing one does not touch the other.

## Skills available in this project

- `/recursive-loop` — act as the loop orchestrator: read roadmap, pick goals,
  launch `self-improve.sh`, handle results. Use this when the user wants
  Recursive to self-improve rather than you directly editing code.

<!-- gitnexus:start -->
# GitNexus — Code Intelligence

This project is indexed by GitNexus as **Recursive** (8120 symbols, 19794 relationships, 300 execution flows). Use the GitNexus MCP tools to understand code, assess impact, and navigate safely.

> If any GitNexus tool warns the index is stale, run `npx gitnexus analyze` in terminal first.

## Always Do

- **MUST run impact analysis before editing any symbol.** Before modifying a function, class, or method, run `gitnexus_impact({target: "symbolName", direction: "upstream"})` and report the blast radius (direct callers, affected processes, risk level) to the user.
- **MUST run `gitnexus_detect_changes()` before committing** to verify your changes only affect expected symbols and execution flows.
- **MUST warn the user** if impact analysis returns HIGH or CRITICAL risk before proceeding with edits.
- When exploring unfamiliar code, use `gitnexus_query({query: "concept"})` to find execution flows instead of grepping. It returns process-grouped results ranked by relevance.
- When you need full context on a specific symbol — callers, callees, which execution flows it participates in — use `gitnexus_context({name: "symbolName"})`.

## Never Do

- NEVER edit a function, class, or method without first running `gitnexus_impact` on it.
- NEVER ignore HIGH or CRITICAL risk warnings from impact analysis.
- NEVER rename symbols with find-and-replace — use `gitnexus_rename` which understands the call graph.
- NEVER commit changes without running `gitnexus_detect_changes()` to check affected scope.

## Resources

| Resource | Use for |
|----------|---------|
| `gitnexus://repo/Recursive/context` | Codebase overview, check index freshness |
| `gitnexus://repo/Recursive/clusters` | All functional areas |
| `gitnexus://repo/Recursive/processes` | All execution flows |
| `gitnexus://repo/Recursive/process/{name}` | Step-by-step execution trace |

## CLI

| Task | Read this skill file |
|------|---------------------|
| Understand architecture / "How does X work?" | `.claude/skills/gitnexus/gitnexus-exploring/SKILL.md` |
| Blast radius / "What breaks if I change X?" | `.claude/skills/gitnexus/gitnexus-impact-analysis/SKILL.md` |
| Trace bugs / "Why is X failing?" | `.claude/skills/gitnexus/gitnexus-debugging/SKILL.md` |
| Rename / extract / split / refactor | `.claude/skills/gitnexus/gitnexus-refactoring/SKILL.md` |
| Tools, resources, schema reference | `.claude/skills/gitnexus/gitnexus-guide/SKILL.md` |
| Index, status, clean, wiki CLI commands | `.claude/skills/gitnexus/gitnexus-cli/SKILL.md` |

<!-- gitnexus:end -->
