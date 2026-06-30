# Manual edit: e2e-session-path-fix

**Date**: 2026-06-30
**Goal**: Fix the E2E smoke gate failure flagged during the Goal 322 self-improve
run (mis-diagnosed by the resume-fix agent as an "aimock startup" issue). Root
cause was a session-path bug, not aimock.
**Files touched**:
- e2e/tests/00-smoke.yaml
- e2e/tests/01-basic-tools.yaml
- e2e/tests/02-session-persistence.yaml
- e2e/tests/03-memory-scratchpad.yaml
- e2e/tests/04-cost-tracking.yaml
- e2e/tests/05-sessions-export.yaml
- e2e/tests/07-llm-judge.yaml
- e2e/tests/24-bash-tool.yaml
- e2e/tests/25-glob-tool.yaml
- e2e/tests/26-facts-memory.yaml
- e2e/tests/27-todo-tool.yaml
- e2e/tests/28-episodic-recall.yaml
- e2e/tests/29-background-tasks.yaml
- e2e/tests/31-checkpoint-tool.yaml
- e2e/tests/33-utility-tools.yaml
- e2e/tests/36-session-rewind.yaml
- CLAUDE.md (corrected stale E2E "Session path isolation" guidance)
**Tests added**: none (existing e2e suites fixed)

## Root cause
`RECURSIVE_SESSIONS_DIR` is a **hard override** in `src/paths.rs::user_sessions_dir`
(returns the env value verbatim, ignoring `RECURSIVE_HOME`). The `recursive-e2e`
container sets `RECURSIVE_SESSIONS_DIR=/workspace/sessions` in `e2e/e2e.yaml`.
Every suite that ran `RECURSIVE_HOME=/tmp/rh-X recursive run ...` then did
`find /tmp/rh-X -name .meta.json` found nothing — the session had landed at
`/workspace/sessions`, not under `RECURSIVE_HOME`. The `if [ -n "$SESSION_DIR" ]`
guard silently skipped the copy, so the `recursive-session:` assertion failed
with "No session directory found under /tmp/sessions-X".

This was the **actual** gate.e2e failure in the Goal 322 run
(`smoke-01: session recorded write_file tool call — No session directory found`).
The resume-fix agent mis-read a separate ECONNREFUSED (it re-ran `recursive`
after argusai had already torn aimock down post-run) and concluded aimock
wasn't starting. aimock was fine — `argusai setup`/`run` start it and it loads
81 fixtures and listens on :4010.

## Fix
Applied the canonical pattern already used by `e2e/tests/11-session-resume.yaml`:
`unset RECURSIVE_SESSIONS_DIR` before the `recursive run` call in every affected
setup step (and in `36-session-rewind.yaml` case D, which re-discovers the
session via `sessions list`). Sessions now land under `RECURSIVE_HOME` and the
`find` locates `.meta.json`.

## Verification
Ran each fixed suite via `argusai -c e2e.yaml setup --skip-build` + `run -s <suite>`
in the `recursive-e2e` container:
- smoke, basic, session, memory, cost, export, judge, glob-tool, facts-memory,
  todo-tool, episodic-recall, background-tasks, checkpoint-tool, session-rewind
  → all green.
- bash-tool, utility-tools → the previously-masked session-path failure in
  scenario A is now fixed; this **exposed** two pre-existing, unrelated
  failures that were previously skipped by sequential fail-fast:
    - 24-bash-tool C: "Bash with cwd='../../' rejected" — sandbox error-message
      text assertion, unrelated to session paths.
    - 33-utility-tools B: "count_lines NOT in MCP serve tool list" — MCP tool
      registration, unrelated to session paths.
  Both are out of scope for this fix and left as-is.

## Notes
- The working tree has many other uncommitted modifications (crates/, src/,
  docs/, tests/) that are NOT from this session — pre-existing WIP. Only
  `CLAUDE.md` and `e2e/tests/*.yaml` were touched here.
- `argusai run` does NOT start the `recursive-e2e` service container; it must
  be preceded by `argusai setup` (or run via `.dev/scripts/e2e-gate.sh`,
  which does init/setup/run/clean through the argusai MCP).
- A benign `aimock auto-start failed: template parsing error` warning appears
  on some runs (argusai plugin's `docker inspect aimock` Go-template has a
  `{{range $k,$v := ...}}` comma issue); it is non-fatal — aimock is reachable
  and suites pass.
