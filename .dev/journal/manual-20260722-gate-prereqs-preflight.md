# Manual edit: gate-prereqs-preflight

**Date**: 2026-07-22
**Goal**: Add a fail-fast preflight that checks quality-gate prerequisites (e2e's Docker/mcp2cli/argusai-mcp + mutants' cargo-mutants) BEFORE the agent runs, so a missing env prereq fails in seconds instead of after a 35-min agent run.
**Files touched**:
- `.dev/scripts/e2e-gate.sh` — added `--check-prereqs` mode (prereq checks only, no test run) and a Docker-daemon check (`docker info`) to the HARD-FAIL prereq section. Previously the script checked mcp2cli/argusai-mcp/e2e.yaml but NOT Docker — a down daemon only surfaced mid-suite at argus-setup.
- `.dev/flows/self-improve.flow.js` — new `assertGatePrereqs(repo)` helper + `preflight.gate-prereqs` step (after `preflight.provider-ping`, before the agent) and a `land.gate-prereqs` step in `landPreserve` (before the gate loop). Mirrors the existing `pingProvider` fail-fast philosophy.

**Tests added**: none (flow orchestration; validated manually).

**Notes**:
- Root cause of the earlier e2e `failed-preserved`: (1) the `normalizeGate` onFail config bug (fixed in `6fad84e`), and (2) the flow never checked e2e env prereqs — colima was down, the agent ran ~35 min, then the e2e gate red-lit. This edit removes cause (2).
- e2e check reuses `e2e-gate.sh --check-prereqs` (single source of truth for the mcp2cli/argusai-mcp/e2e.yaml resolution logic). On Docker-daemon-down, `assertGatePrereqs` best-effort runs `colima start` (90s timeout) then re-checks; if still missing it throws with actionable install/start instructions.
- mutants check: `cargo-mutants --version`; missing → fail-fast with `cargo install cargo-mutants` hint. Rationale: the mutant gates exit 2 when cargo-mutants is missing, which triggers resume-fix and the agent usually can't install it inside the sandboxed worktree — wasted rounds then failed-preserved. cargo-mutants is a one-time global install; failing fast upfront is the better trade (the only false-block is a goal that touches no crate, in which case mutant gates self-skip anyway — but self-improve goals almost always touch code).
- Validated: with colima stopped, `e2e-gate.sh --check-prereqs` exits 3 emitting "docker daemon（colima start / Docker Desktop 启动）" — the regex `/docker daemon/i` in `assertGatePrereqs` matches this and triggers the colima auto-start branch. With colima up + cargo-mutants 27.1.0 installed, the check passes.
- Also done this session (separate commits): published flowcast 0.6.1 to npm (0.6.0 was broken — `index.js` imports `./rate-limiter.js` but the `files` allowlist omitted it; 0.6.1 adds it), and switched this repo's `.dev/flows` dep from `file:../../../flowx` to npm `^0.6.0`.
