# Manual edit: branch-hygiene

**Date**: 2026-07-14
**Goal**: Land mutation-baseline into main, open ACP PR, clean stale worktrees/branches
**Files touched**:
- Merged PR #10 (`chore/mutation-baseline`) — fmt fix + debt doc + rebase onto main
- Opened PR #11 (`feature/acp-protocol-support`) — rebase + resolve `e2e/e2e.yaml`
- Deleted stale local branches/worktrees and squash-merged remotes
**Tests added**: none (existing mutation pins landed via PR #10)
**Notes**:
- PR #10 CI had failed solely on `cargo fmt --check`; fixed after rebase.
- ACP conflict resolution kept both ACP init suite and renamed “Basic Agent Tool Usage”.
- PR #11 Windows CI: unused `grace_period` on non-Unix + TempDir backslashes breaking JSON in tests; both fixed before merge.
- Left in place: preserve/otel detached HEAD (e2e-failed salvage, deferred).
- Discarded obsolete `self-improve/count-lines-*` (already on main).
