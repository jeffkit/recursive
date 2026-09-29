# Journal — issue #40: agent(mode=parallel) 死锁修复（worker 预算 + 取消传播）

- Date: 2026-07 (manual, self-improve pipeline issue-40)
- Goal: Fix #40 — parallel agent workers had no wall-clock budget and no
  cancellation path; one stalled `provider.complete()` parked the parent turn
  forever (0% CPU, no FinishReason).
- Files touched:
  - `src/run_core.rs` — `call_llm` non-stream branch now selects on the
    shutdown token and the remaining wall budget (spent budget short-circuits
    without issuing the call); `Error::Cancelled` maps to the existing
    FinishReason::Cancelled path (invariant #7). +2 unit tests.
  - `src/tools/agent.rs` — `AgentTool` gains `wall_timeout_secs` +
    `shutdown_token` + `shutdown_token_slot` (builders
    `with_wall_timeout_secs` / `with_shutdown_token` /
    `with_shutdown_token_slot`); `effective_shutdown_token()` resolves
    static-token-first, slot-second (clone-out under lock, `into_inner()`
    poison recovery). `build_worker_runtime` and `execute_parallel`
    propagate budget + child token via the effective token. `execute_parallel`
    wraps `join_all` in a select against the child token and an aggregate
    deadline, aborts stragglers and emits a paired placeholder result per
    dispatched worker (invariants #7/#8); parallel task deregisters via a Drop
    guard on all paths incl. abort. +3 unit tests
    (incl. `token_slot_resolves_and_prefers_static`).
  - `src/tools/send_message.rs` — `WorkerRegistry::deregister_sync` (try_write)
    for the Drop-guard abort path.
  - `src/multi.rs` — new `pub type SharedTokenSlot`; parameter of
    `register_subagent_if_enabled` is now `Option<SharedTokenSlot>`, wired into
    the AgentTool via `with_shutdown_token_slot`.
  - `crates/recursive-cli/src/cli/builder.rs` (loop-mode assembly) — passes a
    one-shot filled slot `Arc::new(Mutex::new(Some(shutdown.clone())))`
    (never refreshed: static-token semantics preserved).
  - `crates/recursive-cli/src/main.rs` — loop-mode passes the same one-shot
    `shutdown` slot; HTTP-serve passes a one-shot `http_shutdown` slot
    (server-lifetime token, cancellable on SIGTERM/Ctrl-C).
  - `crates/recursive-tui/src/runtime_builder.rs` — both builder chains forward
    `.wall_timeout_secs(config.wall_timeout_secs)` AND create a
    `subagent_token_slot: SharedTokenSlot`, passing
    `Some(subagent_token_slot.clone())` to `register_subagent_if_enabled`
    (previously `None`). `TuiRuntime` carries the slot as a `pub` field.
  - `crates/recursive-tui/src/backend.rs` — `Backend` exposes
    `subagent_token_slot` (cloned out of `TuiRuntime` in both
    `spawn_with_state*`); `worker_loop` receives it and mirrors the per-turn
    interrupt token into the slot at every token-install site
    (SendMessage / ConfirmPlan / SetGoal / RunSkillPrompt), and clears it at
    every matching clear site. Ctrl-C Interrupt therefore cancels the current
    turn's parallel sub-agent workers via the existing goal-383 child-token
    tree — no new state machine. +2 cfg(test) tests
    (`turn_start_populates_subagent_token_slot`,
    `turn_end_clears_subagent_token_slot`).
  - `tests/issue40-parallel-agent-budget.rs` — 6 integration tests: hanging
    provider × N∈{2,4} × max_steps=45 + budget → Ok + WallClockExceeded;
    static-token mid-run cancel → Ok + Cancelled, all worker ids paired;
    token-present + wall-deadline → WallClockExceeded (not mislabeled
    Cancelled); slot-path mid-run cancel
    (`parallel_workers_static_token_and_slot_both_resolve`) — the production
    TUI wiring; healthy-path regression.
- Tests added: 3 agent.rs unit, 6 integration, 2 TUI backend, 1 TUI
  runtime_builder struct-contract test.
- Notes: default behaviour unchanged (`wall_timeout_secs=0` → no new
  deadlines; no-token/slot-empty paths identical to before). Static token, if
  attached, takes precedence over the slot. Aggregate-deadline branch in
  `execute_parallel` records `timed_out` at select time — the
  Cancelled/WallClockExceeded label is decided BEFORE `cancel()` is called.

## Invariants 收尾（实施记录，02-plan.md §A–F）

- `src/run_core.rs`：抽离 `complete_with_budget`（非流式 select）与
  `wall_clock_finish` helper；`inject_skill_reminder` 移到
  `src/agent/types.rs`；`StaticBreakdownCache` 移到新模块
  `src/context_breakdown.rs`；`run_inner` 回到 ≤150 行、生产代码 ≤1500 行。
- 取消通路终态（与代码一致，取代早期记录）：
  - **TUI**：per-turn token slot 接线 — runtime_builder 两个构建路径均传
    `Some(slot)`，backend 在每个 turn 开始时把 interrupt token 写入 slot、
    turn 结束清空；Ctrl-C 经 child-token 树取消本 turn 的 parallel worker。
  - **HTTP-serve**：传服务器级 `http_shutdown`（一次性填充的槽，SIGTERM/
    Ctrl-C 可取消 sub-agent worker）。
  - **CLI loop / run**：传 `shutdown` 一次性填充槽（静态 token 语义）。
- `RECURSIVE_WALL_TIMEOUT_SECS` 默认仍为 0（不改默认值；README :172/:303 已
  有配置指引）。默认环境下的兜底恢复仍依赖显式设置该 env，或经 Ctrl-C 的
  per-turn token 取消（TUI 现已接通）。
- 验证（本轮最终事实）：`cargo test -p recursive --lib` 绿；
  `--test issue40-parallel-agent-budget` 6/6；`--test invariants` 41/41；
  `cargo test -p recursive-tui` 821 passed；`.dev/scripts/tui-test-presence.sh`
  通过（本轮 backend/runtime_builder 各新增 cfg(test)）；`cargo fmt --all`、
  `cargo clippy --all-targets --all-features -- -D warnings` 干净。

## 2026-09-29 人工复核终验（gate + 实机）

管线 guarded 后由人工接手复核（代替 jeffkit 拍板）：

- **全量质量门全绿**（target 热缓存）：`cargo fmt --all --check` 干净；`cargo clippy
  --all-targets --all-features -- -D warnings` 0 warning；`cargo test --workspace
  --no-fail-fast` 全部 ok（含 issue40-parallel-agent-budget 6/6、invariants 41/41、
  recursive-tui 821 passed）；管线门禁 `flows/gates/repo-tests.sh` exit 0（含 doc-tests）。
- **G3 实机端到端确认 PASS**：在本 worktree（大仓级 workspace）以
  `RECURSIVE_WALL_TIMEOUT_SECS=300 recursive run "并行派 4 个子 agent 分别调研
  src/crates/tests/docs"`（deepseek-flash，--output-format json）实测：4 个只读
  worker 全部完成、`stop_reason: end_turn` 正常收尾，全程 104s，无 0% CPU park、
  无需 kill —— issue 复现场景（多 worker 并行深调研）确认已修复。
- **代码评审结论：可合入**。取消链路四 surface 闭合（TUI 每turn slot 镜像同一
  token）、select 三/四路均有必然触发项、占位配对两条路径成立、无 Mutex 跨 await。
  两条非阻塞建议留 follow-up：①聚合超时分支可用 is_finished() 抢救已完成 worker
  的真实结果（现被 "worker did not finish" 占位替换）；②send_message.rs:105 注释
  所称 "later sweep" 实不存在，改注释或补 sweep。
- 处置：单 commit 合入 main 并推送（此前管线 guarded 未推送，符合人工确认前提）。
