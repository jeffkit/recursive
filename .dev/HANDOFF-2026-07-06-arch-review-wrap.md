# Hand-off: Recursive 架构审查收尾

**Date**: 2026-07-06
**From**: 接手 `HANDOFF-2026-07-06-p1-3-and-big-refactor.md` 的会话
**To**: 下一个会话
**Status**: 架构审查正式收尾。P3-1 + P3-2 落地、推到 origin。**P1-3（crate 拆分）和 Session companion 拆分都被推迟**，理由记录在本文档——下个会话**不要**被旧 handoff 的描述牵着走重新评估这两项。
**前置**: 先读 `CLAUDE.md` + `.dev/AGENTS.md`（铁律）+ `docs/INTERNALS.md`（P1-2 落地的调用链 + 锁层次文档，仍然准确）。

---

## 这次会话做了什么

### Round 1: P3 cleanup 落地（commit `70b96ac`，已 push 到 origin）

`HANDOFF-2026-07-06-p1-3-and-big-refactor.md` 列的两个"小尾巴"全部清掉：

- **P3-1** — `src/run_core.rs::effective_step_limit` 加 `RECURSIVE_HARD_STEP_CAP` env var。默认 unset / `0` / unparseable 时行为完全不变（`max_steps=0` 仍返回 `usize::MAX`），仅当 env var 设为正整数时 clamp。`recursive loop` 长 session 行为保护住。
- **P3-2** — `src/multi.rs::MemoryEntry` 加 `#[serde(default)] pub seq: u64`，`SharedMemory` 内部 `Arc<AtomicU64>` 从 1 起单调递增（`0` 留给旧数据反序列化）。同 key 覆盖也推进 `seq`，解决原 review 抱怨的"wall-clock timestamp 同秒碰撞丢顺序"。
- 7 个新测试守护默认行为不变 + clamp 路径 + 向后兼容 + 单调性。
- `gitnexus_impact` 评级 HIGH（实际行为兼容、已报告），所有质量门绿。

### Round 2: 价值讨论 → 推迟 P1-3 和 Session 拆分

**关键判断**：handoff `HANDOFF-2026-07-06-p1-3-and-big-refactor.md` 推荐的两件 HIGH 风险"大改"，**经价值评估后都不该做**。具体理由见下文两节。

### Round 3: Push + 收尾

- 本地 `main` push 到 `origin/main`（`fff6807..387d534`）—— 本会话产物全部到远端。
- 本 handoff 文档覆盖旧 handoff。

---

## P1-3（kernel/platform crate 拆分）—— 推迟，理由记录

### handoff 的提议

把 `src/` 拆成 `recursive-kernel`（kernel/run_core/agent/message/error/llm/tools）+ `recursive-platform`（http/mcp/coordinator/multi/session/storage/weixin/memory），CRAN 风格 re-export 保 `use recursive::*`。

### 为什么推迟

handoff 列的拆分理由（"kernel 层应当稳定、轻量；platform 层是变体"）是审美 + 防御性论证，**没有症状驱动**。逐项审常见拆分收益：

| 收益 | Recursive 当前能否拿到 |
|---|---|
| 独立编译提速 | ❌ kernel 还在快速迭代，独立编译收益≈0 |
| 公共 API 边界清晰 | ❌ 外部用户极少，没有第三方 platform 实现 |
| 强制分层纪律 | ⚠️ 已有 `AGENTS.md` Invariant #2 + GitNexus 图约束，不是实际问题 |
| 复用性 | ❌ kernel 离开 Recursive 自家 tools/providers/message 体系无独立价值 |
| publish 灵活 | ❌ Recursive 是单产品，两版本号同步反而更摩擦 |

**长期代价**（handoff 没列的）：
1. 跨 crate 重构成本永久上升——Recursive 还在 P 系列迭代中段
2. `docs/INTERNALS.md`（P1-2 刚落地的 220 行文档）所有路径假设失效，要重写
3. GitNexus 索引重建（symbol UID 全变），impact graph 断档
4. Session 拆分（如果有价值）会被 crate 拆分卡住

### 替代方案（如果未来真要做）

| 替代 | 成本 | 价值 |
|---|---|---|
| (α) 把 `src/` 子模块边界收紧（`pub(crate)` 替代 `pub`） | 1 会话 | 边界纪律 80%，无外部成本 |
| (β) 加 `docs/architecture/packaging.md`，写"何时触发真拆 crate"判据 | 半会话 | 决策有据，未来触发点明确 |

**未来触发"真拆 crate"的判据**（建议写进 packaging.md 时参考）：
- 出现第三方 platform 实现的明确需求
- kernel 想脱离 Recursive 自家 tools/providers 体系复用
- kernel 编译时间成为 CI 瓶颈且能证明拆 crate 可缓解

