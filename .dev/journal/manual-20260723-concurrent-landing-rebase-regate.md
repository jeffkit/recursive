# Manual edit: self-improve flow 支持并发落地（rebase + re-gate）

**Date**: 2026-07-23
**Goal**: 修掉 `commit` 步骤的 `mainHead === baseline` 硬校验——它在并发 flow 下必然误伤
（第二个完成的 flow 一定 `mainHead !== baseline` → 拒绝落地 → 走 preserve）。换成「main 动了就
rebase 到当前 mainHead + 在合并树上重跑门 + 绿才落」，让并发 flow 能自动安全落地，且保住
「落下的 == 被门验过的」不变量。同时堵住 land-preserve 的泄漏（旧实现门验 B0+ours 却落到 B1）。

**Files touched**:
- `.dev/flows/self-improve.flow.js`
  - `commit` 步骤拆成 `commit.prep` / `commit.rebase` / `regate.*` / `commit.land`：
    - `commit.prep`：worktree 提交 agent 改动，判定 `mainMoved = mainHead !== baseline`。
    - 快路径（main 未动）：`commit.land` 直接 cherry-pick（落 B0+ours，== 被门验过的）。
    - 慢路径（main 已动）：`commit.rebase` 把 detached worktree `git rebase <mainHead>` 得到
      B1+ours（返回 full sha 从返回值赋给 wtSha，避免 resume 时回调被跳过、wtSha 退回旧 sha
      的 bug）；`runRegate` 在 rebased worktree 上重跑完整门（`regate.<name>` key，不跳过、
      不带 fix 循环，门红即 preserve）；`commit.land` cherry-pick rebased sha（父即 mainHead，无冲突）。
  - 新增 `runRegate(worktreeDir)`：复用 `qualityGatesFor`+`loadGates`+`normalizeGate`+`runGate`，
    但 step key 用 `regate.<name>`（`gate.<name>` 已 completed 会被 cp.step 跳过，故不能直接复用
    `runQualityGates`），不带 resume-fix 循环（rebase 引入的语义冲突交给 preserve/人，agent 已收工）。

**Tests added**: none（.dev/ flow 脚本，非产品代码；已过 `node --check`）。

**Mechanics validated**（throwaway git 仓）:
- detached worktree @ B0+ours → `git rebase B1` → 新 commit parent==B1、内容含 sibling+ours ✅
- `git cherry-pick --no-commit <B1+ours>` 到 main(@B1) → 干净、暂存 ours ✅
- 冲突路径：rebase 非零退出 → `git rebase --abort` → 回到 ours、worktree 干净（标准 git 语义）✅

**Notes**:
- 旧 `mainHead === baseline` 校验的注释提「绝不 reset --hard baseline」——但当前代码根本没 reset，
  只是 cherry-pick 到 mainHead。校验实际只剩「main 动了就拒绝」这一 gate-integrity 闸，且其逃生通道
  land-preserve 在 B0+ours 上重跑门却落到 B1，泄漏了它本要保护的不变量。本改动用 rebase+re-gate
  正确保住不变量，并发下也能自动落地。
- 成本：慢路径多跑一次完整门（test/clippy/fmt/e2e + mutant 门）。mutant 门按 diff 自跳
  （disjoint-files 并发下开销与首轮相同）；只在 main 真动时才付，符合「该付时才付」。
- 残留竞态：regate 与 commit.land 之间若 main 又被推进（第三个 flow 落地），cherry-pick rebased
  sha 到更新 main 可能落到未验组合。靠 cherry-pick 冲突检测兜文本冲突；语义冲突 slips
  （与任何并发 merge 同量级），不额外处理（避免无限 rebase 循环）。
- ⚠️ 提交待补：编辑完成时 IDE shell 不可用，`git add/commit` 未能执行。需在 shell 恢复后提交：
  `git add .dev/flows/self-improve.flow.js .dev/journal/manual-20260723-concurrent-landing-rebase-regate.md && git commit -m "fix(flow): 支持并发落地 rebase+regate (g329 事故续)"`
