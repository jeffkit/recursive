# Manual edit: self-improve-hardening

**Date**: 2026-06-04
**Goal**: Harden the self-improve loop with three improvements identified via review
**Files touched**:
- `.dev/goals/231-review-static-invariant-checks.md` — new goal file
- `.dev/goals/232-goal-complexity-hint.md` — new goal file
- `.dev/goals/233-ops-parallel-conflict-rule.md` — new goal file
- `.dev/OPERATIONS.md` — added §3.2.1 parallelism safety rule + tip about Complexity hint
- `.dev/scripts/self-improve.sh` — added complexity hint: `## Complexity: hard` escalates to pro tier and 400 steps
- `.dev/scripts/review-changes.sh` — added static invariant checks: unwrap/expect (critical, auto-reject) and sandbox bypass (warning)
**Tests added**: none (meta-tooling changes, not product code)
**Notes**:
- Static checks use grep on added diff lines; test-context lines are filtered
  by presence of `#[cfg(test)]` or `#[test]` markers (rough but effective)
- Sandbox check is warning-only to avoid false positives on legitimate Path::new uses
- Complexity hint is advisory: caller-set RECURSIVE_PROVIDER/MAX_STEPS always wins
- Reviewer cross-provider: deepseek product → minimax review, minimax product → deepseek review
- Review prompt: added revision guidance section (root cause, actionable suggestions)
- Goals 234 and 235 written for Recursive to self-implement (multi-round revision, reviewer with tools)
