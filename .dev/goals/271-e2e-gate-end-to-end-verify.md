# Goal 271 — Self-improve e2e gate end-to-end verify

**Roadmap**: Phase 17 (Production Hardening) — meta-goal
to validate the self-improve plumbing built in
2b50e08 / db7fbc4 / 0e8d037.

**Design principle check**:
- Implemented as: a single-line trivial change that the agent
  will commit, then run the existing e2e gate
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- ❌ Does NOT add a new feature flag
- ❌ Does NOT modify any test, e2e fixture, or mcp integration

## Why

After Goal 269, we have 4 self-improve plumbing fixes
landed on main:
- `2b50e08` — surface argus-init errors
- `db7fbc4` — build e2e/plugins before argus-init
- `0e8d037` — resolve e2e plugins argusai-core path correctly
  from any worktree (handles the `.worktrees/<name>/`
  prefix that broke the relative `../../../` path)

We have **never** actually run a self-improve end-to-end with
all four fixes in place. The previous goal 270 (a 1-line
noop) failed at the e2e plugins build step — exactly the bug
that 0e8d037 fixes. This goal re-runs the noop and verifies
the entire e2e gate works.

If the gate passes, future self-improve runs can rely on
the plumbing. If it fails, the captured init log tells us
exactly where the chain breaks (thanks to 2b50e08).

## Scope (do exactly this, no more)

### 1. Trivial product change

Add a single `tracing::trace!` line to `src/main.rs` `fn main()`
near the top (after logger init). This produces a real diff
for the e2e binary.

If `tracing::trace!` is already used at the top of `main`,
place yours immediately after. If `tracing` is not in scope
at that location, use `eprintln!` — the goal is to ship a
diff, not to add logging infrastructure.

### 2. Tests

No tests added. The e2e gate is the test.

### 3. Verification

- `cargo test --workspace` — green
- `cargo clippy --all-targets --all-features -- -D warnings` —
  clean
- `cargo fmt --all` — applied
- `cargo build` — produces a binary

## Acceptance

- A commit lands with a one-line addition
- The self-improve e2e smoke gate runs and:
  - **E2E PASSED** → write a brief observation confirming the
    plumbing works. Goal complete.
  - **E2E FAILED** → read `.dev/runs/e2e-init-argusai-wt-*.log`
    and the run log. Report the init error to the lead. Do
    NOT attempt to fix the e2e in this goal — separate
    investigation.

## Notes for the agent

- The four plumbing fixes are in place. The e2e gate is
  expected to pass: init/setup/run/clean were all manually
  verified end-to-end after each fix landed.
- If the gate fails, the most likely remaining failure modes
  are:
  1. Docker daemon not running (check `docker info` first)
  2. argusai-mcp server binary mismatch (we hand-built it in
     the main repo but the worktree may not have rebuilt
     after e2e/plugins changes — should auto-fix on next
     self-improve run since we now install fresh)
  3. WORKTREE_ID env not propagating to mcp server sub-process
     (symptom: namespace empty in init log)
- Keep the change minimal. 1 file, 1 line. The goal is to
  verify plumbing, not to ship a feature.
- If you are tempted to add tests, write a `#[ignore]`'d one
  with a comment explaining why it's ignored — do NOT add
  blocking tests in this goal.

## Provenance

This is a re-run of goal 270 (which failed for plumbing
reasons now fixed). The body is essentially identical to
`.dev/goals/270-e2e-gate-verify.md` — keep the same scope.