**当下结论**：以上判据一个都没触发。推迟。

---

## Session companion 拆分 —— 推迟，理由记录

### handoff 的提议

把 `AgentRuntime` 拆出 `Session` companion 对象，把不需要 `&mut kernel` 的方法（`set_event_sink`、`current_todos`、`current_goal`、`approve_plan_mode_request`、`reject_plan_mode_request`、`plan_approval_gate`、`plan_mode_request_gate`）挪到不需要锁的伴随对象上。HTTP/TUI 调用面从 `Arc<Mutex<AgentRuntime>>` 改成 `Arc<Mutex<AgentRuntime>> + Arc<Session>` 双持有。

### 为什么推迟 —— handoff 的瓶颈描述已经过时

handoff 给的核心理由是"**单锁包整个 runtime**，busy 时连读 goal state 都得等"。但读 `src/runtime.rs:158-203` + `src/http/handlers.rs:735-750` + `docs/INTERNALS.md` 第 2 节同步原语表后发现**该描述部分准确但已过时**：

| handoff 描述 | 实际现状 |
|---|---|
| "set_goal/clear_goal/current_goal/plan_approval_gate 全都要抢同一把 tokio Mutex" | ❌ **错**。`goal_state`、`todo_list`、`plan_approval_gate`、`plan_mode_request_gate` 已经都是 `Arc<RwLock<>>` / `Arc<...Gate>`，**已经能脱离 runtime mutex 访问**（P1-2 已记录在 INTERNALS.md） |
| "force_clear_goal_when_runtime_busy 是瓶颈证据" | ❌ **过时**。该函数（实际名 `runtime_goal_state_clear`，`http/handlers.rs:739`）逻辑是 `try_lock` 失败 → 直接走 `goal_state.write()` 绕过 runtime mutex。**这就是 Arc-shared 设计的成功案例，不是被迫等的证据** |
| "busy 时连读 goal state 都得等" | ❌ **错**。`current_goal()` 直接读 `Arc<RwLock<Option<GoalState>>>`，不需要 runtime 锁 |

### 真正剩下串行化的只有三个 `&mut self` 方法

剥掉过时描述，`tokio::Mutex<AgentRuntime>` 真正串行化的只有 `run()` / `enqueue()` / `set_event_sink()`。**这三个本质上就该串行**：

- **`run()` / `enqueue()`**：驱动 kernel 跑一个 turn，会修改 transcript。两个 turn 并发跑会破坏 transcript 一致性——不是锁粒度问题，是**语义约束**。任何 agent runtime 都必须串行化 turn。
- **`set_event_sink()`**：会重新注册 TodoWriteTool 的 side effect（P0-2 文档化契约）。turn 中途换 sink 会让正在进行的 emit 流到新 sink，违反直觉。

### 结论

Session companion 拆分的实际收益≈0：
- "concurrent 读 goal/todo/plan gate 不被 turn 阻塞" → 已经能
- "concurrent clear goal 不被 turn 阻塞" → 已经能（`force_clear_goal_when_runtime_busy` 是 Arc-shared 的成功案例）
- "可以在 turn in-flight 时调 set_event_sink" → 现在不能，但**有意为之**

加上拆分要改 HTTP 4 处 + TUI 5 处 + 测试调用面大量改动，工程税高，**没有用户能感觉到的改善**。

**当下结论**：推迟。如果未来出现"必须在 turn in-flight 时切换 event sink"这类真实场景，再回来评估。

---

## 当前环境状态

### Git
```
main checkout (工作目录根):
  分支: main (HEAD: 387d534)
  与 origin/main 同步
  工作树 clean

worktree 列表（别的 session 在用，不要碰）:
  .worktrees/feat-providers-remote-catalog  [feat/providers-remote-catalog]
  .worktrees/g323-salvage                    [feat/g323-tui-loop]
  .worktrees/tui-mutant-debt                 [tui-mutant-debt]
  .worktrees/tui-mutant-debt-rest            [tui-mutant-debt-rest]
  （本会话的 .worktrees/p3-step-cap-and-memory-seq 已清理）
```

### GitNexus 索引
- 当前 index 停在 `9603add`（本地 P3 commit SHA）—— push 时被 rebase 成 `70b96ac`（远端 SHA）。GitNexus 看到 HEAD 是 `387d534` 会报 stale。
- **建议下个会话第一件事跑 `npx gitnexus analyze`**——把索引跟到 `387d534`。本会话最后已经做过一次，但 push 后 SHA 变了，所以再跑一次。

