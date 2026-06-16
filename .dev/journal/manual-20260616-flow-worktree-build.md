# Manual edit: flow-worktree-build

**Date**: 2026-06-16
**Goal**: 修复 self-improve.flow.js 两处架构缺陷：
  1. 每次 run 前未重新编译 recursive 二进制，导致 agent 可能用旧版本执行新 goal
  2. agent 直接在 main checkout 改动文件，而非在隔离 worktree 内

**Files touched**:
- `.dev/flows/self-improve.flow.js`

**Changes**:

### Fix 1: preflight.build
在 `preflight.baseline` 之后添加 `preflight.build` 步骤，执行 `cargo build --release`。
每次 run 都重新编译，确保 agent 使用的是当前最新代码对应的二进制。
该步骤由 Checkpoint 幂等管理，续跑时若已编译则跳过。

### Fix 2: worktree 隔离
引入 `gitWorktreeAdd` / `gitWorktreeRemove`（flowcast 已内置）实现 per-run 隔离：

- `preflight.worktree`：在 `.worktrees/<runId>` 创建隔离 worktree（detached HEAD from current baseline）
- agent 所有文件改动（`cwd: worktreeDir`）发生在 worktree 内，main checkout 始终干净
- 质量门（test/clippy/fmt/e2e）均在 worktreeDir 内执行，保证测试的是 agent 实际改动
- resume-fix 也在 worktreeDir 内续跑
- self-review diff 取自 worktreeDir
- 全绿后：在 worktree 创建临时提交 → `cherry-pick --no-commit` 到 main checkout → 以正式 message 提交
- 失败/跳过：worktree 直接丢弃，main 无需 reset（`withSelfModGuard` 的回滚逻辑对 main 是 no-op）
- 退出清理：注册 `exit/SIGINT/SIGTERM` 钩子确保 worktree 总被移除

**Tests added**: none（flow 为 JS 元工具，无 Rust 测试）

**Notes**:
- `.worktrees/` 已在 .gitignore 中，worktree 不会被意外提交
- `--bin` 选项仍可覆盖二进制路径（e.g. debug build 调试时）
- 续跑（resume）时若 worktree 已被清理，会自动重建（幂等检查 `existsSync`）
- cherry-pick 无冲突风险：worktree 从当前 HEAD 创建，main checkout 在 agent 执行期间不变
