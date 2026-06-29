# Manual edit: e2e-dockerfile-resume

**Date**: 2026-06-29
**Goal**: Fix the broken `e2e/Dockerfile` so the `recursive-e2e` container can be
rebuilt, and make `e2e/tests/11-session-resume.yaml` actually validate the new
`recursive resume` (sessionid + synthetic continue) behavior end-to-end.

**Files touched**:
- `e2e/Dockerfile` — build the `recursive` binary from the `recursive-cli`
  crate instead of the `recursive-agent` lib crate.
- `e2e/tests/11-session-resume.yaml` — `unset RECURSIVE_SESSIONS_DIR` in the
  run/resume commands and guard the `cp -r "$SESSION/."` against an empty path.

**Tests added**: none (test-infra fix; verified by rerunning the existing suite).

**Notes**:
- The Dockerfile was stale: `cargo build --release -p recursive-agent` only
  builds the root lib crate (post `recursive-cli`/`recursive-agent` split) and
  does NOT produce the `recursive` binary. The `COPY --from=builder
  /build/target/release/recursive` step therefore failed on a fresh build. The
  3-week-old `recursive:e2e` image had been built with an older Dockerfile
  (before the split). Fixed by building `-p recursive-cli` (whose `[[bin]]`
  target is `recursive`).
- Test 11 had a pre-existing conflict with `e2e.yaml`: the container-wide
  `RECURSIVE_SESSIONS_DIR=/workspace/sessions` overrides `RECURSIVE_HOME`, so
  the first run's session landed in `/workspace/sessions`, the test's
  `find /tmp/rh-resume` returned nothing, `SESSION` came back empty, and
  `cp -r "$SESSION/."` devolved into `cp -r "."` — copying the entire root
  filesystem into `/tmp/sessions-resume` (filled Docker disk, 17 GB reclaimed
  on cleanup) and leaving no `.meta.json` for the `recursive-session:` assertion.
  This was independent of the resume change. Fix: `unset RECURSIVE_SESSIONS_DIR`
  so sessions land under `RECURSIVE_HOME=/tmp/rh-resume`, plus a non-empty
  path guard before `cp`.
- Workflow note: `argusai -c e2e.yaml run` does NOT create the `recursive-e2e`
  container — it must be pre-created with `argusai -c e2e.yaml setup` (or
  `rebuild`). Running `run` without `setup` silently no-ops the exec steps
  (argusai reports ✓ without checking the exec exit code) and the assertion
  then fails with "No session directory found".

**Verification**:
- `argusai -c e2e.yaml setup` → image built, `recursive-e2e` container started.
- `argusai -c e2e.yaml run -s resume` → `2 passed, 0 failed, 0 skipped`
  (Resumed run produced a valid completed session ✓; resume-step1.txt side
  effect persists ✓). Confirms the synthetic "Continue from where you left
  off." resume path reaches `status: completed` by appending to the same
  session directory.
