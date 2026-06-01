# Manual edit: plan-mode-v2-design

**Date**: 2026-06-01
**Goal**: Worktree cleanup, roadmap review, and design of Plan Mode 2.0 goals
**Files touched**: `.dev/goals/165-plan-mode-v2-core.md` (new), `.dev/goals/166-plan-mode-v2-http-sdk.md` (new), `.dev/ROADMAP-v4.md`
**Tests added**: none (design-only session)

## Summary

### Worktree cleanup
- Removed stale worktree `/Users/kongjie/projects/recursive-tui158` on branch
  `feat/g164-llm-revision`. The revert commit (`ad6a71f`) was an intermediate
  state from g164 development; g164 is already merged to main (`ebcb88f`).
- Worktrees remaining: `.worktree/feat-g165-sdk-subprocess` (g165, being
  worked on by another agent)

### Phase 19 audit
- Reviewed `feat/phase19-sdk-ecosystem` branch: all tests pass (Rust 770+,
  TypeScript 12, Python 21), clippy clean.
- Branch contains: Python SDK, TypeScript SDK, install.sh, Homebrew formula,
  multi-platform release CI, SDK gap analysis vs `@anthropic-ai/claude-agent-sdk`.
- Updated ROADMAP-v4.md Phase 19 status for 19.1/19.2/19.3.

### Plan Mode 2.0 design
Discussed Phase 18 direction with user. Decided to focus on 18.3 Hierarchical
Planning, implemented as a Plan Mode redesign matching Claude Code's
`EnterPlanMode` / `ExitPlanMode` pattern.

Key design decisions:
1. Plan mode is a **permission model** (agent-callable tools, not just user command)
2. `enter_plan_mode` tool blocks all write operations while active
3. `exit_plan_mode(plan)` presents a markdown document for user approval
4. User can iterate on the plan via chat before approving
5. Architecture supports TUI (modal) + HTTP (SSE + confirm/reject endpoints) + SDK

Two goals designed:
- **Goal 165**: Core redesign — `enter_plan_mode` / `exit_plan_mode` tools,
  read-only enforcement gate in agent loop, `PlanApprovalGate` mechanism,
  system prompt guidance, TUI modal update to show markdown plan.
- **Goal 166**: HTTP/SDK support — `plan_proposed` SSE event, new
  `POST /sessions/:id/plan/confirm` and `.../reject` endpoints,
  Python/TypeScript SDK `approve_plan()` / `reject_plan()` methods.

Reference: fake-cc `EnterPlanModeTool` / `ExitPlanModeTool` implementation
(~/Downloads/fake-cc/src/tools/).
