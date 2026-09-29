# Goal 407 — 补齐取消令牌最后两个 surface：REPL 与 weixin headless（#40 blocker 1 收口）

**Roadmap**: issue #40 re-land（d174b8b）后 A/B 双侧审查共同确认的 blocker 1，2026-09-29
由 jeffkit 授权按 goal 自迭代解决（不发 GitHub issue）。COORD-issue31-40.md §3.2 口径：
「默认 `(None,None)` 无界 park」单独跟踪——本 goal 即该跟踪项的落地。

**依赖**: Goal 404/#40（wall 预算传播 + 取消令牌四 surface 接线 + TUI per-turn
`SharedTokenSlot` 镜像机制，已合入 main `d174b8b`）。

**Design principle check**:
- ✅ Does 消灭「不可中断」这一类：所有产品 surface 的 in-flight turn 都能被
  SIGINT/SIGTERM 取消（Run/Loop/Resume/TUI 已具备，本 goal 补 REPL 与 weixin）。
- ❌ Does NOT 引入默认 wall timeout——`RECURSIVE_WALL_TIMEOUT_SECS=0` 默认语义保持
  不变，时限是显式 opt-in（默认非零属产品决策，本 goal 不做）。
- ❌ Does NOT 改 `execute_parallel` 的 `(None, None)` 分支本体——程序化构造仍可达，
  但产品面从此不可达（invariant #7 finish-is-data 语义不动）。
- ❌ Does NOT 动 `.github/**` 与仓库外路径。

## Why（2026-09-29 核实）

#40 落地后并行 agent 的死锁防护 = wall 预算 ∥ 取消令牌，二选一即可终止。但取消令牌
的接线存在两个缺口（`crates/recursive-cli/src/main.rs`）：

| surface | 现状 | 后果 |
|---|---|---|
| `repl()`（:2547） | `build_runtime` 未传 shutdown token | `wall_timeout_secs=0` 默认下，并行 agent 挂死 ⇒ turn 永久 park，且无法优雅打断 |
| `run_weixin_headless_daemon`（:2821，token 实参在 ：2856） | `None, // shutdown_token` | 同上；无人值守面连 Ctrl-C 都没有，只能 kill -9 |

参照系：`Cmd::Loop`（:970）已用 `shutdown_signal()`（:1814，ctrl_c ∥ SIGTERM →
`CancellationToken`）；TUI 已有 per-turn slot 完整范本（`runtime_builder.rs` 持
`subagent_token_slot: SharedTokenSlot`，backend 四类 turn-start/clear 点镜像）。

**关键语义约束（为什么 REPL 不能直接抄 Loop 的静态 token）**：REPL 是多轮长驻——
静态 token 一旦取消即永久失效，第一轮 Ctrl-C 会毒化后续所有 turn。REPL 必须用
per-turn 令牌（父 token = `shutdown_signal()`，每 turn 派 child 放入 slot、turn 结束
清空）；weixin daemon 进程生命周期=令牌生命周期，静态 token 即正确。

## Scope（do exactly this, no more）

### 1. weixin headless（小而确定）
- `run_weixin_headless_daemon`：`build_runtime(..., shutdown_signal())`——SIGTERM/SIGINT
  ⇒ in-flight turn 以 `FinishReason::Cancelled` 收尾后进程照既有路径退出。

### 2. REPL per-turn 令牌（主体）
- `repl()`：参照 TUI 范本接入 per-turn slot——turn 开始安装 `shutdown_signal()` 的
  child token、turn 结束（含 Cancelled）清除；先调查 REPL 现有 Ctrl-C 处理路径
  （输入行编辑期的 Ctrl-C 与 turn await 期的 SIGINT 是两条路，别弄破前者）。
- 若 `cli::builder::build_runtime` 静态 token 参数无法表达 per-turn 语义，允许给
  builder 增加可选 slot 实参（复用 #40 的 `SharedTokenSlot` 类型），**不改既有签名
  的调用点语义**。

### 3. 测试
- REPL：并行 agent 运行中触发令牌取消 ⇒ 当前 turn `Cancelled` 收尾、**后续 turn
  照常可用**（多轮不毒化——这是与静态 token 的分界判据）。
- weixin：令牌取消 ⇒ in-flight turn Cancelled 收尾、daemon 优雅退出。
- 既有门不破：`tui-test-presence.sh`、CLI/TUI 既有测试、`cargo clippy -p
  recursive-agent -p recursive-tui -p recursive-cli --all-targets --all-features
  -- -D warnings`（**注意对改动 crate 强制重 lint**，2026-09-29 的教训：预热缓存
  会让 clippy 空转）；落地后盯 main CI 到绿。

## 验收（acceptance）
1. REPL：并行 agent 运行中 SIGINT ⇒ 当前 turn `Cancelled`、REPL 存活、下一 turn 正常。
2. weixin：SIGTERM ⇒ in-flight turn `Cancelled`、优雅退出。
3. 产品面 `(None, None)` 聚合分支不可达（Run/Loop/Resume/TUI/REPL/weixin 全有令牌）。
4. 全量门 + main CI 绿。
