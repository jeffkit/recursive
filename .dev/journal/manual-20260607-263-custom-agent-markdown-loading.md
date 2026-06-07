# Manual edit: 263 — custom agent markdown loading landed

**Date**: 2026-06-07
**Goal**: Add `src/tools/agent_defs.rs` — a unified agent-definition
registry that reads `~/.claude/agents/*.md` and
`<project>/.claude/agents/*.md` and unions them with the 2
built-in agent types (`explore`, `general_purpose`). The unified
`AgentTool` (from goal 262) resolves `subagent_type` against this
registry.

**Files touched** (vs main cf31e6e):
- `src/tools/agent_defs.rs` — **+454 lines** (new module)
- `src/tools/agent.rs` — +265 lines (wire `Arc<AgentDefinitions>` into
  unified `AgentTool`)
- `src/cli/builder.rs` — +11 lines (load at startup)
- `src/tools/mod.rs` — +2 lines (register module)
- `Cargo.toml` / `Cargo.lock` — `serde_yaml` dep added
- `.dev/{journal,observations,metrics,reviews}/` — agent artifacts

**Tests added**: 14+ unit tests in `agent_defs.rs` (parse,
load, resolve, error paths). Goal-spec: 14 cases required, all
implemented.

**Recovery actions**:
- E2E gate infrastructure bug — `self-improve.sh` auto-build
  of `e2e/plugins/dist/` tried `pnpm install` in the worktree,
  but `e2e/plugins/package.json`'s
  `file:../../../infra4agent/argusai/packages/core` dep only
  resolves from the MAIN repo (where `infra4agent` is a sibling
  of `Recursive`). From a worktree, `../../../` lands in
  `Recursive/.worktrees/`, not `~/projects/`. Two consecutive
  263 runs hit this and got force-reset by the EXIT trap. Fixed
  in two commits:
  - `b634ece fix(scripts): E2E plugin dist worktree fallback —
    copy from main repo` (initial fix; broken because
    `REPO_ROOT` in a worktree resolves to the worktree itself)
  - `78cf338 fix(scripts): use git-common-dir to find main repo
    for E2E plugin copy` (the actual fix: `git rev-parse
    --path-format=absolute --git-common-dir | xargs dirname`
    gives the main repo's root regardless of which worktree
    invokes the script, because worktrees share the main
    repo's `.git`)
- 3rd run (130627Z) was the charm: all gates green
  (cargo test 1130 + 15 bin, clippy, fmt, E2E smoke 3/0,
  review approved).

**Notes**:
- The 263 agent itself wrote good code on the FIRST attempt
  (the 1st and 2nd run both got past cargo test/clippy/fmt
  with all-green output; the gate that failed was the E2E
  build prereq, not the agent's work). So no goal-spec change
  was needed — only the script fix.
- The 1st failed run's worktree was force-removed by the EXIT
  trap; the 2nd's was also cleaned. The 3rd's was merged via
  `.dev/scripts/land-self-improve.sh` and then `git worktree
  remove` + `git branch -d`. All 3 dead branches are gone.
- The "READY TO LAND" pointer in the success log says
  `custom-agent-markdown-loading-deepseek-pro-20260607T130647Z-42863`
  but the actual worktree name is `...130627Z-42801` (TS
  mismatch — the log uses the *commit* timestamp, the worktree
  uses the *launch* timestamp). The land script handles the
  worktree path directly, so this is just a pointer-cosmetic
  issue; the 1st attempt using the log's short-id failed
  cleanly with "worktree not found".
- The `land-self-improve.sh` verdict-detection grep is
  `committed.*self-improve\|journaled to\|agent succeeded` —
  doesn't match the new "dev: observation" commit-message
  format. Result: the script shows the log tail and asks
  "Continue anyway?". Piped `Y` answers via stdin.

**Next**:
- Refresh GitNexus index for `agent_defs.rs` (new symbols).
- Launch goal 264 (coordinator mode + team/task tools —
  depends on 263's `AgentTool::call` resolving `subagent_type`
  against the registry).