### PR 队列
- 本轮 P3 是 fast-forward merge 到 main（用户授权），**没开 PR**。
- 全部前序 PR (#6–#9) 都已 merged。

---

## 架构审查全局状态

| 项 | 状态 |
|---|---|
| P0 / P2 / P3 / P1-1 / P1-2 | ✅ 全 merged |
| P3-1 / P3-2（本轮） | ✅ merged `70b96ac` + push 到 origin |
| P1-3 crate 拆分 | ❌ 推迟（理由见上） |
| Session companion 拆分 | ❌ 推迟（理由见上） |

**架构审查正式收尾**。`run_inner` 394 → 117 行、锁层次有 INTERNALS.md 文档、P3 cleanup 全清。**剩下的不是架构问题，是产品/feature 问题**。

---

## 接下来真正该做什么

Recursive 是 self-improving Rust coding agent——产品工作的核心是**让它自己跑起来改自己**，而不是无限做架构 review。建议方向：

| 候选 | 说明 |
|---|---|
| **跑 self-improve loop** | `.dev/flows/self-improve.flow.js` 已经成熟。挑 `.dev/goals/` 里没完成的 goal，让 Recursive 自改 |
| **从 `.dev/goals/` 选真 feature** | 那里有完整的 goal 列表，是 Recursive 的真实 roadmap |
| **从 GitHub issues 选** | 真实用户反馈、bug |
| **TUI/HTTP feature 工作** | `crates/recursive-tui/` 还有 tui-mutant-debt 等清理工作（别的 session 在跑） |

**不要做的**：
- 不要重新评估 P1-3 或 Session 拆分（除非出现新证据）。本会话已批判性审视过。
- 不要再写"架构审查续作"handoff。架构审查收尾了。
- 不要碰 `.worktrees/` 下别的 worktree。

---

## 强制操作清单（接手第一件事）

1. **读 `CLAUDE.md` 整篇** — worktree 铁律、质量门、E2E 规则、"Known self-improve failure modes"段。
2. **读 `.dev/AGENTS.md` 整篇** — 8 条 Invariant。
3. **读 `docs/INTERNALS.md`** — 调用链 + 锁层次。**仍然准确**。
4. **读本 handoff 的"P1-3 推迟理由"和"Session 拆分推迟理由"两节**——避免被旧 handoff 误导。
5. **跑 `npx gitnexus analyze`** 把索引跟到 `387d534`。
6. **跑 `gitnexus_impact` 在任何代码改动前**（CLAUDE.md 强制）。
7. **不要直接在 main checkout 改代码** — 按 worktree 铁律开新 worktree。

## 不要做的事

- 不要在 `main` checkout 直接改产品代码。
- 不要碰 `.worktrees/` 下别的 worktree（别的 session 在用）。
- 不要重新评估 P1-3 / Session 拆分除非出现新证据。
- 不要修 `Cargo.toml` 的 `package.name`（`recursive-agent`）—— publish 流程依赖这个。
- 不要把 `AGENTS.md` 改名 —— `src/config.rs::load_project_context` 硬读路径。
- 不要 reset/amend 已有 commit。
- 不要跳过 cargo 质量门。

## 提醒

- `cargo` 不在默认 PATH — 所有命令前缀 `PATH="$HOME/.cargo/bin:$PATH"`（memory `feedback_cargo_path`）。
- 用中文回答（user global rule）。
- 修改前一定先跑 `gitnexus_impact`（CLAUDE.md 强制）。
- High/Critical 风险要先报告用户、得到确认再改（CLAUDE.md 强制）。

---

## 本会话工作产物索引

| 路径 | 内容 |
|------|------|
| commit `70b96ac` (origin/main) | P3-1 hard step cap + P3-2 monotonic seq |
| commit `387d534` (origin/main) | GitNexus 元数据刷新 |
| `.dev/journal/manual-20260706-p3-step-cap-and-memory-seq.md` | P3-1 + P3-2 决策理由 |
| `.dev/HANDOFF-2026-07-06-p1-3-and-big-refactor.md` | **旧 handoff，已被本文件覆盖** —— 仍保留作历史记录，但 P1-3 / Session 拆分建议已被推翻 |
| **`.dev/HANDOFF-2026-07-06-arch-review-wrap.md`** | **本文件** |

---

## TL;DR 给下个会话

> P3-1 + P3-2 落地、push 到 origin。架构审查正式收尾。
>
> **P1-3 crate 拆分和 Session companion 拆分都被推迟** —— 旧 handoff
> `HANDOFF-2026-07-06-p1-3-and-big-refactor.md` 的瓶颈描述已过时
> （P1-2 已经把 goal_state/todo_list/plan gates 都 Arc-shared），
> 下个会话不要被它牵着走重新评估。
>
> 接下来真正该做的：跑 self-improve loop、做 `.dev/goals/` 里的真
> feature、或者从 GitHub issues 选。架构已经稳了。
>
> 接手第一件事：跑 `npx gitnexus analyze` 把索引跟到 `387d534`。
