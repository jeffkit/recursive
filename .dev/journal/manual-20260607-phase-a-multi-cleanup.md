# Manual edit: Phase A — remove dead multi.rs code

**Date**: 2026-06-07
**Goal**: Phase A of multi-agent unification (architecture review P1).
Delete truly dead code from `src/multi.rs`: `Pipeline` /
`PipelineResult` / `StageOutcome` / `TeamOrchestrator` / `TeamResult`
/ `DelegationResult` / `parse_delegations`. Verified 0 callers
outside `multi.rs` itself (and the re-export in `lib.rs`).

## Files touched

- `src/multi.rs` — removed 504 lines of dead types and their tests
  (5 Pipeline tests, 3 parse_delegations tests, 2 TeamOrchestrator
  tests)
- `src/lib.rs` — removed 5 dead types from the `pub use multi::{...}`
  re-export
- `tests/v050_integration.rs` — removed the
  `multi_agent_pipeline_execution` integration test (it only
  exercised the dead `Pipeline` type) and cleaned up its now-unused
  imports

## Verification

- `cargo test --workspace` → 1386 passed, 0 failed
- `cargo clippy --all-targets --all-features -- -D warnings` → clean
- `cargo fmt --all` → clean (auto-removed a trailing blank line in
  the `mod tests` block)
- `gitnexus_impact` for `Pipeline` and `TeamOrchestrator` →
  `impactedCount: 0, risk: LOW` for both
- `gitnexus_detect_changes` → 0 affected processes

## Net diff

```
 src/lib.rs                |   5 +-
 src/multi.rs              | 504 ----------------
 tests/v050_integration.rs |  51 ----
 3 files changed, 4 insertions(+), 556 deletions(-)
```

## What was kept (and why)

`SharedMemory`, `MessageBus`, `AgentPool`, `AgentRole`,
`coordinator_system_prompt` — these are still used by the
`spawn_worker` / `spawn_workers_parallel` / `team_manage` tool
family. Phase B (unified `Agent` tool) will refactor that family
into one entry, at which point the multi-agent backbone will be
re-evaluated.

## Notes

This is the lowest-risk step in the multi-agent unification. It is
a pure deletion: no behavior changes, no API changes for callers
that use the still-live types, and the only test removal is one
test whose entire purpose was to exercise the dead `Pipeline` type.

A similar audit was performed for `team_manage.rs` and
`spawn_workers_parallel.rs`; they have callers (`SharedMemoryRead`
/ `SharedMemoryWrite` used by `spawn_worker` family) and are
**not** dead. They will be unified in Phase B, not deleted in
Phase A.

## Commit

`1995b7a` (squashed via merge `--no-ff` from
`chore/phase-a-cleanup-multi-dead-code`). Branch retained; can be
deleted after the next cleanup pass.

## Next

- Phase B: draft goal file for unified `Agent` tool (merge
  `sub_agent.rs` + `spawn_worker.rs` + `spawn_workers_parallel.rs` +
  `team_manage.rs` into one `agent_tool.rs` with fake-cc-style
  parameter-based dispatch on `subagent_type` / `team_name` /
  `run_in_background` / `isolation`).
- Phase C: draft goal file for custom agent markdown loading
  (`load_agents_dir.rs`).
- Phase D: draft goal file for coordinator mode + team/task tools.
- `a2a.rs` stays untouched across all four phases.
