# manual-20261012-issue176-worker-id-reuse

- **Date**: 2026-10-12
- **Issue**: #176 — `fix(tasks): 复用 worker_id 重派会覆盖登记，旧 worker guard 反删新 worker`
- **Baseline HEAD**: 6c724056 (`origin/main`)
- **Chosen option**: **A（拒绝复用）**，并叠加规格「陷阱」要求的 guard 身份校验

## 根因

三处独立缺陷叠加成「登记被删 + 消息假投递」：

1. `spawn_background_worker` 在插入 `WorkerTable` 前没有 `contains_key` 检查。
   `WorkerTable` 以 `worker_id` 为键，同 `worker_id` 的第二次 `background:true` 派发
   直接覆盖旧条目；表里那份 `Arc<WorkerHandle>` 是旧 worker mpsc sender 的**唯一**持有者，
   覆盖即 drop → 旧 worker 的 `rx.recv()` 立刻返回 `None` → 旧 worker 退出。
2. 退出 guard（`WorkerDeregisterGuard::drop`）只做 `remove(&worker_key)`，不校验身份，
   于是**已经退出的旧 worker** 把**新 worker** 的条目删掉。此后新 worker 仍在跑，
   但已从表里消失、且旧条目里它的 sender 也随 `remove` 被 drop。
3. `SendMessageTool` 的 `task_id` 分支在遍历 `WorkerTable` 找不到活句柄时，
   落到 `return Ok("Message delivered to task ...")`。任务登记还在（`TaskRegistry`
   与 `WorkerTable` 是两套记录），所以调用方拿到「已投递」的成功回复，
   而实际上没有任何 turn 会执行。

## 改动

### `src/tools/agent.rs`

- `WorkerHandle`（240-252）新增 `pub worker_id: Arc<String>`：记录该 worker 的表键，
  供 guard 校验槽位归属。用 `Arc<String>` 避免每个 worker 多一次 String 分配。
- `WorkerDeregisterGuard`（268-312）由「`worker_key: String`」改为
  「`worker_key: Arc<String>` + `task_id: TaskId`」；新增模块级函数
  `remove_worker_if_current(workers, key, task_id)`：**仅当槽位仍属于该 task_id 时才 remove**。
  guard 的 `Drop` 改为调用它。这样即使键被复用，旧 worker 的 guard 也不会删掉新条目。
- `spawn_background_worker`（933-952）新增前置检查：`workers.contains_key(worker_id)`
  为真时返回 `Error::ToolRejected`（理由含 "is already live"）。检查放在
  `build_worker_runtime` **之前**，所以被拒的派发不构建 runtime、不注册 task、不投递首轮 prompt。
- `spawn_background_worker`（979-990 / 1010-1023）构造 `WorkerHandle` 与 guard 时
  带上 `worker_id_arc` / `task_id`。

### `src/tools/send_message.rs`

- `task_id` 分支（235-298）不再假成功：
  - 句柄存在但 `tx.send` 失败（通道已关）→ `Error::ToolRejected`；
  - 遍历后没有句柄 → `Error::ToolRejected`，理由含 "has no live worker"。
  两条路径都仍先把消息写进 `task.output_tx`（保留人读 `task_output` 的行为），但绝不回「已投递」。
- 模块头 `# Resolution order` 文档（1-20）同步说明新语义。

### `docs/architecture/tools/multi-agent.md`

- 修正过时描述：`WorkerTable` 实际是 `Arc<Mutex<HashMap<...>>>`（文档原写 `RwLock`）；
  补充「活 `worker_id` 不可复用」与「无活句柄即报错」两条契约。

## 为什么选 A 而不是 B

先按规格要求核实范围（**本 worktree 内实测**，非推断）：

- `WorkerTable` 的**写入点只有一个**：`src/tools/agent.rs::spawn_background_worker` 的
  `.insert(...)`。
- **读取点只有两个**，都在同一文件语义下：`src/tools/send_message.rs:251`（按 task_id 遍历）
  与 `:281`（按 worker_id `get`）。
