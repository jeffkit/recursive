# Hand-off: Recursive 架构审查后续（P1-3 crate 拆分 + AgentRuntime 大改）

**Date**: 2026-07-06
**From**: 接手 `HANDOFF-2026-07-05-p1-followup.md` 的会话，落地了 P1-1（拆 `run_inner`）和 P1-2（锁层次文档 + `SessionLifecycle`）
**To**: 下一个会话
**Status**: P1-1 完成、P1-2 完成。剩余 P1-3（HIGH 风险）+ 一个真正的"大改"（companion split）+ 两个小尾巴。
**前置**: 先读 `CLAUDE.md` + `.dev/AGENTS.md`（铁律），再读 `docs/INTERNALS.md`（**本次新加**，必读——是下面两个改动的输入文档）。

---

## 这次会话做了什么（先建立上下文）

接 `HANDOFF-2026-07-05-p1-followup.md` 后做了三件大事 + 1 份 journal：

### Round 1: 推送 P0 + P2/P3/P1-1-gate（前 handoff 已写代码，本次只是推送）
- **PR #6**（`fix/p0-arch-fixes`，merged 17:24）— P0 真 bug
  - RunShell timeout 泄漏子进程 → `kill_on_drop(true)` + `start_kill()`
  - `set_event_sink` 副作用文档化 + 加 `replace_event_sink` 兄弟方法
  - HTTP fixtures 提取到 `tests/http_common/`
  - README/CLAUDE.md/website 的 `agent.rs` 引用更新为 `kernel.rs` / `runtime.rs` / `run_core.rs`
- **PR #7**（`fix/p1-arch-followup`，merged 17:34）— P2/P3 + P1-1 gate
  - README v0.5 示例 → v0.7 `AgentRuntime::builder()`
  - 双 `AGENTS.md` 加交叉引用（不能改名，`config.rs` 硬读路径）
  - CLAUDE.md 写入 3 条 self-improve 失效模式
  - `RunShell` 加 LLM 可调 `max_output_bytes`（2 MiB 硬 cap）
  - `with_tools` 改 `self.clone()` + 替换字段（避免漏字段）
  - `run_inner_function_body_stays_small` invariant 测试（≤ 400 行 gate）

### Round 2: P1-1 拆 `run_inner`（commit `080c172`，PR #8，merged 17:57）
**HIGH 风险落地**。8 个 staged commits，每 stage 独立 commit + 全套质量门 + invariant 测试全程绿：

| Stage | Helper | body 行数 |
|---|---|---|
| 起始 | — | 394 |
| 1 | `make_outcome` 工厂（统一 6 处 `RunInnerOutcome` 构造） | 375 |
| 2 | `check_shutdown` | 356 |
| 3 | `enforce_transcript_budget` | 340 |
| 4 | `drain_mailbox` | 324 |
| 5 | `handle_no_tool_calls` | 314 |
| 6 | `process_tool_results`（sentinel + stuck detection + skill injection） | 195 |
| 7 | `dispatch_llm_step`（stream forwarder + LLM call + emits） | **117** |
| 8 | 收紧阈值 400 → 150 + journal | — |

**Invariant #1**（loop 小）重新有意义；**Invariant #8**（tool_call/tool_result pairing）原样保留，4 个 `tool_call_pairing::*` + 3 个 `stuck_detection_*` 测试全绿。

### Round 3: P1-2 锁层次文档 + SessionLifecycle（commit `0541da4`，PR #9，merged 06:46）
**CRITICAL 风险但实际零破坏面**（所有 47 个 indirect caller 都通过 `pub fn` API）。两件配对的改动：

1. **`docs/INTERNALS.md`**（新文档，220 行）— 调用链 + 11 个同步原语表 + 锁获取顺序 + 4 个执行模式 + per-turn/session/process 状态归属 + 新增交互面接入指南。
2. **`src/runtime.rs`** — `AgentRuntime` 顶层 `session_closed: bool` → `session: SessionLifecycle { closed: bool }` 子结构。命名为 `Lifecycle` 而非 `State` 避免和 `http::SessionState` / `agui_tui::SessionState` 同名混淆。

**注意**：P1-2 的范围是**保守版**。journal 里写明了为什么没挪 `session_id` / `turn_index`（它们和 `CheckpointState::enabled()` 耦合），以及为什么没做"真正的大改"——见下文。

### Round 4: GitNexus 元数据刷新（commit `b2958db`）
两次 `npx gitnexus analyze` 后的 symbol/edge count 自动写回。无语义价值，纯 metadata。

