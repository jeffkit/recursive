# Manual edit: 268 — fix E2E basic suites 01-05

**Date**: 2026-06-08
**Goal**: Apply session path fix + tool name aliases to E2E suites 01-05

**Outcome**: self-improve agent (MiniMax-M3, 85 steps, $1.96) did the work
correctly but left it uncommitted ("Commit is out of scope per goal spec").
Git object database corruption in the worktree prevented landing via the
normal land script. Files copied manually to main and committed as `eee375e`.

**Files touched**:
- `e2e/tests/01-basic-tools.yaml`
- `e2e/tests/02-session-persistence.yaml`
- `e2e/tests/03-memory-scratchpad.yaml`
- `e2e/tests/04-cost-tracking.yaml`
- `e2e/tests/05-sessions-export.yaml`
- `src/cli/builder.rs` — added `list_dir`/`glob` alias for GlobTool,
  `search_files` alias for SearchFiles

**Tests**: 1164 lib tests passed

**Notes**:
- v1 (268 original) spent 200 steps debugging Docker binary arch issues — goal
  file said "run E2E to verify" which the agent took literally. Fixed by
  rewriting goal to ban E2E execution entirely.
- v2 (268 rewrite) worked correctly in 85 steps, clean pattern-A+B application.
- Goal file should always have "do NOT commit" replaced with explicit commit
  instructions, or agent will leave work uncommitted.
- Git object DB corruption in worktree was caused by the worktree having no
  commits (brand-new branch) combined with staged files referencing objects
  from main — a rare but known git worktree edge case.

**Next**:
- Update argusai plugin skill to prioritize MCP tools over CLI (isolation support)
- Wire `isolation.namespace` into CLI path (Option C from gap analysis)
- self-improve.sh: replace `argusai` CLI calls with `mcp2cli` → `argus_setup`/`argus_run`
