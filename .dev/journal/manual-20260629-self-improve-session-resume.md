# Manual edit: self-improve-session-resume

**Date**: 2026-06-29
**Goal**: Switch `.dev/scripts/self-improve.sh`'s resume paths from
`replay --resume-from N <transcript>` to native `recursive resume
--from-file <session-dir>` (the orthodox session-id resume), matching
the recursive CLI change in `manual-20260629-resume-session-id-converse.md`.

**Files touched**:
- `.dev/scripts/self-improve.sh` — after the first `run`, capture the
  session directory from the stderr line `session: recording to <dir>`
  (via `rg -o ... | cut -d' ' -f4`) into `SESSION_DIR`. Convert all
  three resume paths:
  1. **Auto-resume (BudgetExceeded)**: `resume --from-file "$SESSION_DIR"`
     (synthetic "Continue from where you left off." — lets the
     interrupted turn finish, no goal re-injection).
  2. **Resume-fix (cargo test failed)**: `resume --from-file
     "$SESSION_DIR" -p "$FIX_PROMPT"` (converse — appends the fix
     prompt as the next user message).
  3. **Clippy-fix / Smoke-fix**: same converse form with their
     respective prompts.
  Removed the `jq '.messages | length'` `RESUME_FROM` computation and
  the `command -v jq` gate from these blocks (no longer needed —
  session-id resume doesn't index into the transcript file). `jq`
  remains used only for the optional cost.json summary elsewhere.
- `e2e/fixtures/11-session-resume.json` — turn-1 fixture
  `userMessage` updated from `"resume-step1.txt"` to
  `"Continue from where you left off"` to match the new synthetic-
  continue resume behavior (turn 0 unchanged — still the run's
  original goal). Keeps `e2e/tests/11-session-resume.yaml` valid for
  a future container rebuild.

**Tests added**: none (shell script; verified via `bash -n` + the
resume mechanism already covered by recursive's unit tests and the
real-LLM smoke in the companion journal entry).

**Notes**:
- self-improve's resume paths operate on sessions that ended cleanly
  (BudgetExceeded / NoMoreToolCalls) — no orphan tool_calls — so
  `resume`'s non-TTY default orphan policy (abort) never trips.
- The resume-fix paths now resume from the live session dir (full
  history, including any prior auto-resume turn) instead of the
  frozen first-run transcript file — the fix agent sees its own
  prior attempts, which is more correct.
- **Container rebuild NOT done**: `recursive-e2e` is not running and
  `e2e/Dockerfile` is stale (builds `-p recursive-agent`, the lib,
  not `-p recursive-cli` which produces the `recursive` binary).
  Rebuilding requires a Dockerfile fix + heavy release build. Not
  necessary to land this change (test 11 is not in the self-improve
  smoke gate, which runs only `00-smoke.yaml`). Flagged as a
  follow-up: fix `e2e/Dockerfile` (`-p recursive-agent` →
  `-p recursive-cli`), rebuild, then run `e2e/tests/11-session-resume.yaml`
  to confirm the synthetic-continue fixture matches.
