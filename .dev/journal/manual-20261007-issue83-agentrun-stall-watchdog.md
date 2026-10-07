Date: 2026-10-07
Goal: #83 — AGENTRUN 活性检测替代纯硬墙（转录停更早杀 / 有增长跑满预算）
Files touched:
  - .dev/flows/agent_watchdog.py (new)
  - .dev/flows/self_improve_bridge_v2.py (main(): 按 env 装守护)
  - .dev/flows/test/flow_v2_paths.py (s37/s38/s39/s40/s41)
  - .dev/OPERATIONS.md (§11 补参数说明)
Tests added: flow_v2_paths s37（转录停更→提前击杀，真 ps+真 SIGTERM）、
  s38（持续增长→不误杀，跑满预算）、s39（缺省未配 → 不动 AGENTRUN；配了 → 装且幂等）、
  s40（判据纯函数表：ps 解析/本 worktree 匹配/后代活性/停更决策）、
  s41（宿主包装真链路提前击杀 + stall-kill.log 留痕）

## What

impl/fix/review 等 AGENTRUN 长节点此前只有扁平 `timeout_secs` 硬墙
（`self_improve_flow_v2.py` 顶部自注的欠账）。新增 `agent_watchdog`：
转录 `transcript.jsonl` size/mtime 停更 ≥ 阈值 且 本 worktree 的 recursive agent
无活跃子进程 → 逐 pid SIGTERM（沿用 timeout 类优雅刷盘链路，永不 killpg，#94）。
有增长即放行跑满预算（10-03 实证 566 轮的合法长活不再被截断）。

阈值 `RECURSIVE_STALL_SECS`，**缺省关闭**：未显式配置时安装函数直接返回 False，
AGENTRUN 执行路径一个字节都不动。接线在宿主层
（`self_improve_bridge_v2.main()` 包一层 `plaita_nodes.AgentRunNode.execute`）——
v2 图内 AGENTRUN 是同步库节点，图无法旁路轮询，这是宿主而非图能做的事。
击杀语义 = timeout 类（engine_error + checkpoint/worktree 豁免 → keeper 重派 L2），
不引入新重试类别（D4 不动）——**显式分类**，见下方修复轮：wrapper 在节点错误上打
`KILL_MARKER`，宿主 `run_host_v3._timeout_class` 认它。

## Evidence

`python3 .dev/flows/test/flow_v2_paths.py`：新 5 场景 全绿（红→绿：实现前 4 个
ERROR `No module named 'agent_watchdog'`）。s37 实测 1.0s 阈值下 1.09s 击杀。
本机既有失败（disk 守卫 19.8GiB<20、`/opt/homebrew/bin/python3.13` 缺失）与本次
改动无关，改动前基线同样 5/36。仅动 `.dev/`，Rust 三件套不受影响。

## Notes / boundary

- 检测面：`$RECURSIVE_SESSIONS_DIR/**/transcript.jsonl`（逐轮重算——首个
  AGENTRUN 时会话目录尚未创建）+ `ps -axo` 按 `--workspace` 指本 worktree 匹配
  agent（同 preflight kill-stale 判据）。观测不到（无进程/无转录）时保持惰性，
  宁可漏杀交硬墙兜底，绝不误杀。
- 长门/编译零转录增长但**有子进程** → 视为健康工作（g349 教训），不击杀。
- 已知边界：`engine=v2-console` 的派发在 console worker 上执行已发布定义，
  本地宿主的安装不生效（与 §11 其它 v3 宿主特性同一边界），已在 OPERATIONS.md
  写明。

## 修复轮（独立评审 NEEDS_FIX，2026-10-07）

**blocker（文档与实链不符）**：原稿只写了「击杀语义 = timeout 类」，实链上
SIGTERM → CLI 退出 143 → agentproc 报 `executor 'recursive' exited 143: (no
stderr)`，而宿主唯一判据是 agentproc 自己的 `"timed out after"`
（`self_improve_bridge_v2.py` 超时分支）——于是击杀被判成**普通节点失败**：原地重跑
一整轮 `RECURSIVE_IMPL_TIMEOUT`、二次挂死才 `node_retry_exhausted` 升人工，与 D4 及
承诺的 L2 续跑相反（s37-s41 只验了「杀不杀」，没验「杀完怎么分类」，所以漏过）。
修法（评审给的第一方案：wrapper 记触发 + 宿主按它分类，不伪造 agentproc 文案）：
- `agent_watchdog.stall_kill_error()`：wrapper 捕获节点失败且 `wd.reason` 已置位时，
  用 `KILL_MARKER`（`[stall-watchdog]`）在消息**最前**重抛（原错误缀尾留取证）；
- `self_improve_bridge_v2._timeout_class()`：`"timed out after"` **或** 该标记 →
  timeout 类（`raise`，不原地重试）；普通失败一字不变，仍走重试。
- 新增 s42（真 SIGTERM 全链）：impl 恰 1 次、无 `node_retries` 记账、checkpoint
  保留（s23 的豁免+续跑链路即由此接上）。两半各自的反事实都已实证红：
  wrapper 不重抛 / 宿主不认标记 → 该场景都红（`_drive_v3` 返回而非上抛）。

**minor**：
- `agent_pids(procs, "")` 曾把 `abspath("")`（= 宿主 cwd）当归属根 → 兄弟 run 的
  agent 也在击杀候选内。现空 worktree 直接返回 `[]`，且 wrapper 对求值不出来的
  worktree 整次调用不装守护（s40 补断言）。
- 安装日志曾无论观测面如何都报「已启用」；`RECURSIVE_SESSIONS_DIR` 未设时
  `paths` 恒空 → 守护永不触发。现按 `watch_root()` 如实分流（s40 覆盖解析规则）。
- `src/tools/agent.rs` single 模式：竞态输家的 `Ok` 改判 `Err` 时原样复用了
  「aggregate deadline」文案且丢掉 worker 自己的报告。现改为把 worker 自身的
  报告（`[worker 'wN' finished: WallClockExceeded|Cancelled]` + 正文/工件引用）
  作为 `Err` 正文——两分支同为 `Err`（确定性不变），文案不再张冠李戴。

Evidence（修复轮）：`flow_v2_paths.py` s37-s42 全绿（s42 1.4s）；反事实双红见上。
`cargo test --lib -- tools::agent::tests`、`cargo clippy --all-targets -- -D warnings`、
`cargo fmt --all` 见本仓三件套输出。
