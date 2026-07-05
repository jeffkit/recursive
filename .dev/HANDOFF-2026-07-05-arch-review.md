# Hand-off: Recursive 架构审查后续工作

**Date**: 2026-07-05
**From**: 资深架构师视角的深度审查 + 第一轮 P0 修复
**To**: 下一个会话
**Status**: P0 已完成，P1/P2 等待接手

---

## 这次会话做了什么

### 1. 架构深度审查（输出在对话里，未落盘）

下一个会话可以从对话历史里看到完整报告。简版摘要：

- **核心矛盾**：Recursive 自我定位"tiny minimal kernel"，但实际是 47K+ 行产品代码 + 6 个 crate + HTTP/MCP/TUI/multi-agent/cloud 全平台。文档与代码现实严重脱节。
- **P0 真 bug**：`RunShell` 超时孤儿进程、`INSECURE_OK` 反向逻辑陷阱。
- **P1 架构债**：`run_inner` 单函数 400 行成新"主循环"、`AgentRuntime` 同步原语混用、模块边界糊掉（4 套并存的多 agent 协作机制）。
- **P2 文档债**：README 示例不可编译、AGENTS.md layout 与现实脱节、self-improve 流的乐观假设（memory 里有 3 条已知不可靠反馈）。

### 2. P0 修复（已提交）

在 worktree `.worktrees/p0-arch-fixes` 上叠加了 2 个 commit（前一个 commit `a49b696` 是上一轮的，本轮是 `81290b1`）：

**commit `81290b1` — `fix(arch): land P0-A/B/C from the architecture review`**

| 文件 | 改了什么 | 为什么 |
|------|---------|--------|
| `src/tools/shell.rs` | `Command::kill_on_drop(true)` + timeout 分支显式 `start_kill` | 修 P0-A：超时返回 Err 时 child 没被杀 |
| `src/http/auth.rs` | INSECURE_OK 旁路 gate 在 `cfg!(debug_assertions)`；release 构建忽略并打 error log | 修 P0-B：release 镜像不能被 env var 绕过 auth |
| `.dev/AGENTS.md` | Layout 同步到 G219 后真实模块拓扑；Invariant #1 改指向 `run_core.rs::RunCore::run_inner` | P0-C：layout 还停在 v0.3 |
| `Cargo.toml` | description 从"minimal, orthogonal"改成"coding-agent platform" | P0-C：crates.io 用户被骗了三个版本 |
| `README.md` | hero 段重写，列出所有 feature 面 + 嵌入式用户的 `--no-default-features` 逃生口 | P0-C |
| `.dev/journal/manual-20260705-p0-arch-real-bugs.md` | journal 手记 | 项目约定（CLAUDE.md 要求每次改写一份） |

**新增测试**：
- `tools::shell::tests::timeout_kills_child_process` — 用 `exec sleep 30` + PID marker file + `kill -0` 轮询，确保超时后子进程真死。pre-fix 这个测试会 hang 30 秒。
- `http::auth::tests::insecure_ok_bypass_is_gated_on_debug_assertions` — source-grep 测试，锁死 `cfg!(debug_assertions)` gate。runtime 测试观察不到 cfg! 行为，grep 是唯一可行的 invariant pinning。

**质量门**：`cargo test --workspace` + `cargo clippy --all-targets --all-features -- -D warnings` + `cargo fmt --all --check` 全绿。

---

## 当前环境状态

### Git 状态

```
main checkout (工作目录根):
  分支: main (HEAD: 9fbb0e4)
  未提交修改:
    AGENTS.md    — GitNexus symbol count 自动更新 (9250 → 10081)
    CLAUDE.md    — 同上
  这些是 GitNexus MCP 自动写回的元数据更新，不是手改，可独立 commit 或丢弃。

worktree 分支（已提交、未推送）:
  .worktrees/p0-arch-fixes              [fix/p0-arch-fixes] 本轮成果 ← 推这个
  .worktrees/feat-providers-remote-catalog  [feat/providers-remote-catalog] 别的会话
  .worktrees/g323-salvage               [feat/g323-tui-loop] 别的会话
  .worktrees/tui-mutant-debt            [tui-mutant-debt] 别的会话
  .worktrees/tui-mutant-debt-rest       [tui-mutant-debt-rest] 别的会话
```

### 推送前要做的

1. **没有 PR**。如果下个会话要发 PR，从 `fix/p0-arch-fixes` 推 `origin/fix/p0-arch-fixes` 然后 `gh pr create`。
2. **rebase 风险**：memory 里的 `feedback_self_improve_rebase_recovery` 提到"用户 PR landing during run create phantom deletions"。如果 main 在 9fbb0e4 之后有新提交，PR 合并前先 `git rebase main`。
3. **不要碰** `.worktrees/` 下其他 worktree（别的 session 在用）。

