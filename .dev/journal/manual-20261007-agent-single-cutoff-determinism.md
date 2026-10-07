Date: 2026-10-07
Goal: #83 gate 修复 — `tools::agent::tests::execute_single_wall_timeout_returns` 在满载下偶发失败
Files touched:
  - src/tools/agent.rs
Tests added: 无（已存在的 execute_single_wall_timeout_returns / execute_single_cancel_returns_within_budget
  由"竞态决定 Ok/Err"改为"两条分支同判 → 确定性 Err"）

## What

`execute_single` 的超时/取消有 **两个同预算的计时器**：dispatch 级聚合 deadline
（`self.effective_deadline()`，在 worker future 首次 poll 之前就算好）与 worker 运行时
自带的 wall clock（`build_worker_runtime` 传 `wall_timeout_secs(self.wall_timeout_secs)`，
在 turn 真正开始时才起算）。正常（空载）下外层先到 → `Err(timeout_result)`；满载时
select 被轮询的时机被推迟，两个分支同时 ready，`tokio::select!` 随机取一个——
取到 worker 分支时 worker 自己已经优雅收尾成
`Ok("[worker 'w0' finished: WallClockExceeded]\n(no final message)")`，测试断言的
`expect_err` 就红了（gate 实测：3226 passed / 1 failed；隔离跑 3/3 绿）。

修法（不动测试、不弱化断言）：`run_worker` 返回 `WorkerReport { text, finish_reason }`
（parallel/sequential 只取 `.text`，行为不变），single 分支把 worker **自身**的
`WallClockExceeded` / `Cancelled` 也判成 `Err`——哪个计时器先响都得到 `Err`，竞态消失。
parallel 的 Ok+占位语义（不变量 #7/#8）与 sequential 的"带标签分段"语义原样保留。

修复轮（独立评审 NEEDS_FIX，2026-10-07）：该 `Err` 正文一度复用了聚合分支的
`timeout_result`（文案「aggregate deadline; worker did not finish」）并丢掉 worker
自己的报告（含工件引用）。现改为一律以 `report.text` 作 `Err` 正文
（`[worker 'wN' finished: WallClockExceeded|Cancelled]` + worker 正文/工件引用）：
`Err` 的确定性不变，文案不再张冠李戴，worker 报告与工件 id 不丢。