- `task_output` / `task_stop` / `list_workers` **完全不碰** `WorkerTable`：它们用
  `TaskRegistry` / `WorkerRegistry`。所以规格「陷阱」里担心的
  「`task_id` 出现在多个工具的输入输出里，改键要一次改全」对本表并不成立——
  改键不会产生「列表里能看到、stop 不了」。

即便如此，B 仍要额外改语义才自洽：键换成 task_id 后，`send_message(worker_id=...)`（文档化的
legacy 寻址、也是 `background_worker_deregisters_from_worker_table_on_exit` 测试用的路径）
要么失去寻址能力，要么补 `worker_id` 标签字段+扫描；而「两次同 worker_id 都存活」时按
`worker_id` 寻址本身就是二义（规格要求的「各自可寻址」在 A 下由 `task_id` 天然满足）。
B 还要求规格测试表的第二个用例构造「旧 guard 迟到」的时序，不得不为此留时序缝。

A 用更小的 blast radius 直接消灭缺陷前提（覆盖不再发生），再按规格「陷阱」加上身份校验，
残余风险最低。因此**只实现 A，没有混入 B 的改键**。

## 验证

红灯（未打补丁的代码 + 新测试，`6c724056`）：

```
thread 'tools::agent::tests::second_spawn_with_live_worker_id_is_rejected' panicked at src/tools/agent.rs:2472:26:
second spawn with a live worker_id must be rejected: "Background worker 'w1' spawned as task 'task-dcde09ea-...'."
thread 'tools::agent::tests::old_worker_exit_does_not_remove_new_entry' panicked at src/tools/agent.rs:2551:9:
the old worker's exit guard must not delete the new worker's entry
thread 'tools::send_message::tests::send_message_to_dead_worker_reports_failure' panicked at src/tools/send_message.rs:639:23:
no live handle must be reported as a failure, not delivered: "Message delivered to task 'task-47eb811f-...'."
test result: FAILED. 0 passed; 3 failed; 0 ignored; 0 measured; 3299 filtered out
```

负控制（各只回退一处修复，测试保持不动，逐条观察失败，之后按 sha256 校验复原）：

- 回退 spawn 前置检查 → `second_spawn_with_live_worker_id_is_rejected` FAILED
  （第二次派发返回 Ok 的 spawn 文本）。
- 回退 guard 身份校验（恢复 pre-fix 裸 `remove(key)`、`String` 键）
  → `old_worker_exit_does_not_remove_new_entry` FAILED
  （`the old worker's exit guard must not delete the new worker's entry`）。
- 回退 `task_id` 假成功兜底 → `send_message_to_dead_worker_reports_failure` FAILED
  （`"Message delivered to task '...'"`）。

绿灯门禁：

- `cargo test -p recursive-agent --lib tools::agent` → 50 passed
- `cargo test -p recursive-agent --lib tools::send_message` → 21 passed
- `cargo test -p recursive-agent --lib` → 3303 passed
- `cargo test --test invariants` → 55 passed
- `cargo clippy -p recursive-agent --lib -- -D warnings` → clean
- `cargo fmt --all -- --check` → clean
- 额外：`cargo test --test agent_team_integration` → 14 passed（防 send_message 契约回归）

## Notes

- 选择 A 的**可见语义变化**：被拒的第二次派发不再往 `TaskRegistry` 写任何东西，
  因此只有「第一个 worker 仍活着」这一种结果；不会再出现「任务列表里两个 worker、
  实际只有一个能收消息」的状态。
- 规格「测试」表把方案 A 的断言写成 Err、方案 B 写成两次都存活，两者互斥；
  本实现取 A 的 Err 分支，并额外断言被拒派发未注册第二个 task、
  第一个 worker 仍能用 `task_id` 真正驱动一个 turn。
- `remove_worker_if_current` 是新增的模块级函数（非 `pub`），仍按 invariant #4 在同文件
  加了单元测试 `remove_worker_if_current_checks_task_identity`（空表 no-op / 异 task 不删 / 同 task 删）。
