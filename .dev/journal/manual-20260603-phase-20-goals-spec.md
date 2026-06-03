# Manual edit: phase-20-goals-spec

**Date**: 2026-06-03
**Goal**: Spec out Phase 20 (v0.7 Refactor & Hardening) — 11 top-level goals + execution order
**Files touched**:
- `.dev/goals/219-07-refactor-delete-deprecated-agent.md` (new, A 类)
- `.dev/goals/220-07-refactor-split-tui-app.md` (new, A 类)
- `.dev/goals/221-07-refactor-split-session.md` (new, A 类)
- `.dev/goals/222-07-llm-provider-trait-split.md` (new, A 类)
- `.dev/goals/223-07-error-typed-variants.md` (new, A 类)
- `.dev/goals/224-07-lint-deny-unwrap-used.md` (new, C 类 — 收尾)
- `.dev/goals/225-07-tools-mod-split.md` (new, B 类)
- `.dev/goals/226-07-sub-crate-tui-extract.md` (new, A→B)
- `.dev/goals/227-07-invariant-e2e-guard-tests.md` (new, C 类)
- `.dev/goals/228-07-goal-dependency-graph.md` (new, C 类)
- `.dev/goals/229-07-unwrap-cleanup-batch.md` (new, B 类 — 模板)
- `.dev/ROADMAP-v4.md` (modified — added Phase 20 section + Batch 42-46 in execution order)

**Tests added**: none (spec-only commit)

**Notes**:
- v0.7 accepts breaking change; version bump 0.6.0 → 0.7.0
- 11 goals split by execution mode: A (human/Claude), B (self-improve mechanical), C (self-improve policy)
- Critical path: 229-NN unwrap batches must complete before 224 deny lint ships
- 219 starts immediately after this commit lands; the worktree `refactor/219-delete-deprecated-agent` will branch from the post-commit HEAD
- The ` D .dev/goals/194-vector-memory.md` pre-existing delete is unrelated and intentionally left alone
