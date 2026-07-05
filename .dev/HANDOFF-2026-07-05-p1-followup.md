# Hand-off: Recursive 架构审查后续（第二轮 P2/P3 + P1-1 准备）

**Date**: 2026-07-05
**From**: 接手 `HANDOFF-2026-07-05-arch-review.md` 的会话，落地 P2/P3 + P1-1 gate
**To**: 下一个会话
**Status**: P2 全部完成、P3 完成 2/4、P1-1 只落地了保护性 gate（实际拆分未做）
**前置**: 先读 `.dev/HANDOFF-2026-07-05-arch-review.md`（第一轮，P0 已落地）+ 本文档

---

## 这次会话做了什么

在第一轮 P0 修复（commit `81290b1`）之上推进了 6 项 + 1 份 journal：
单 commit `a9a7129` 落在分支 `fix/p1-arch-followup`（worktree
`.worktrees/p1-arch-followup`）。

| 优先级 | 项目 | 文件 |
|--------|------|------|
| P2-1 | README v0.5.0 示例 → v0.7 `AgentRuntime::builder()`，编译通过 | `README.md` |
| P2-2 | 两份 AGENTS.md 加交叉引用（不能改名，`src/config.rs` 硬读路径） | `AGENTS.md`, `.dev/AGENTS.md` |
| P2-3 | self-improve 3 条失效模式写进 CLAUDE.md | `CLAUDE.md` |
| P3-3 | RunShell 加 LLM 可调 `max_output_bytes`（2 MiB 硬 cap）+ 2 测试 | `src/tools/shell.rs` |
| P3-4 | `AgentKernel::with_tools` 改 `self.clone()` + 替换字段（避免漏字段） | `src/kernel.rs` |
| P1-1 准备 | 加 `run_inner_function_body_stays_small` invariant 测试（≤ 400 行） | `tests/invariants/loop_size_orthogonality.rs` |

**主动跳过**（理由在 journal 里）：
- P3-1（`effective_step_limit(0) → usize::MAX`）—— 显式契约且有测试守护，不是 bug
- P3-2（`multi::SharedMemory::set` SystemTime）—— 改 wire format 风险大、收益低
- P1-1 实际拆分 —— HIGH 风险 69 caller，单会话拆完不现实

**质量门**：`cargo test --workspace`（1071+ 测试）+ `cargo clippy --all-targets --all-features -- -D warnings` + `cargo fmt --all --check` 全绿。

---

## 当前环境状态

### Git 状态

```
main checkout (工作目录根):
  分支: main (HEAD: 9fbb0e4)
  未提交修改:
    AGENTS.md    — GitNexus MCP 自动写回 (symbol count 9250 → 10081)
    CLAUDE.md    — 同上
  → 这些是 GitNexus 自动元数据，不是手改，可独立 commit 或丢弃。

worktree 分支（已提交、未推送）:
  .worktrees/p0-arch-fixes              [fix/p0-arch-fixes]        ← 第一轮成果，可推
  .worktrees/p1-arch-followup           [fix/p1-arch-followup]     ← 本轮成果，可推 ⭐
  .worktrees/feat-providers-remote-catalog [feat/providers-remote-catalog] 别的会话
  .worktrees/g323-salvage               [feat/g323-tui-loop]       别的会话
  .worktrees/tui-mutant-debt            [tui-mutant-debt]          别的会话
  .worktrees/tui-mutant-debt-rest       [tui-mutant-debt-rest]     别的会话
```

### GitNexus 索引

- 当前 index 停在 `9fbb0e4`（main HEAD）。
- 本轮 commit `a9a7129` 在 worktree 里，GitNexus 看不到。
- 跑 `npx gitnexus analyze` 可刷新到当前 HEAD；不刷也行，影响分析精度但不影响代码。

### 推送前要做的

1. **没有 PR**。下个会话发 PR 的话：
   ```bash
   cd .worktrees/p1-arch-followup
   git push -u origin fix/p1-arch-followup
   gh pr create --base main ...
   ```
2. **两轮成果发 1 个 PR 还是 2 个**：建议**两个独立 PR**（`fix/p0-arch-fixes` 和 `fix/p1-arch-followup`），因为：
   - P0 是真 bug，单独发便于回滚
   - P2/P3/P1-1-prep 是改善性 + 测试，独立 merge 风险隔离
   - 如果想合一个，先把 `fix/p1-arch-followup` rebase 到 `fix/p0-arch-fixes` 上（其实它已经基于 `81290b1` 了，等于 stack 关系）