### GitNexus 索引

- 当前 index 停在 `9fbb0e4`（main HEAD）。
- 我的 commit `81290b1` 在 worktree 里，GitNexus 看不到。
- 跑 `npx gitnexus analyze` 可以刷新到当前 HEAD；不刷也行，影响分析精度但不影响代码。

---

## 接下来要做的事（按优先级）

### 🟠 P1-1: 把 `run_inner` 拆成状态机

**问题**：`src/run_core.rs:499-893`，单个函数 ~400 行。里面塞了 shutdown 检查、mailbox 排空、transcript 预算、压缩、LLM 调用、流式 forwarder、inline reasoning 提取、tool 执行（带 plan-mode gate + permission hook + pre/post hooks + 并行批 + 序列化回退）、DENIAL_LIMIT 哨兵前处理、stuck detection 滑窗、skill injector、5 种 finish 分支。

**为什么是 P1**：AGENTS.md Invariant #1 已经从"agent.rs::Agent::run"改成"run_core.rs::RunCore::run_inner"，但只是把分支搬了家。继续加 feature 一定会在 400 行里产生交叉 bug。

**建议方案**：拆成显式状态机
```
StepStart → DrainMailbox → CheckBudget → TrimTranscript
  → MaybeCompact → CallLLM → HandleCompletion
  → { NoTools → Done | HasTools → DispatchTools → StuckCheck → StepStart }
```
每个状态独立函数，循环体只有 `loop { match state { ... } }` 不到 30 行。

**风险**：HIGH。下游影响 `App::submit_prompt` 等多条 TUI 调用链（impact 报告 69 个间接 caller）。要先：
- `gitnexus_impact RunCore upstream` 看全图
- 在 `tests/invariants/loop_size_orthogonality.rs` 加新测试锁死"循环体 ≤ N 行"
- 拆分一次落一阶段，分多个 commit

### 🟠 P1-2: `AgentRuntime` 字段重构 + 锁层次文档

**问题**：`AgentRuntime`（`src/runtime.rs:138-182`）11 个字段，混用 `Arc<Mutex>`、`Arc<RwLock>`、`Arc<AtomicUsize>`、`tokio::sync::Mutex`、`tokio::sync::RwLock`、`std::sync::Mutex`、`std::sync::RwLock` 7 种同步原语。`CheckpointState` 把 6 个 `Option<Arc<...>>` 打包只是为减少字段数。HTTP 层 `SessionState` 又再叠一层。**没有任何文档说明锁层次、获取顺序、能否重入**。

**建议**：
1. 把"会话级运行时状态"统一进一个 `Session` 对象
2. 在 `docs/INTERNALS.md`（新建）写下锁层次：HTTP 顶层 → session → runtime → kernel
3. 把 `CheckpointState` 拆回独立字段，去掉"为减字段数硬凑"的痕迹

**风险**：MEDIUM。运行时内部重构，公共 API 不变。

### 🟠 P1-3: 决定 kernel-vs-platform 分层

**问题**：feature flag 太多但没"内核 crate"。`recursive-cli`、`recursive-tui` 已经独立，但 `recursive-agent` 还是混着所有东西。

**建议**：开 `recursive-kernel` crate 只含 `kernel.rs + run_core.rs + agent/types.rs + message.rs + error.rs + llm/ + tools/`（基础），把 `http/`、`mcp.rs`、`mcp_server.rs`、`coordinator.rs`、`multi.rs`、`team.rs`、`tasks.rs`、`session/`、`memory/`、`storage/`、`weixin/` 等抽到 `recursive-platform` crate。

**风险**：HIGH。破坏 v0.7 公共 API。但**越拖成本越高**——CRAN 风格"两个 crate + re-export"可保 `use recursive::*` 不变。

### 🟡 P2-1: README 示例代码不可编译

**问题**：`README.md` 第 30 行起：
```rust
let mut agent = Agent::builder()
    .llm(llm)
    .tools(tools)
    ...
```
但 `Agent` / `AgentBuilder` 在 Goal 219 已删除。`Cargo.toml` 是 0.7.0，README hero 还在 v0.5.0。

**修法**：换成 `AgentRuntime::builder()`。前一个 commit `a49b696` 已经动过 README hero，但示例代码段没修。

### 🟡 P2-2: 两份 AGENTS.md 混淆

