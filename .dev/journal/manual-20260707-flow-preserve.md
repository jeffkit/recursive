# Manual edit: flow-preserve

**Date**: 2026-07-07
**Goal**: 把 self-improve flow 的「失败即硬回滚丢 worktree」改成「agentic 修复 loop + preserve 后盾」，避免 172 步成果因一次门禁/评审红灯被清空。
**Files touched**: `.dev/flows/self-improve.flow.js`
**Tests added**: none（flow 为 JS，无 cargo 测试覆盖；已 `node --check` + `--prune-preserve` 派发冒烟）

## 背景

g324 两次跑都 rolled-back，根因不同：
- **minimax 那次**：`run.recursive` 30 分钟超时被杀（`timedOut:true, transcriptMessages:0`），半成品过 `cargo test` 必红，1 轮 resume-fix 救不回 → 回滚丢 30 分钟成果。
- **deepseek-pro 那次**：agent 自然结束（354 msg，全部门禁绿），但 self-review 给的是**截断到 20k 字符的 diff**，reviewer 看不到 src/http/handlers.rs 等关键改动 → 假阴性 NEEDS_FIX → 回滚丢全绿成果。

两次都印证：硬回滚 + 1 轮 resume-fix + 截断 diff 太脆。

## 改动（A–F）

- **A. 门禁 resume-fix 改 N 轮循环**：`runQualityGates` 不再靠 flowcast `runGate` 内置 1 轮 resume-fix；改用 `onFail:'rollback'`（纯检查），flow 自己控循环——每轮喂**最新** stderr、链式 replay 上一轮 fix-transcript、再重跑。`MAX_FIX_ROUNDS` 默认 3（`--max-fix-rounds` 可配）。
- **B. 评审 NEEDS_FIX 走 N 轮循环**：旧代码一见 NEEDS_FIX 就回滚；改喂评审意见回 agent 修，N 轮，仍 NEEDS_FIX 才 preserve。评审修过代码后**重跑门禁**确认没回归（`reviewFixRan` 守卫）。
- **C. 修评审 diff 截断**：`selfReview` 把 `gitDiff(...).slice(0,20_000)` 换成「完整 diff 写 `.review-diff.patch` + `git diff --stat` 清单」，reviewer 用 Read 读完整 diff + 源文件交叉验证；评审完即删并撤销 intent-to-add，不污染提交。
- **D. 超时 30min→2h**：`RUN_TIMEOUT_MS` 默认 7_200_000（`--timeout` 可配），注入 `recursive()` 调用。超时也走 resume 路径，transcript 为空时降级 fresh run（靠 worktree on-disk 状态续修）。
- **E. failed-preserved 后盾**：循环耗尽不再回滚，`preserveScene` 保留 ① `refs/preserve/<run-id>` 分支 ② `preserved.diff` ③ `<tag>-failure.log`，worktree 挪到 `.worktrees/preserve/<run-id>/`。`main()` finally 对 `*-preserved` verdict 跳过 cleanupWt。panic 也升级为 preserve（旧实现只保 transcript）。
- **F. 消费命令**：`--resume-preserve <run-id>`（从 preserve 现场接着修，注入失败上下文 + 跑 fixer + 门禁 + 评审）、`--land-preserve <run-id>`（跑门后 cherry-pick 落地）、`--prune-preserve <run-id>`（清现场）。

## 新 verdict

`committed / failed-preserved / panic-preserved / skip-commit`（`rolled-back` 仅在 preserveScene 自身失败时退化出现）。`announce` 对 `*-preserved` 带 `--resume-preserve / --land-preserve / --prune-preserve` 接手指令。

## Notes

- `runAttemptWithGoal` 是 `runAttempt` 的 resume 变体（允许 goal 覆盖），为 `--resume-preserve` 服务；主路径仍走 `runAttempt`。
- 未改 Rust kernel / run_core，符合 invariant #1。
- flowcast `quality-gate.js` 未改（vendored），循环逻辑全在项目 flow 内。
- 下一步：用 `--provider deepseek --model deepseek-v4-flash --reviewer-provider minimax --hitl ilink` 重跑 g324 验证。