3. **rebase 风险**：memory 里 `feedback_self_improve_rebase_recovery` 提示"用户 PR landing during run create phantom deletions"。如果 main 在 9fbb0e4 之后有新提交，PR 合并前 `git rebase main`。
4. **不要碰** `.worktrees/` 下别的 worktree（别的 session 在用）。

---

## 接下来要做的事（按优先级 + 风险）

### 🟠 P1-1（重头戏）: 拆 `run_inner` 成 phase helpers 或状态机

**位置**: `src/run_core.rs:499-893`，单函数 ~394 行

**已就绪的 gate**: 本轮新加的 `loop_size_orthogonality::
run_inner_function_body_stays_small` 测试已经把它锁在 ≤ 400 行。
**这是落地真实拆分的前置条件——已经满足。**

**建议方案（我的 lean）**: **sibling helpers**，不要一上来就状态机。理由：
- 拆成 `dispatch_tool_batch`, `handle_completion`, `check_budget`,
  `trim_transcript`, `maybe_compact` 等命名良好的 helper
- 循环体只剩 `loop { drain_mailbox(); check_budget();
  trim_transcript(); let completion = call_llm().await?;
  if completion.no_tools { break } dispatch_tool_batch().await?; }`
- diff 小、易 review，每一步都能独立测试
- 状态机方案虽然"更架构师"，但循环体改成 `loop { match state }` 反而可能让 line count 不降反升

**风险**: HIGH。69 个间接 caller（`App::submit_prompt` 等）。

**推进步骤**:

1. **`gitnexus_impact RunCore upstream`** 看全图（**先做这步**）
2. **新开 worktree**（铁律：绝不在 main checkout 改代码）：
   ```bash
   git worktree add .worktrees/p1-run-inner-split -b refactor/run-inner-split a9a7129
   ```
3. **分阶段 commit**（每阶段独立 commit，跑完整质量门）：
   - Stage 1: 抽 `drain_mailbox`（仅读 mailbox，不改 transcript）
   - Stage 2: 抽 `check_budget`（shutdown + step limit + transcript budget）
   - Stage 3: 抽 `trim_transcript` + `maybe_compact`
   - Stage 4: 抽 `call_llm` + inline reasoning 提取
   - Stage 5: 抽 `dispatch_tool_batch`（最大的一个，含 plan-mode gate
     + permission hook + pre/post hooks + 并行批 + 序列化回退）
   - Stage 6: 抽 `handle_completion`（5 种 finish 分支）
4. **每阶段后**跑：
   ```bash
   PATH="$HOME/.cargo/bin:$PATH" cargo test --workspace
   PATH="$HOME/.cargo/bin:$PATH" cargo clippy --all-targets --all-features -- -D warnings
   PATH="$HOME/.cargo/bin:$PATH" cargo fmt --all --check
   PATH="$HOME/.cargo/bin:$PATH" cargo test --test invariants run_inner
   ```
   invariant 测试应该全程绿（每阶段都在减 body 行数）。
5. **拆完后**把 `run_inner_function_body_stays_small` 的 400 阈值降到新 baseline + ε（比如 150）。

### 🟠 P1-2: `AgentRuntime` 字段重构 + 锁层次文档

**位置**: `src/runtime.rs:138-182`（11 字段，7 种同步原语混用）

**建议**:
1. 把会话级状态合并进 `Session` 对象
2. **新建 `docs/INTERNALS.md`** 写锁层次：HTTP 顶层 → session → runtime → kernel
3. 拆 `CheckpointState` 回独立字段

**风险**: MEDIUM（公共 API 不变，内部重构）

**注意**: P1-1 拆完后做这个会更顺（run_inner 已经瘦身）。

### 🟠 P1-3: 决定 kernel-vs-platform 分层

**问题**: feature flag 太多但没"内核 crate"。`recursive-agent` 还是混着所有东西。

**建议**: 开 `recursive-kernel` crate（kernel.rs + run_core.rs + agent/types.rs + message.rs + error.rs + llm/ + tools/），把 http/mcp/coordinator/multi/team/tasks/session/memory/storage/weixin 抽到 `recursive-platform`。

**风险**: HIGH（破坏 v0.7 公共 API）。CRAN 风格"两个 crate + re-export"可保 `use recursive::*` 不变。

**注意**: 这个建议**值得先开 issue 讨论再动**——影响 publish、影响下游 import 路径、影响 self-improve flow。

### 🟡 小尾巴（可塞 cleanup commit）

