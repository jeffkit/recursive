# manual-20261020-goal415-turn-identity-verification

- **Date**: 2026-10-20 (Asia/Shanghai)
- **Goal**: 415 — 会话轮次与恢复一致性 W1：turn 身份单一来源（#173；tracking #189 W1）
- **Baseline HEAD**: 4abc3a81（worktree `selfimprove-1792565967`）
- **结果**: **no-op** —— Goal 415 的全部 scope 已由 commit `b767abf6`
  （fix(session): #173 resume/AG-UI 后 turn_index 归零 → 从 checkpoints 日志播种，
  经 `e9e314ec` integrate(batch8) 合入 main）落地，且逐项核对本 goal 验收后无缺口。
  本 worktree 相对 baseline 零改动（`git status` clean，`git diff HEAD` 空）。

## 逐项核对（spec → 现状）

1. **turn 身份播种** ✅ `src/runtime.rs:2268-2269`：`enable_checkpoints`
   在注册任何 checkpoint 工具前 `let seed = crate::checkpoint_log::next_turn_index(&log_path);
   self.checkpoints.turn_index.store(seed, Ordering::Relaxed);`。resume
   （`crates/recursive-cli/src/cli/resume.rs:752`）、批处理
   （`crates/recursive-cli/src/main.rs:3020`）、AG-UI（`src/http/agui.rs:1072`）
   三个重开点全部经由该函数，一处播种即覆盖两条 goal 点名的恢复路径。
   播种源 `next_turn_index`（`src/checkpoint_log.rs:165`）= `max(turn)+1`，
   缺/空日志 → 0，损坏日志 `tracing::warn!` 后 0（降级可观测，符合 owner
   ruling「任何降级必须可观测」）。
2. **记录带 turn_id、续跑不撞号** ✅ `checkpoint_save`
   （`src/tools/checkpoint.rs:212-282`）写入的 `CheckpointRecord.turn` 取自
   播种后的共享 `turn_index`；run 收尾 `fetch_add(1)`（`src/runtime.rs:598`）
   只在播种之后发生，续跑记录从 `max(已存 turn)+1` 起接续。规格注记「不做」
   项（log 行格式不动，`turn_id` 字段名/`turn_seq` 留 #174）与现状一致：
   字段仍叫 `turn`，本单未改 `checkpoints.jsonl` 格式。
3. **回归测试** ✅ headline 三类齐备（名字近义）：
   - resume 播种：`tests/checkpoint_e2e.rs:175`
     `resume_continues_turn_numbering`（造 3 轮带 checkpoint 的会话 → 新
     runtime over 同一 session dir → `turn_index()==3`，且新记录 turn=3、
     全表无重号）——即 goal 点名的「新记录 turn_id ≥ N」断言。
   - AG-UI 路径：`src/http/agui.rs:2563`
     `agui_thread_continues_turn_numbering`（线程已有 0/1/2 记录 →
     `build_agui_runtime` → `turn_index()==3`，并断言 `checkpoints_enabled()`
     防空断言）。
   - 无 checkpoint 历史从 0 起步：`src/runtime/tests.rs:477`
     `enable_checkpoints_seeds_from_existing_log` 后半
     （缺日志 → `turn_index()==0`）+ `src/checkpoint_log.rs:247`
     `next_turn_index_zero_for_missing_or_empty_log`。
   - 单元播种断言：同 `enable_checkpoints_seeds_from_existing_log` 前半
     （0/1/2 → 3）。
   - 另有 `rewind_after_resume_restores_expected_files`
     （`tests/checkpoint_e2e.rs:246`）钉住下游语义：resume 后的记录不落
     `plan_rewind` cutoff 之下。
4. **负控** ✅ 记录在案（`.dev/journal/manual-20261012-issue173-turn-index-seed.md`
   「验证·负控」节）：临时删除 `store(...)` 两行后 4 个新用例全红
   （0 vs 3），恢复后 `src/runtime.rs` SHA-256 与负控前一致。

## 边界遵守

禁改清单（`src/compact/**`、`src/rewind.rs`、`src/tools/checkpoint.rs`、
`src/tools/agent.rs`、`src/tools/send_message.rs`、`src/session/**`、
`src/multi.rs`、`src/coordinator.rs`、`src/run_core.rs`、`src/kernel.rs`、
`.dev/**`、`docs/**`、`crates/recursive-tui/**`、`src/http/cold_load` 语义）
——本次零改动，天然遵守。已知局限（跳号：会话最后动作是 `checkpoint_save`
而非 run 结束时 `max(turn)+1` 会跳一个号）在原 journal 有诚实记录，属 #174
`turn_seq` 的范围，不在本单。

## 验证（本 worktree，baseline 4abc3a81 上复跑）

- `cargo test --workspace` → 3533 passed / 1 failed；唯一失败为
  `mcp::tests::http_initialize_fails_fast_and_ignores_the_tool_budget`
  （#209 已知负载敏感假红：并行全量 111.76s 下超时；单跑
  `cargo test -p recursive-agent --lib mcp::tests::http_initialize_fails_fast`
  → ok 0.35s）。未改 `mcp.rs` 预算或断言。
- 定向：`cargo test -p recursive-agent --lib checkpoint` → 71 passed；
  `--test checkpoint_e2e` → 4 passed；`--lib next_turn_index` → 3 passed；
  `--lib agui_thread_continues` / `--lib enable_checkpoints_seeds` → 各 1 passed。
- `cargo clippy --all-targets --all-features -- -D warnings` → clean（4m40s）。
- `cargo fmt --all -- --check` → clean。

## Notes

- 本单为验证型 no-op：无代码改动、无新测试（既有 4+1 条已覆盖 spec 全部
  断言），仅留此 journal 供 #189 簇追踪 W1 状态。
- W2/W3（#174 恢复集对齐 / #175 SessionLock+原子写）与并行子单 #176 未动，
  依赖本钉子（已存在）可开后续 goal。