**问题**：
- `/AGENTS.md`（项目根，给 Recursive 自己读的精简版）
- `.dev/AGENTS.md`（开发用，完整不变量列表）

下个 agent 不知道看哪份。建议：根目录的改名为 `AGENTS.brief.md` 或直接删（指向 `.dev/AGENTS.md`），让 `.dev/AGENTS.md` 成为唯一来源。

### 🟡 P2-3: self-improve 流已知不可靠但文档乐观

**问题**：memory 里有 3 条反馈：
- `feedback_self_improve_rollback_may_not_reset` — agent 不可恢复崩溃后 worktree 留脏
- `feedback_self_improve_rebase_recovery` — 跨用户 PR 产生 phantom deletion
- `feedback_self_improve_worktree` — 必须用 `parallel-self-improve.sh`

但 CLAUDE.md / AGENTS.md 仍把 self-improve 流描述成可靠 pipeline，`self-improve.sh` 标记 deprecated 但仍在场。

**建议**：
1. CLAUDE.md 加一段"Self-improve 已知失效模式"，把 memory 里的 3 条直接列进去
2. 把 `parallel-self-improve.sh` 的前置检查（git status / rebase / 清理 `.worktrees/`）写成强制 gate 而不是建议
3. 或者承认 self-improve 是实验性工具而不是开发主流程

### 🟢 P3 小问题（建议合并成一次 cleanup commit）

- `src/run_core.rs:898` `effective_step_limit` 当 `max_steps=0` 返回 `usize::MAX`，无 transcript limit 时理论上可跑无穷。生产硬 cap 1000。
- `src/multi.rs:37` `SharedMemory::set` 用 `SystemTime::now()`，时钟跳变会让 timestamp 错乱。改 monotonic clock + epoch（参考 `src/http/mod.rs:106` 已经为 session 用了 `OnceLock<Instant>`）。
- `RunShell` 默认 `max_output_bytes = 128KB` 对 `cargo build` 远不够，错误诊断信息被截。建议允许 LLM 在 args 里指定更大限额。
- `kernel.rs:253-268` `Self::with_tools` 全字段手 clone，加字段时容易漏（git log 里已经漏过一次）。改成 `..self` 解构或派生 `Clone` 后修改。

---

## 强制操作清单（接手第一件事）

1. **读 `CLAUDE.md` 整篇**——项目规则（worktree 铁律、质量门、E2E 规则）。
2. **读 `.dev/AGENTS.md` 整篇**——8 条 Invariant 不变（我已经更新了 Invariant #1 和 layout 段）。
3. **读本轮 commit `81290b1` 的 commit message**——`git show 81290b1` 看 P0-A/B/C 改了什么、为什么。
4. **读 memory 里的反馈条目**（`~/.claude/projects/-Users-kongjie-projects-Recursive/memory/`）——尤其是 worktree / self-improve / cargo PATH 三条铁律。
5. **不要直接在 main checkout 改代码**——按 worktree 铁律，新功能/重构必须开新 worktree。

## 不要做的事

- 不要碰 `.worktrees/feat-providers-remote-catalog`、`.worktrees/g323-salvage`、`.worktrees/tui-mutant-debt*`——别的会话在用。
- 不要在 `main` checkout 直接改产品代码（main 是 stable 共享 admin tree）。
- 不要 reset/amend 已有 commit（新 commit 优先）。
- 不要修 `Cargo.toml` 加新依赖（AGENTS.md 第 6 条；需要 journal 写理由）。
- 不要跳过 cargo 质量门（`-D warnings` 让 clippy warning 也是 fail；self-improve flow 会触发 rollback）。

## 提醒

- `cargo` 不在默认 PATH——所有命令前缀 `PATH="$HOME/.cargo/bin:$PATH"`（memory `feedback_cargo_path`）。
- 用中文回答（user global rule）。
- 修改代码后查 `docs/DOC_CODE_MAP.md` 是否需要同步文档（user global rule）。

---

## 本会话工作产物索引

| 路径 | 内容 |
|------|------|
| `.worktrees/p0-arch-fixes/` (HEAD: `81290b1`) | 本轮所有 commit |
| `.dev/journal/manual-20260705-p0-arch-real-bugs.md` | journal 手记 |
| `.dev/HANDOFF-2026-07-05-arch-review.md` | **本文件** |

完整审查报告（含 P0-P3 全部分级、5 条架构师建议、不变量违反的具体证据）只存在于对话历史里，**没有落盘**。如果下个会话需要原报告作为输入，让用户从对话历史里复制粘贴，或者下个会话自己重新跑一遍 `gitnexus_query` + 读关键文件再写一份。