---

## 当前环境状态

### Git 状态
```
main checkout (工作目录根):
  分支: main (HEAD: b2958db)
  未提交修改:
    AGENTS.md    — GitNexus 自动写回 (10121 → ? symbols)
    CLAUDE.md    — 同上
  → 这是 GitNexus 自动元数据，可独立 commit 或丢弃。

worktree 列表（已清理本会话的，剩下别的 session 的）:
  .worktrees/feat-providers-remote-catalog  [feat/providers-remote-catalog]
  .worktrees/g323-salvage                    [feat/g323-tui-loop]
  .worktrees/tui-mutant-debt                 [tui-mutant-debt]
  .worktrees/tui-mutant-debt-rest            [tui-mutant-debt-rest]
  （可能还有别的 session 加的，下个会话开 worktree 前先 git worktree list）
```

### GitNexus 索引
- 当前 index 停在 `b2958db`（main HEAD）。
- 已跟到 HEAD，所有 `gitnexus_impact` / `gitnexus_query` 调用都应该准确。

### PR 队列
- 全部 merged。无 open PR。

---

## 接下来要做的事（按优先级 + 风险）

### 🟠 P1-3: kernel-vs-platform crate 拆分（HIGH 风险，**先开 issue 讨论**）

**问题**: 现在所有产品代码都在一个 root crate `recursive-agent`（`Cargo.toml` 的 `members = [".", ...]`）。`src/` 下混着：
- **内核层**（应当稳定、轻量）: `kernel.rs`、`run_core.rs`、`agent/`、`message.rs`、`error.rs`、`llm/`、`tools/`
- **平台层**（HTTP、MCP、coordinator、multi-agent、session 持久化、storage、weixin）: `http/`、`mcp.rs`、`mcp_server.rs`、`coordinator.rs`、`multi.rs`、`session.rs`、`memory/`、`storage/`、`weixin.rs`

feature flag 太多但没"内核 crate"。**`recursive-agent` 还是混着所有东西**。

**建议方案**（来自前轮 handoff）: 开 `recursive-kernel` crate（kernel.rs + run_core.rs + agent/types.rs + message.rs + error.rs + llm/ + tools/），把 http/mcp/coordinator/multi/team/tasks/session/memory/storage/weixin 抽到 `recursive-platform`。CRAN 风格"两个 crate + re-export"可保 `use recursive::*` 不变。

**风险**: HIGH（破坏 v0.7 公共 API）。

**强烈建议**: **先开 issue 讨论再动**——影响：
- `publish` 流程（需要决定 publish 哪个 crate、版本号怎么对齐）
- 下游 import 路径（即便用 re-export，`Cargo.toml` 依赖也要改）
- self-improve flow（`.dev/flows/self-improve.flow.js` 里如果有路径假设要更新）
- GitNexus 索引（每次 `gitnexus analyze` 会重新建图，可能要重新校准 `processes`）

**推进步骤**:

1. **先写设计 doc** 而不是直接动代码：
   - 列出每个 `src/` 子模块归属哪个 crate
   - 决定 re-export 策略（`pub use recursive_kernel::*` 在 `recursive-agent/src/lib.rs`？还是改名？）
   - 决定 publish 顺序（kernel 先 publish、platform 后 publish？）
   - 决定 self-improve flow 兼容性策略
2. **开 GitHub issue** 把设计 doc 贴上去、@ 用户讨论
3. **拿到 user buy-in** 之后再开 worktree 动代码

**输入文档**:
- `docs/INTERNALS.md`（P1-2 新加的）— 第 1 节调用链、第 5 节状态归属，直接复用其分类
- `.dev/AGENTS.md` — Invariant #2（Tools/Llm 正交）已经强制了这个分层

**判断**: 这个动作可能要 2-3 个会话才能完成。先开 issue、得到用户 OK 再开始。**不要单方面动手**。

---

### 🟠 真正的大改: `AgentRuntime` ↔ `Session` companion 拆分

**问题（P1-2 没解决的）**: HTTP 和 TUI 都把 `AgentRuntime` 塞进 `Arc<tokio::sync::Mutex<AgentRuntime>>`（见 `http/handlers.rs:248`、`tui/backend.rs:678`）。**单锁包整个 runtime**，意味着：