- ~~本单未改 `send_message` 的 `worker_id` 分支~~ → **该判定已被 reviewer 推翻并收口**，
  详见下节「复查返工」。另：legacy `WorkerRegistry` 的「无条目 → Ok(not found)」清单式回复
  仍按原契约保留（被 `verifier_probe_send_message_by_worker_id_after_exit` 钉住）。
- 残余不确定性：旧 worker 的 guard 迟到时，若期间 `task_stop` 已把新 worker 也停掉，
  槽位会由新 worker 自己的 guard 清理；A 方案下不存在「槽位属于谁」的歧义，
  因为同一时刻一个 `worker_id` 至多一个活条目。

## 复查返工（review rework · 第二个 commit）

### 1. `worker_id` 分支的「无活 handle」必须返回失败（reviewer 要求，已收口）

原实现该分支在 `tx.send()` 失败（receiver 已 drop）时返回
`Ok("Message queued for worker '<id>', but it has exited and cannot continue.")`，
与 `task_id` 分支的 `Err(ToolRejected "…has no live worker…")` 不一致；规格验收项写的是
「无活句柄 → **返回失败**（不是"已投递"）」。现改为同口径 `Err(ToolRejected)`，措辞含
`has no live worker … channel is closed … no turn will run`。

**可达性我更正了 reviewer 给的机制**（不是判断题，做了实测探针）：

- 探针（临时文件，已删）测 async block 的 drop 顺序：
  `PROBE aborted drop order: ["LOCAL", "UPVAR"]`、`PROBE completed drop order: ["LOCAL", "UPVAR"]`
  —— **body 局部变量先 drop，`async move` 捕获的 upvar 后 drop**。
- 因此 `WorkerDeregisterGuard`（body 局部）**先于** `rx`（upvar）被 drop：正常退出与
  「跑到一半被 abort」两条路径上，**表项先被移除、通道后关闭**，所以 reviewer 描述的
  「rx 已 drop 但 guard 还没拿到表锁」这个窗口在该路径上**不成立**。
- 但该状态**确实可达**，只是路径不同：`task_stop` 若在 worker **第一次被 poll 之前**落地，
  body 局部根本不存在（guard 从未构造），而 `rx` 在 future 构造时就已 move 进去、随 future
  被 drop。探针：`PROBE abort-before-first-poll: guard_ran=false, send_err=true`
  —— 表项残留 + 通道已关。
- 该真实窗口被并发的验收者探针独立复现：
  `VERIFIER immediate-stop: provider_calls=0 slot_present_after_stop=true`
  （worker 从未被 poll，槽位残留）。故这条 `Err` 收口是**必要**的。

### 2. 顺带收口同函数的 legacy 假成功（同一标准）

`WorkerRegistry::get` 不查 `is_done()`；`ParallelDeregister::drop` 的
`deregister_sync` 只用 `try_write`（竞争失败即放弃，条目残留待下次 `register` 清扫，见其文档），
所以「worker 已结束但 done 邮箱还在表里」可达。原实现向该邮箱 `push` 后回
`Ok("Message delivered to worker '<id>'.")` —— 与 #1 同类假成功，现改为 `Err(ToolRejected)`。
「无条目 → Ok(not found 清单)」**未动**（既有契约，且被验收者探针钉住）。

### 3. 返工中发现并修复的方案 A 自身缺陷（`src/tools/agent.rs`）

**(a) 死槽位把 worker_id 永久锁死（我自己引入的回归）**：
规格原文是「拒绝复用一个**仍然存活**的 worker_id」，而首版实现只做 `contains_key`。
结合上文 1 的真实窗口（`task_stop` 先于首次 poll → 死槽位残留），
`task_stop` 之后**再也无法复用该 worker_id**（无限期 `already live`）。
现改为**按存活判定**：`existing.tx.is_closed()` 为真视为死槽位，**驱逐**后继续派发；
只有存活槽位才 `Err`。既有测试 `second_spawn_with_live_worker_id_is_rejected` 仍绿。

