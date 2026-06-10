# Goal 270 — E2E gate verify (no-op)

**Roadmap**: Phase 17 — Verify the self-improve.sh e2e gate (after
the `2b50e08` fix) is operational end-to-end.

**Design principle check**:
- Implemented as: a single-line trivial change that the agent
  will commit, then run the existing e2e gate
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- ❌ Does NOT add a new feature flag
- ❌ Does NOT modify any test, e2e fixture, or mcp integration

## Why

After Goal 267 (unified atomic_write), self-improve.sh failed
e2e gate with `SESSION_NOT_FOUND` from argusai-mcp. Lead
investigation in `2b50e08` added error capture to the init
phase. Manual probe (running argus-init/setup/run/clean by
hand) succeeded — but that bypasses self-improve.sh's
WORKTREE_ID propagation. We need a real self-improve run to
confirm the fix works in the full pipeline.

This is a deliberately trivial goal so the agent finishes the
product commit fast, and the e2e gate runs against that commit.
If the gate passes, we know the integration works. If it fails,
the captured init log tells us exactly where the chain breaks.

## Scope (do exactly this, no more)

### 1. Trivial product change

Add a single `tracing::trace!` line to a safe location (e.g.
the top of `src/main.rs` `fn main()`) that logs when the binary
starts. This produces a real diff for `cargo build` to include
in the binary, and a real commit for the e2e gate to test
against.

```rust
// near the top of src/main.rs:
tracing::trace!("recursive-agent starting (e2e gate probe)");
```

Place it after the logger is initialized, before any
significant work. If `tracing::trace!` is already used at the
top of `main`, place yours immediately after that.

### 2. Tests

No tests added. The e2e gate is the test.

### 3. Verification

- `cargo test --workspace` — green (existing tests only)
- `cargo clippy --all-targets --all-features -- -D warnings` —
  clean
- `cargo fmt --all` — applied

## Acceptance

- A commit lands with a one-line `tracing::trace!` (or similar
  trivial) addition
- The self-improve e2e smoke gate runs and:
  - **If PASSED** — done. Goal complete.
  - **If FAILED** — read `.dev/runs/e2e-init-argusai-wt-*.log`
    and the run log. Report what broke to the lead. Do NOT
    attempt to fix the e2e in this goal — that's a separate
    investigation.

## Notes for the agent

- The e2e gate has historically failed with `SESSION_NOT_FOUND`
  (g267) and an unrelated argusai MCP session issue. The fix in
  `2b50e08` captures init output to a side log file. If the
  gate fails, **read the init log first** — that is the highest
  signal-to-noise diagnostic.
- Do not modify any test, fixture, or mcp integration. This
  goal is purely a "verify the integration is working" probe.
- The change must be in `src/` (not `.dev/`, not docs) so the
  e2e binary is actually different from main's HEAD.
- Estimated diff: 1 file, 1 line.
- If `tracing::trace!` is unavailable at the chosen location
  (e.g. main not yet in a tracing context), use `eprintln!` —
  the goal is to ship a diff, not to add logging infrastructure.