- `set_event_sink`、`run`、`enqueue`、`set_goal`、`clear_goal`、`current_goal`、`plan_approval_gate` 全都要抢**同一把 tokio Mutex**
- HTTP 的 `force_clear_goal_when_runtime_busy` 注释说明这是个瓶颈——busy 时连读 goal state 都得等
- 任何并发场景（HTTP 多请求同 session、TUI backend 处理用户 abort）都被串行化

**为什么 P1-2 没做**: 见 `manual-20260705-p1-2-runtime-internals.md` 的"Notes"段。短版：要做这个，得拆 `AgentRuntime` 出一个伴随 `Session` 对象，把不需要 `&mut kernel` 的方法（`set_event_sink`、`current_todos`、`current_goal`、`approve_plan_mode_request`、`reject_plan_mode_request`、`plan_approval_gate`、`plan_mode_request_gate`）挪到不需要锁的伴随对象上。HTTP/TUI 的调用模式要从 `Arc<Mutex<AgentRuntime>>` 改成 `Arc<Mutex<AgentRuntime>> + Arc<Session>` 双持有。

**风险**: HIGH。改 HTTP/TUI 调用面，影响所有 interactive surface。

**强烈建议**: **等 P1-3 crate 拆分先做**——如果在 `recursive-agent` 一个 crate 里做这个大改、之后又要拆 crate，会重复劳动。先拆 crate、再在 `recursive-platform` 里做 Session 拆分，更顺。

**推进步骤**（假设 P1-3 已落地）:

1. **设计 Session 对象**:
   ```rust
   // 在 recursive_platform 里
   pub struct Session {
       // 从 AgentRuntime 剥出来的字段
       todo_list: Arc<RwLock<Vec<TodoItem>>>,
       plan_approval_gate: Arc<PlanApprovalGate>,
       plan_mode_request_gate: Arc<PlanModeRequestGate>,
       goal_state: Arc<RwLock<Option<GoalState>>>,
       event_sink: Arc<RwLock<Arc<dyn EventSink>>>,  // 注意：现在 event_sink 也要 RwLock
       // ...
   }
   pub struct AgentRuntime {
       kernel: AgentKernel,
       transcript: Arc<Vec<Message>>,
       session: Session,  // ← 伴随对象
       // 只剩需要 &mut kernel / 串行化的字段
   }
   ```
2. **HTTP/TUI 调用面改造**:
   ```rust
   // 之前
   let rt = Arc::new(tokio::sync::Mutex::new(runtime));
   // 之后
   let rt = Arc::new(tokio::sync::Mutex::new(runtime));
   let session = rt.lock().await.session_handle();  // Arc<Session>
   // 只读操作用 session，不抢 rt 锁
   ```
3. **每一步独立 commit + 全套质量门**（参考 P1-1 的 staged 推进法）

**输入文档**:
- `docs/INTERNALS.md` 第 2 节（同步原语表）+ 第 3 节（锁获取顺序）——直接告诉下一会话哪些字段已经 Arc-shared、哪些需要改
- `http/handlers.rs:735-760`（`force_clear_goal_when_runtime_busy`）——这是当前瓶颈的具体证据，新设计要解决这个

**判断**: 单 session 能做完，但需要先和用户确认设计方向（Session 对象的字段粒度、HTTP/TUI 调用面是否破坏 v0.7 公共 API）。

---

### 🟡 小尾巴（可塞 cleanup commit）

#### P3-1: `max_steps=0` 加生产硬 cap

前轮 journal `manual-20260705-p1-arch-followup.md` 评估为"显式契约、有测试守护、不是 bug"。如果决定要给生产环境加硬 cap：

- 做 `RECURSIVE_HARD_STEP_CAP` env var，**不要**用隐藏常量
- 默认 unbounded（保 `recursive loop` 长 session 行为）
- 显式文档化

低优先级。

#### P3-2: `multi::SharedMemory::set` 用 monotonic seq

前轮 journal 评估为"改 wire format 风险大、收益低"。当前 `MemoryEntry.timestamp` 是 `SystemTime::now()`，仅用于显示 + ID hashing。

正确修复：加 `seq: u64` 字段做内部排序键、保留 `timestamp` 作 wall-clock 展示。要更新 wire format 文档。

低优先级。

#### TUI 改动跟进

本会话没碰 `crates/recursive-tui/src/`，但如果下个会话在 P1-3 或大改里动了 TUI：
- 跑 `.dev/scripts/tui-test-presence.sh` + `.dev/scripts/tui-mutants.sh`
- 跑 PTY tour（`.dev/skills/tui-acceptance.md`）
- `RECURSIVE_TUI_TEST_PRESENCE=0` opt-out 仅纯重构可用、要写明理由