**(b) 派发前置检查存在 TOCTOU（原 #176 覆盖在并发下仍可达）**：
前置检查发生在 `build_worker_runtime`（async 且慢）**之前**，两个并发同 id 派发可同时通过，
随后盲目 `insert` 即回到「覆盖登记」。并发的验收者探针实测：
`VERIFIER RACE: both concurrent spawns succeeded in 2/400 iterations`。
现改为**检查+插入在同一把锁内**（失败方 `task_registry.forget(&task_id)` 撤销自己刚注册的 task，
不留下孤儿；此时尚未 spawn，无需 abort）；同一探针随后实测
`VERIFIER RACE: both concurrent spawns succeeded in 0/2000 iterations`。

**残余（未做，需要时另开）**：`task_stop` 本身仍会留下那个死槽位（`slot_present_after_stop=true`），
现在只是「可驱逐 / 可复用」而非「立即释放」。要让 `task_stop` 当场清槽，需要把 `WorkerTable`
接进 `TaskState`/`TaskRegistry`（动 `src/tasks/`），超出本单文件面，故不做。

### 4. 并发事故记录（非本单代码问题）

返工期间，另一个验收者在**同一 worktree** 里把它会话开始时的快照 `cp` 回
`src/tools/agent.rs`/`src/tools/send_message.rs`，**覆盖了我未提交的返工**（本轮 `send_message.rs`
的 `worker_id`/done-mailbox 改动与其新增测试、`agent.rs` 的死槽位/原子插入改动）。
我从自己的快照 `/tmp/issue176/send_message.rs.rework`（sha `6b03d022…`）恢复了
`send_message.rs`，并逐项复核 `agent.rs`（`is_closed()`/`forget`/`remove_worker_if_current`/
`task_id` 身份校验均在，验收者探针残留 0 行），随后立即提交以消除未提交窗口。
验收者本人的残余不确定（01:56:47 之后是否有第三版未被捕获）：我核对恢复后的
`send_message.rs` sha 与我的快照一致，且下述 5 条新测试全绿，故**无丢失**。

### 5. 返工的门禁与负向对照

新增/改测试：
- `tools::send_message::tests::send_message_to_dead_worker_by_worker_id_reports_failure`（直接构造死句柄）
- `tools::send_message::tests::send_message_to_done_legacy_mailbox_reports_failure`
- `tools::agent::tests::dead_worker_slot_is_evicted_so_worker_id_is_reusable`
- `tools::agent::tests::worker_id_is_reusable_after_stop_before_first_poll`
- `tools::agent::tests::concurrent_same_worker_id_spawn_admits_at_most_one`（竞态护栏，命中率实测 ~0.5%/迭代）

负向对照（各自只回退一处，测试保持不动，观察转红后按 sha256 复原）：
- 回退 `worker_id` 分支为 `Ok("Message queued …")`
  → `send_message_to_dead_worker_by_worker_id_reports_failure` FAILED
  （`worker_id with a closed channel must fail, not report queued: "Message queued for worker 'w-dead', but it has exited and cannot continue."`）
- 回退 legacy `is_done()` 守卫
  → `send_message_to_done_legacy_mailbox_reports_failure` FAILED
  （`a done legacy mailbox must not report delivery: "Message delivered to worker 'w-done'."`）
- 回退死槽位驱逐（改回 `contains_key`）
  → `dead_worker_slot_is_evicted_so_worker_id_is_reusable`、
    `worker_id_is_reusable_after_stop_before_first_poll` FAILED（`already live`）
- 回退原子插入（改回盲目 `insert`）
  → `concurrent_same_worker_id_spawn_admits_at_most_one` FAILED
    （`both concurrent spawns registered worker_id 'w1'`）

- 未把 worker 表落盘（#106 范围）；未改 `background:true` 启动语义。无新增依赖。
