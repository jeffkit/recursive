# Manual edit: goal-369 — doc accuracy sweep (max_steps default, removed-type refs, test-utils note)

**Date**: 2026-08-02
**Goal**: Public-facing docs that lie about current behavior — 5 issues, text/comment-only fixes.

## Files touched

- `README.md`
  - `:171` — `RECURSIVE_MAX_STEPS` row in the Configuration table: `| 32 |` → `| 0 (unlimited) |`,
    purpose column now says "Loop budget (0 = unlimited; set to N to cap at N steps)".
  - `:301` — same env var in the full env-var reference: `| 32 |` → `| 0 (unlimited) |`,
    purpose column now says "Max tool-call loop iterations per run (0 = unlimited)".
  - `:85-88` — MockProvider pointer: added "(requires the `test-utils` feature:
    `cargo run --example basic --features test-utils`)" after `examples/basic.rs`.

- `src/runtime/builder.rs`
  - `:156` — `max_steps` doc-comment: "default 32" → "default 0 (unlimited)". Code default
    (0) untouched — only the doc was wrong.

- `.dev/AGENTS.md`
  - Invariant #7 (`:123-124`): `Agent::run` → `AgentRuntime::run`, `AgentOutcome` →
    `RuntimeOutcome`, `{ finish: ... }` → `{ finish_reason: ... }`, `outcome.finish` →
    `outcome.finish_reason`. (RuntimeOutcome's field is `finish_reason`, verified at
    `src/runtime.rs:65-75`; `main.rs::exit_for_finish` reference left as-is — the function
    exists as `cli::output::exit_for_finish`.)

- `src/compact/mod.rs`
  - `:7` — module doc: `AgentBuilder::compactor(...)` → `AgentRuntimeBuilder::compactor(...)`.
    No other `AgentBuilder` reference exists in `src/compact/`.

- `tests/invariants/finish_reason_data.rs` (comment text only, no test logic)
  - `:3-4` — the invariant #7 quote mirroring `.dev/AGENTS.md` updated to
    `AgentRuntime::run` / `RuntimeOutcome { finish_reason }`.
  - `:9` — `outcome.finish` → `outcome.finish_reason` in the mirrored quote.
  - `:103` — `Ok(AgentOutcome { finish })` → `Ok(RuntimeOutcome { finish_reason })`.

- `tests/invariants/loop_size_orthogonality.rs` (comment text only)
  - `:3` — stale quote of invariant #1: `agent.rs::Agent::run` →
    `src/run_core.rs::RunCore::run_inner` (matches current canonical `.dev/AGENTS.md`).

## Tests added

None — docs/comments only. `cargo test --workspace` green, `cargo clippy --all-targets
--all-features -- -D warnings` clean, `cargo fmt --all` clean. Doc-tests unaffected
(only descriptive `///` text changed; the `builder.rs` doctests are `#[ignore]`d anyway).

## Notes

- **Judgment call — `finish` → `finish_reason`.** The goal's literal instruction was a
  type swap (`AgentOutcome` → `RuntimeOutcome`), but a naive swap would leave
  `Ok(RuntimeOutcome { finish: ... })` and `outcome.finish` — both lies, since
  `RuntimeOutcome` has `finish_reason`, not `finish` (src/runtime.rs:65-75). Fixed both
  occurrences in `.dev/AGENTS.md` and the mirrored quote in `finish_reason_data.rs`.
  This is the same "docs that lie" class the goal targets.
- **Left `loop_size_orthogonality.rs:70` as-is.** "after Goal 219 moved it out of
  `Agent::run`" is an explicit historical removal-pointer (same category as
  `src/agent/mod.rs:4`, which the goal says to leave alone), not a claim about the
  current API. The goal's `:70` mention was cross-checked and this is the rationale.
- **Out of scope, left stale (noted for a follow-up):** `docs/architecture/invariants.md:54`
  still says `AgentRuntime::run` returns `Ok(AgentOutcome ...)` — it was not in the goal's
  file list, so it was NOT touched. Also `docs/architecture/agent-loop.md:63` still shows
  `Agent::run` in the invariant-#1 quote (historical pointer). Neither is covered by the
  goal's acceptance greps.
- **README "540+ tests" understatement** (~3475 actual) untouched per the goal's explicit
  note — separate P3 concern.
- Acceptance greps all pass:
  - `rg "RECURSIVE_MAX_STEPS.*\| 32 \|" README.md` → 0 hits
  - `rg "default 32" src/runtime/builder.rs` → 0 hits
  - `rg "Agent::run|AgentOutcome|AgentBuilder::" .dev/AGENTS.md src/compact/mod.rs` → 0 hits
  - `rg "MockProvider" README.md` shows the `test-utils` feature alongside
