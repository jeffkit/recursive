# Manual edit: selfimprove-flow-review

**Date**: 2026-07-02
**Goal**: 修复 `.dev/flows/self-improve.flow.js` review 中发现的全部潜在问题（按优先级高→低）
**Files touched**:
- `.dev/flows/self-improve.flow.js`
- `.dev/flows/test/e2e.test.js`

## 修复清单

**高优先级**
1. **budget-resume 后 resume-fix transcript 路径丢失**：引入 `latestTranscript` 跟踪最近成功
   的 transcript；发生过 budget resume 后，质量门的 resume-fix 从 `resumedTranscript` replay，
   不再丢失 resume 阶段的全部 tool call（旧代码会让 agent 在「忘了刚做啥」的状态下修 bug）。
2. **cherry-pick 冲突时 guard 破坏性 reset main**：移除 `withSelfModGuard` 包裹（worktree 即沙箱，
   guard 的 `reset --hard baseline` 在 worktree 隔离模式下既冗余又有破坏性）。改为 cherry-pick 前
   显式校验 `main HEAD == baseline`，被推进则拒绝落地；冲突时 `git cherry-pick --abort` 保持 repo
   干净，**绝不 reset --hard 吃掉别人的提交**。顺带修好了 4 个预存红测试（guard 在 `.worktrees/`
   仍注册时抛「回滚后工作树仍脏」）。
3. **`killStaleRecursiveProcs` 误杀并发 run**：重写识别逻辑——从 recursive argv 的
   `--transcript-out …/runs/<runId>/…` 提取 runId，用 `pgrep -af self-improve.flow.js` 收集活跃
   flow 的 `--run-id` 集合；带 runId 但对应 flow 已死 = 孤儿才杀，活跃 run 的子进程一律跳过。
4. **reviewer 无 VERDICT 误判 NEEDS_FIX 回滚**：`reviewWithRetry` 对「ok 但无 verdict」改为重试，
   仍无 verdict 才归 `UNAVAILABLE`（不丢弃成果），把「reviewer 没遵循格式」与「代码真有问题」分开。
5. **announce committed 过期合并文案**：worktree `--detach` + cherry-pick 直接到 main 当前分支，
   根本没有独立 branch 可 merge；通知改为「已直接落在 main checkout 当前分支，无需 merge」。

**中优先级**

6. **commit-pending 补完整门 + 失败兜底**：抽 `commitPending()`，跑 builtin + `.flowcast/gates.json`
   全部门链（覆盖 e2e/tui 硬门，不止 cargo 三件套）；门红时通知 + 写 failure context + exitCode=1，
   改动保留待人处理。
7. **budget-resume 后检测 panic**：resume 自己 panic（exit 101/128+N）现在走 `panic-preserved`，
   不再漏进质量门（旧代码只查 `budgetExceeded`）。
8. **failure context tailLog 取 resumedTranscript**：budget-after-resume 的诊断信息不再用原始 transcript。
9. **reviewer-provider 未配置醒目区分**：`selfReview` 在无 `RECURSIVE_API_BASE/API_KEY` 时返回
   `misconfig` 标记，`reviewWithRetry` 透传；通知文案区分「未配置（建议加 --reviewer-provider 或
   --no-review）」与「网络 down」，避免 review 层悄无声息缺席。
10. **pingProvider 401/403 fail-fast**：bad key 在 preflight 就抛错，不再等 agent 跑几分钟后挂掉。

**低优先级 / 清理**

11. 信号 handler 用具名引用统一注册/注销，修重复清理 + 闭包泄漏。
12. `buildEnv` 去掉 `RECURSIVE_MAX_STEPS` 双重设置，单一来源（`recursiveProviderEnv`）。
13. `selfReview` 的 recursive 调用加 `transcriptOut`（审计可见）+ `--reviewer-max-steps` 上限。
14. `latestJournal` 按 mtime 排序（字典序在 manual-/gNN- 混排时不等于时间序）。
15. `goalSubject` 优先取 markdown 一级标题文本。
16. `gitDiff` 的 `--intent-to-add` 只对 status 列出的未跟踪文件做，不全目录扫描。
17. `ensureGitExclude` 同时排除 `.worktrees/`，避免 worktree 目录污染 main 的 clean 预检。
18. `computeMetrics` 用显式 `baseline..HEAD` 范围。
19. （由 #2 一并完成）guard 在 worktree 模式下的冗余 rollback 已移除。

## Tests added

- `E2E commit-pending 成功路径：完整门全绿 → 补提交`
- `E2E commit-pending 失败路径：门红灯 → 不提交 + exitCode 1 + 改动保留`
- `E2E reviewer 未配置 → UNAVAILABLE（不回滚）→ committed`
- `E2E reviewer 正常返回无 VERDICT → UNAVAILABLE（不回滚）→ committed`

## Notes

- 这是 `.dev/` 开发元工具，非产品源码；质量门用 `node --test .dev/flows/test/e2e.test.js`
  （9/9 绿，原 5 个里 4 个预存红测试随 #2 一并修好）。
- worktree 模式下不再用 `withSelfModGuard`；`captureBaseline` 仍用于 preflight 与 metrics/通知。
- `--reviewer-max-steps` 为新增 CLI 选项（默认不传 → 沿用 recursive 自身默认）。
- 未改 Rust kernel 一行；recursive 二进制仅作为被调度的执行器。
