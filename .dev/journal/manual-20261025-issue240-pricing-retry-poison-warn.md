# 2026-10-25 — #240 pricing retry 记录 poison 锁静默丢弃 → WARN 可观测

**[16:3x] 交付行（wt-b1 席）**：`src/llm/pricing.rs` `RetryLog::take`
（原 :241 `lock().map(take).unwrap_or_default()`）——mutex poisoned（持锁
线程 panic 过）时整批累计 retry/计费记录静默丢弃返回空：poison 后所有
retry 从 `run_core` 步级账面（:548 每步 drain）消失、成本归集少计、
无日志。修法=改 `match`：`Ok` 臂原样 `mem::take`；`Err(poisoned)` 臂
`tracing::warn!`（target `recursive::llm`；`lost` = `poisoned.into_inner()
.len()` 即仍压在锁后的记录数；语义句「pricing retry records lost to
poisoned lock (degradation)」）后仍返回 `Vec::new()`——**drain 语义零
改动**（记录不回收；「poison 后回收记录」是产品决策，存照 Notes「两面」）。
sibling `record()` 的 `if let Ok` 吞 poison 不在本单（issue 只列 :241，
见 Notes）。1 测试新增 `retry_log_take_warns_when_lock_is_poisoned`(:975)：
正控健康 drain 返回 2 条且不告在前；poison 注入=另起 `std::thread`
持锁 `panic!`，`join().is_err()` + `log.records.lock().is_err()` 双证锁
确已 poison（同文件子模块可达私有字段 `records`，零生产 API 增加）；
断言返回空 + 语义句 + `lost=3` 计数；平台中性无 cfg 按 #224 口径，
不触 env 故无 #222 EnvGuard。负控还原 `unwrap_or_default()` → 钉红
「a poisoned drain must be announced, not silently emptied」（终态 :1012；
panic 报 :1004 为还原态行号，差 −8）、正控绿，`cmp` 逐字节还原。登记页
#240 ❌→✅。三门无管道全绿（fmt --check EXIT=0 / clippy --workspace
--all-targets --all-features -D warnings EXIT=0 / test --workspace EXIT=0
5305 passed/0 failed/12 ignored，74 suites）。#209 未动 mcp.rs。未 push、未动 main。等主控验收。

## Date

2026-10-25（wt-b1 席，基线 ddc92e43 = origin/main 含 #238，叠在本席
#239 提交 4065f170 之上；主控轮值注入 16:5x，两联 #239+#240 之二）

## Goal

#240（P3，fix(observability)，Goal 428 S4 扫描立单）：`RetryLog::take`
poison 臂 `unwrap_or_default()` 静默清零 → poison 臂 WARN（语义句 +
count 若可得）+ 回归钉（poison 注入：持锁 panic 后调用 → WARN 断言；
std `PoisonError` 处理照惯）。owner ruling「任何降级必须可观测」。

## Files touched

- `src/llm/pricing.rs`（产品：`take` :241 `lock().map().unwrap_or_default()`
  → `match` + `Err(poisoned)` 臂 WARN，+12 行含 3 行 rustdoc；测试：
  `retry_log_take_warns_when_lock_is_poisoned` 1 新增，紧邻 #116 的
  `RetryLog` 钉群之后）
- `docs/architecture/degradation-registry.md`（#240 行 ❌→✅，新行号 /
  观测方式 / 回归钉列）

## Tests added

- `llm::pricing::tests::retry_log_take_warns_when_lock_is_poisoned`
  （`#[tracing_test::traced_test]` + `#[test]`，同文件 :975）：
  1. 正控：seed 2 条 → `take().len() == 2` 且 `!logs_contain(SENTENCE)`
     （置于注入之前，避免缓冲区串扰）；
  2. 注入：再 seed 3 条；`Arc::clone(&log.records)` 交给 `std::thread::spawn`
     的闭包，闭包内 `lock().expect(..)` 持锁后 `panic!("poison the retry
     log")`；父线程 `join().is_err()` 证线程确 panic，`log.records.lock()
     .is_err()` 证锁确已 poison（夹具自证，防「测了个健康锁」）；
  3. 断言：`take()` 返回空（语义未变）+ `logs_contain(SENTENCE)` +
     `logs_contain("lost=3")`。
- 负控实证（临时还原、钉红、逐字节还原）：还原 `take` 体为原
  `self.records.lock().map(|mut r| mem::take(&mut *r)).unwrap_or_default()`
  → `panicked at src/llm/pricing.rs:1004:9: a poisoned drain must be
  announced, not silently emptied`，EXIT=101（同时 stderr 有 `thread
  '<unnamed>' panicked at :988` = 夹具故意的 poison 线程，非故障）；
  `cp` 快照回 + `cmp` 通过。正控段绿（panic 点在注入段之后）。
  （还原态少 8 行；终态 `assert!` 在 :1012。）

## Notes

- **三门无管道**（日志 /tmp/b1-240-{fmt,clippy,test}.log）：三者均
  `> log 2>&1; echo EXIT=$?`，无 `| tail`/`| grep`；数字见交付行。
  本单先 `cargo fmt --all -- --check` 预检再跑钉（#239 吃过一轮 fmt 红）。
- **爆炸半径（GitNexus MCP 本席不可用，grep 代替，同 #231 注 6）**：
  `RetryLog::take` 调用方仅两处——`anthropic.rs:366` / `openai.rs:412`
  （各自 `ChatProvider::take_retry_records` 实现）→ `run_core.rs:548`
  每步 drain 入 `LlmRetry` 事件。`Ok` 臂逐字原样；`Err` 臂返回
  `Vec::new()` 与原 `unwrap_or_default()` 等值。返回类型/签名/控制流
  零变。风险 LOW。
- **两面（存照，未硬上）**：`PoisonError::into_inner()` 已拿到 guard，
  技术上可直接 `mem::take` 回收那批记录（账面不丢），甚至
  `Mutex::clear_poison`。但「poison 即丢」是现行成文语义（`RetryLog`
  rustdoc 未承诺 poison 后行为；`record()` 同样 `if let Ok` 跳过），
  改成回收=改变 drain 语义=产品决策，超出本单「可观测性」口径。现状
  WARN+count 是 ruling 的下限；count 同时就是「若回收能救回多少」的
  证据，供 owner 裁决。
- **sibling `record()`（:224 `if let Ok(mut records)`）**：poison 后新
  retry 也静默不入账（push 被跳过）。issue 只列 :241（`take`），本单
  不扩面；若要补，形态同款（`Err` 臂 WARN，每次 retry 一条——poison 后
  响度随 retry 次数走，属应得）。立此存照交主控决定是否另立单。
- **poison 的现实可达性**：两段临界区分别是 `Vec::push`（仅 OOM 可
  panic）与 `mem::take`（不可 panic），poison 近乎理论场景；按 ruling
  仍登记并补观测——真发生时正是最需要痕迹的时刻（某处 panic 已被
  catch_unwind/tokio 任务边界吞掉，retry 账面是唯一旁证）。
- **测试线程 panic 噪音**：poison 线程的 panic 消息走 libtest 的
  output capture（spawned 线程继承 capture sink），通过时不外泄；
  负控红时随 stderr 一并打印（见上），属预期。
- mutants 前瞻（本地未跑，按单规 §2b）：删 warn → 钉红（负控已实证）；
  `lost` 替常量/删字段 → `lost=3` 断言红；`Err` 臂返回非空 → `drained
  .is_empty()` 红；`Ok` 臂替 `Vec::new()` → 正控 `len()==2` 及 #116
  两邻钉红。
