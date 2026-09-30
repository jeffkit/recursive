# Goal 410 — session 录制的 windows 路径兼容修复 + 解除 cfg(unix) 恢复 windows 覆盖

**Roadmap**: 2026-09-30 CI 剥层实证（6589279 / f705760 连环 windows 失败的同因收敛点）。
原登记于大仓 `docs/COORD-issue31-40.md` §7 四追（该文档已随双会话协同完结删除，本文件接替）。

**依赖**: 无前置。完成后可将 `ci.yml` 中 windows leg 的 `continue-on-error` 摘除恢复阻塞。

**Design principle check**:
- ✅ Does 修产品 bug：loop/resume 模式往 workspaces 写 session 录制文件，路径构造须用
  `PathBuf::join` / `std::path` 组件，不得手工拼字符串分隔符。
- ❌ Does NOT 改变 unix 上的录制布局与路径形状（mac/linux 行为零变化）。
- ❌ Does NOT 为绕过 bug 而在 windows 上关闭录制（修因，不修症状）。
- ❌ Does NOT 动 turn_mutants / cli_resume_surfaces 的测试语义——只解除文件级
  `#![cfg(unix)]`，用例本体不变。

## Why（2026-09-30 CI 实证）

windows runner 上 8 个 turn_mutants 用例同因失败：
`loop failed: session: recording to C:\Users\...`——loop 模式起会话时写 workspaces
录制路径直接报错，**疑为 session 录制路径构造的 windows 兼容产品 bug**（在真实
windows 终端使用 loop/resume 同样会踩）。cli_resume_surfaces 的 stub resume 在
windows 非零退出（`--session-out` legacy 警告路径）属同一覆盖缺口。两文件已临时
`#![cfg(unix)]`（6589279 / f705760），windows 确定性跳过。

**同期背景**：windows leg 已降级非阻塞（ci.yml `continue-on-error`，2026-09-30），
本 goal 落地且 windows 全绿后摘除该行，恢复三平台阻塞语义。

## Scope（do exactly this, no more）

### 1. 定位与修复 session 录制路径构造
- 复现路径：windows 下 `recursive loop`（或任何触发 session recording 的路径），
  观察录制文件路径的生成代码；找到硬编码 `/` 或字符串拼接点。
- 修复：全部改走 `std::path` 组件 API；创建父目录处用 `create_dir_all`（windows
  上父目录缺失也可能是失败根因之一，一并核实）。

### 2. 解除测试封印
- `crates/recursive-cli/tests/turn_mutants.rs`：删 `#![cfg(unix)]` 及理由注释。
- `crates/recursive-cli/tests/cli_resume_surfaces.rs`：删 `#![cfg(unix)]` 及理由注释。
- 若 stub resume 的 windows 失败是独立原因（非录制路径），在 goal journal 里记录
  实证后另行定性，不得混在本次修复里硬凑绿。

## 验收

1. windows runner：turn_mutants 8 用例 + cli_resume_surfaces 全部执行（非 skip）且绿。
2. mac/linux 本地：两文件全绿，无回归；session 录制文件布局不变。
3. `ci.yml` 摘除 windows `continue-on-error`，三平台矩阵回到阻塞语义，main 全绿。

---

## 附：#40 blocker 1 库级残留（防遗失登记，非本 goal 范围）

Goal 407 已把产品面 `(None, None)`（无 wall timeout + 无取消令牌）的无限 park 消灭为
不可达（REPL per-turn 令牌 + weixin 静态令牌）。**库级**（`--no-default-features`
裸用内核）`(None, None)` 仍无界 park——是否给库默认兜底超时属产品决策，不阻塞任何
现网路径。原 A 侧评审旗标，登记于此防止 COORD 删除后遗失。
