# Goal 408 — WorkerRegistry 陈旧条目清理：alive 标志 + register 惰性 sweep

**Date**: 2026-09-30
**Goal**: #40 re-land（d174b8b）评审 follow-up — `deregister_sync` 的注释声称
"the entry is removed by a later sweep"，但该 sweep 从不存在。补上真实自愈路径
（mailbox alive 标志 + register 惰性清理）并把注释改成真话。

**Files touched**:
- `src/tools/send_message.rs` — `WorkerMailbox` 增加 `done: Arc<AtomicBool>` 与
  `mark_done()` / `is_done()`；`WorkerRegistry::register` 在持写锁期间
  `retain(|_, mb| !mb.is_done())` 再插入；`deregister_sync` 注释改为真话；
  新增 5 个定向测试。
- `src/tools/agent.rs` — 新增 `WorkerMailboxDoneGuard`（RAII，覆盖 run_worker 的
  **每条**退出路径：正常 return / `?` 早退 / panic unwind / abort unwind）；
  `run_worker` 入口快照自身 mailbox 并挂 guard；`execute_parallel` 保留预注册
  得到的 mailbox，在聚合超时/取消分支 `handle.abort()` **之前**代为 `mark_done()`
  （被 abort 的 task 不保证再被调度）；新增 2 个测试。

**Tests added**:
- `send_message.rs`: `register_sweeps_done_entries`（A mark_done → register B ⇒ A 消失、B 存活）、
  `register_does_not_sweep_live_entries`（存活条目不被误清）、
  `register_sweeps_entry_marked_through_registry_clone`（标志经 Clone 共享）、
  `deregister_sync_removes_entry_without_marking_done`（同步正常路径仍移除条目且不置位）、
  `mailbox_done_flag_is_shared_and_defaults_alive`。
- `agent.rs`: `run_worker_flags_its_mailbox_done_on_exit`（正常 return 置位 + 下一次
  register 清掉残留条目）、`execute_parallel_timeout_flags_unfinished_workers_done`
  （聚合超时 abort 后未完成 worker 被聚合方标记 done，下一次 register 清场；
  占位配对语义不变）。

## Design notes

- **为什么 sweep 只在 register**：零常驻成本（无后台线程/定时器），注册是唯一的
  写热点，天然是回收点。
- **为什么 abort 由聚合方标记**：`JoinHandle::abort()` 之后被 abort 的 task 可能
  再也不被 poll，自己跑不到 guard；`execute_parallel` 在 abort **之前**对它预注册
  时拿到的 mailbox 置位——用的是本 dispatch 自己持有的实例（不是 `registry.get`），
  所以即使期间同 id 被别的 dispatch 重新注册，也不会误标别人的 mailbox。
- **为什么 guard 覆盖全部路径**：`WorkerMailboxDoneGuard` 是 `run_worker` 的局部量，
  在 `?`（build runtime 失败）、正常返回、panic/abort unwind 上都会 Drop；
  `mark_done` 只是 AtomicBool store，Drop 上下文安全（不等待、不加锁）。
- **不变的东西**：`send_message` 对模型暴露的 schema/行为未改；`execute_parallel`
  的 select 结构与占位配对（invariant #7/#8）未改；无 `unwrap()`（invariant #5）。

## Verification

- 定向：`cargo test -p recursive-agent --lib tools::send_message`（20 passed）、
  `tools::agent::tests::execute_parallel*`（4 passed）、新增两个 agent 测试全绿；
  新 abort 测试连续 15 次运行稳定（current_thread 运行时 + 无竞争 RwLock 读
  不 yield，宿主的 abort 是异步的，故断言能在 task 被丢弃前观察到注册表）。
- 手工变异验证（证明测试真的钉住行为，而非只钉住存在性）：
  1. 删掉 `execute_parallel` 里聚合方的 `mark_done()` ⇒
     `execute_parallel_timeout_flags_unfinished_workers_done` FAILED（w0 未置位）；
  2. 把 `WorkerMailboxDoneGuard::drop` 体换成 no-op ⇒
     `run_worker_flags_its_mailbox_done_on_exit` FAILED。
- 门：`cargo fmt --all -- --check`（clean）、
  `cargo clippy --all-targets --all-features -- -D warnings`（clean）、
  `cargo test --workspace`（全绿，lib 821 passed）、
  `.dev/scripts/agent-mutants.sh`（--in-diff 函数级，10 mutants）见 gate 输出。

## E2E gate round (fix 1/3)

失败信号：`target/release/recursive not found — run 'cargo build --release -p recursive-cli' first`，
随后 fallback 到 Docker 全量 e2e，`argus-build` 报 `Build exited with code 1`（31s，秒级失败＝
卡在 buildkit 元数据解析而非编译），于是容器从未启动，smoke case 报 `File ... does not exist`。

**根因是环境/构建产物，不是本次源码改动**（本地 smoke 直接跑产品二进制，改动的代码路径全绿）：

1. `e2e-local.sh` 只**使用**不**构建** `target/release/recursive`（脚本第 33-35 行），
   worktree 里从未构建过 release 二进制 ⇒ 快路径首行即红。
   修复：`cargo build --release -p recursive-cli`（4m34s）。二进制时间戳（10:12）晚于
   最后一次 src 改动（09:37），故本地 smoke 验的就是本次改动后的代码。
2. Docker 镜像构建失败是 AGENTS.md 记录过的 buildkit 元数据解析问题（DNS 污染），
   修复：先把基础镜像拉进本地 store —— `docker pull rust:1.88-slim` /
   `debian:bookworm-slim` / `docker/dockerfile:1.4`，再构建即可（验证：手工
   `docker build -f e2e/Dockerfile -t recursive:e2e-wt-probe .` 全流程 success）。

**验证**（两条路径都跑过，均绿）：
- `sh .dev/scripts/e2e-gate.sh` → exit 0（`local smoke PASS`，2 scenarios）；
- `RECURSIVE_E2E_DOCKER=1 sh .dev/scripts/e2e-gate.sh` → exit 0
  （`argus-build` success 492ms、`argus-setup` running、`smoke PASSED ✓`）。
- 清理：`docker rmi recursive:e2e-wt-probe`、删掉失败轮遗留的空网络
  `argusai-wt-bba4863-network`（0 容器，符合 AGENTS.md failure-mode #5 的安全条件）。

无源码改动（`git status` 仍只有本轮 goal 的两个源文件 + 本 journal）。