---

## 强制操作清单（接手第一件事）

1. **读 `CLAUDE.md` 整篇** — worktree 铁律、质量门、E2E 规则、"Known self-improve failure modes"段（本会话没改）。
2. **读 `.dev/AGENTS.md` 整篇** — 8 条 Invariant。
3. **读 `docs/INTERNALS.md`**（**P1-2 新加，必读**）— 调用链 + 锁层次，是 P1-3 和大改的输入文档。
4. **读 journal `manual-20260705-p1-1-run-inner-split.md` + `manual-20260705-p1-2-runtime-internals.md`** — 两轮决策理由。
5. **读 memory 反馈**（`~/.claude/projects/-Users-kongjie-projects-Recursive/memory/`）— worktree / self-improve / cargo PATH 三条铁律。
6. **跑 `gitnexus_impact` 在任何代码改动前**（CLAUDE.md 强制）。
7. **不要直接在 main checkout 改代码** — 按 worktree 铁律开新 worktree。

## 不要做的事

- 不要在 `main` checkout 直接改产品代码。
- 不要碰 `.worktrees/` 下别的 worktree（别的 session 在用——开 worktree 前先 `git worktree list`）。
- 不要单方面启动 P1-3（**先开 issue**）。
- 不要修 `Cargo.toml` 的 `package.name`（`recursive-agent`）—— publish 流程依赖这个。
- 不要把 `AGENTS.md` 改名 —— `src/config.rs::load_project_context` 硬读路径。
- 不要 reset/amend 已有 commit（新 commit 优先）。
- 不要跳过 cargo 质量门（`-D warnings` 让 clippy warning 也是 fail）。

## 提醒

- `cargo` 不在默认 PATH — 所有命令前缀 `PATH="$HOME/.cargo/bin:$PATH"`（memory `feedback_cargo_path`）。
- 用中文回答（user global rule）。
- 修改代码后查 `docs/DOC_CODE_MAP.md` 是否需要同步文档（user global rule；当前项目没有这个文件，可跳过）。
- 修改前一定先跑 `gitnexus_impact`（CLAUDE.md 强制）。
- High/Critical 风险要先报告用户、得到确认再改（CLAUDE.md 强制；P1-3 和大改都是 HIGH）。

---

## 本会话工作产物索引

| 路径 | 内容 |
|------|------|
| PR #6（merged） | P0 真 bug |
| PR #7（merged） | P2/P3 + P1-1 gate |
| PR #8（merged） | P1-1 拆 `run_inner` |
| PR #9（merged） | P1-2 锁层次文档 + SessionLifecycle |
| `docs/INTERNALS.md` | 调用链 + 锁层次（必读，P1-3 + 大改的输入） |
| `.dev/journal/manual-20260705-p1-1-run-inner-split.md` | P1-1 决策理由 |
| `.dev/journal/manual-20260705-p1-2-runtime-internals.md` | P1-2 决策理由（含"为什么没做真正的大改"） |
| `.dev/HANDOFF-2026-07-05-arch-review.md` | Round 1 handoff（P0） |
| `.dev/HANDOFF-2026-07-05-p1-followup.md` | Round 2 handoff（P2/P3/P1-1-prep） |
| **`.dev/HANDOFF-2026-07-06-p1-3-and-big-refactor.md`** | **本文件** |

---

## TL;DR 给下个会话

> P0/P2/P3/P1-1/P1-2 全部落地、PR #6/#7/#8/#9 全部 merged。`run_inner` 394 → 117 行，
> 锁层次有 `docs/INTERNALS.md` 220 行文档。
>
> 接下来按优先级：
> 1. **P1-3 拆 crate** — 先开 issue 讨论（HIGH 风险、影响 publish/下游/self-improve），用 INTERNALS.md 第 5 节作输入
> 2. **AgentRuntime ↔ Session companion 拆分** — 解决 `tokio::Mutex<AgentRuntime>` 串行化瓶颈；建议等 P1-3 落地后再做，避免重复劳动
> 3. 小尾巴 P3-1/P3-2 可塞 cleanup commit
>
> 不要在 main checkout 改代码。所有改动前跑 `gitnexus_impact`。所有 cargo 命令前缀 `PATH="$HOME/.cargo/bin:$PATH"`。P1-3 和大改都要先报告 user、得到确认。