- **P3-1（重新评估后）**: 如果决定要给 `max_steps=0` 加生产硬 cap，做 `RECURSIVE_HARD_STEP_CAP` env var，**不要**用隐藏常量。
- **P3-2**: `multi::SharedMemory` 加 `seq: u64` 字段做内部排序键，保留 `timestamp` 作 wall-clock 展示。要更新 wire format 文档。
- TUI 模块如果被本轮 commit 影响（实际没有，但提醒一下）：跑 `.dev/scripts/tui-test-presence.sh` + `.dev/scripts/tui-mutants.sh`。

---

## 强制操作清单（接手第一件事）

1. **读 `CLAUDE.md` 整篇** —— worktree 铁律、质量门、E2E 规则、**新加的"Known self-improve failure modes"段**。
2. **读 `.dev/AGENTS.md` 整篇** —— 8 条 Invariant（Invariant #1 现在指向 `run_core.rs::RunCore::run_inner`）。
3. **读本轮 commit `a9a7129` 的 commit message**：
   ```bash
   git show a9a7129
   ```
4. **读第一轮 commit `81290b1`**（P0-A/B/C 真 bug 修复，理解 base）。
5. **读 journal** `.dev/journal/manual-20260705-p1-arch-followup.md`（本轮决策理由，含"为什么 P3-1/P3-2 跳过"）。
6. **读 memory 反馈**（`~/.claude/projects/-Users-kongjie-projects-Recursive/memory/`）—— worktree / self-improve / cargo PATH 三条铁律尤其重要。
7. **不要直接在 main checkout 改代码** —— 按 worktree 铁律，新功能/重构必须开新 worktree。
8. **跑 impact 分析**: `gitnexus_impact RunCore upstream` 在改 `run_inner` 前必做。

## 不要做的事

- 不要碰 `.worktrees/feat-providers-remote-catalog`、`.worktrees/g323-salvage`、`.worktrees/tui-mutant-debt*` —— 别的会话在用。
- 不要在 `main` checkout 直接改产品代码（main 是 stable 共享 admin tree）。
- 不要 reset/amend 已有 commit（新 commit 优先）。
- 不要修 `Cargo.toml` 加新依赖（AGENTS.md 第 6 条；需要 journal 写理由）。
- 不要跳过 cargo 质量门（`-D warnings` 让 clippy warning 也是 fail；self-improve flow 会触发 rollback）。
- 不要改 `Cargo.toml` 的 `package.name`（`recursive-agent`）或在 crates.io 改名 —— publish 流程依赖这个。
- 不要把 `AGENTS.md` 改名 —— `src/config.rs::load_project_context` 硬读这个路径。

## 提醒

- `cargo` 不在默认 PATH —— 所有命令前缀 `PATH="$HOME/.cargo/bin:$PATH"`（memory `feedback_cargo_path`）。
- 用中文回答（user global rule）。
- 修改代码后查 `docs/DOC_CODE_MAP.md` 是否需要同步文档（user global rule）。
- 修改前一定先跑 `gitnexus_impact`（项目 CLAUDE.md GitNexus 段强制要求）。
- High/Critical 风险要先报告用户、得到确认再改（项目 CLAUDE.md 强制要求）。

---

## 本会话工作产物索引

| 路径 | 内容 |
|------|------|
| `.worktrees/p1-arch-followup/` (HEAD: `a9a7129`) | 本轮所有 commit |
| `.dev/journal/manual-20260705-p1-arch-followup.md` | journal 手记（含决策理由、跳过理由、下个会话指引） |
| `.dev/HANDOFF-2026-07-05-arch-review.md` | 第一轮 handoff（P0 已落地） |
| `.dev/HANDOFF-2026-07-05-p1-followup.md` | **本文件** |

完整架构审查报告（含 P0-P3 全部分级、5 条架构师建议、不变量违反的具体证据）只存在于第一轮对话历史里，**没有落盘**。如果下个会话需要原报告作为输入，让用户从对话历史里复制粘贴，或者下个会话自己重新跑一遍 `gitnexus_query` + 读关键文件再写一份。

---

## TL;DR 给下个会话

> 第一轮 P0（commit `81290b1`）+ 本轮 P2/P3/gate（commit `a9a7129`）都已在 worktree 落地。
> 接下来三件事按优先级：
> 1. **推 P0 + 本轮两个 PR**（独立 push、独立 PR）
> 2. **拆 `run_inner`** —— gate 已就绪，开新 worktree、`gitnexus_impact` 先跑、分 6 阶段每阶段独立 commit
> 3. **P1-2 锁层次文档** + **P1-3 kernel/platform 拆 crate**（后者先开 issue 讨论）
>
> 不要在 main checkout 改代码。所有改动前跑 `gitnexus_impact`。所有 cargo 命令前缀 `PATH="$HOME/.cargo/bin:$PATH"`。
