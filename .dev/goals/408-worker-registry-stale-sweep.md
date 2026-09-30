# Goal 408 — WorkerRegistry 陈旧条目清理：alive 标志 + register 惰性 sweep（#40 评审遗留）

**Roadmap**: issue #40 re-land（d174b8b）评审 follow-up（A 侧评审旗标，B 侧 2026-09-30 立项）。
`deregister_sync` 注释声称 "the entry is removed by a later sweep"，但 sweep 从不存在。

**依赖**: 无（独立小改，与并行工作无交叉）。

**Design principle check**:
- ✅ Does 让 WorkerRegistry 自愈：worker 结束/被中止时标记 done，下次 register 惰性清理 done 条目。
- ❌ Does NOT 改变 send_message 工具对模型暴露的 schema/行为（注册表是内部设施）。
- ❌ Does NOT 改 `execute_parallel` 的 select 结构与占位配对语义（invariant #7/#8 不动）。
- ❌ Does NOT 引入任何后台线程/定时器——sweep 只发生在 register 调用时（惰性，零常驻成本）。

## Why（2026-09-30 核实）

`src/tools/send_message.rs:105`（deregister_sync）注释声称锁竞争放弃后
"the entry is removed by a later sweep"——**该 sweep 不存在**。后果：Drop 路径
（ParallelDeregister，abort/取消时）若 `try_write` 争用失败，陈旧条目滞留注册表，
直到同 id 重注册才被覆盖；期间对它的 send 会得到 mailbox 缺失/滞留消息的错误，
presence 表说谎。评审结论：改注释或补 sweep——本 goal 选择补 sweep（真实自愈）+
同步修正注释为真话。

`WorkerMailbox`（send_message.rs:37）是 `Arc<Mutex<VecDeque<String>>>` 纯队列，
无对端关闭信号——需显式 alive 标志。

## Scope（do exactly this, no more）

### 1. WorkerMailbox alive 标志
- `send_message.rs`：`WorkerMailbox` 增加 `done: Arc<AtomicBool>`（default false）；
  `mark_done()`（worker 终态时调用）与 `is_done()`；`Clone` 语义保持共享同一标志。

### 2. 生命周期接线（agent.rs）
- `run_worker` 的**每条**退出路径（正常 return、WallClockExceeded/Cancelled 等
  FinishReason 分支、Err）在返回前对自身 mailbox `mark_done()`；
- abort 路径：`execute_parallel` 对未完成 worker `handle.abort()` 前/后（任选一致位置）
  mark_done——注意 aborted task 可能来不及执行，须由聚合方（execute_parallel）代为
  标记，**不得依赖被 abort 任务自己跑**。

### 3. register 惰性 sweep
- `WorkerRegistry::register`：持写锁期间顺带 `retain` 掉 `is_done()` 的条目，再插入新表项；
- `deregister_sync` 注释改写为真话："on contention the entry is swept by the next
  register"（ sweep 存在性成立后这句话才允许保留）。

### 4. 测试（本 goal 的 headline）
- `tests/` 或 send_message 模块测试：
  - register A → mark_done → register B ⇒ sweep 后 A 消失、B 存活；
  - 存活条目不被误清；
  - deregister_sync 正常路径仍移除条目；
  - abort 场景：execute_parallel 聚合超时后，未完成 worker 的注册表条目
    在下一次 register 时被清（占位配对语义不变）。

## Files NOT to touch
- `.github/**`；`execute_parallel` 的 select 分支结构与占位配对逻辑（只在 abort 处
  增补 mark_done 调用）；`src/tools/` 下与 send_message/registry 无关的文件。

## Acceptance
1. 定向：`cargo test -p recursive-agent --lib tools::send_message` + 新增 sweep 测试全绿；
2. 全量：`cargo test --workspace`、`cargo clippy --all-targets --all-features -- -D warnings`、
   `cargo fmt --all -- --check`；agent-mutants 门（src/ 改动必触发）；
3. journal 记录 + commit message 按 Goal 407 惯例引用编号。

## Notes for the agent
- `deregister_sync` 在 **Drop 上下文**调用：不得加锁等待/睡眠；mark_done 是
  AtomicBool store，Drop 安全。
- invariant #5：产品代码无 `unwrap()`（锁用 `unwrap_or_else(|e| e.into_inner())` 惯例）。
- 2026-09-30 教训：本地 clippy 可能因预热缓存空转——对改动 crate `touch` 后再 lint。
