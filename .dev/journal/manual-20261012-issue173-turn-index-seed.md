# manual-20261012-issue173-turn-index-seed

- **Date**: 2026-10-12 (Asia/Shanghai)
- **Issue**: jeffkit/recursive#173 — `turn_index` 在 resume / AG-UI 后归零 →
  checkpoints 编号重复、rewind 恢复错文件（P1，属 #189「会话轮次与恢复一致性」簇）
- **Baseline HEAD**: 6c724056（分支 `fix/173-turn-index-seed`，worktree `recursive-173`）

## 根因

`AgentRuntime::enable_checkpoints`（`src/runtime.rs:2229-2294`）只把
`CheckpointState::turn_index`（`src/runtime/checkpoint.rs:40` 的 `Arc<AtomicUsize>`）
共享给 `checkpoint_save`，**从不播种**；全仓唯一写入是 `run_inner` 收尾处的
`fetch_add(1)`（`src/runtime.rs:598` 区域），没有任何 `store(...)`。于是每个
「新 runtime + 已存在 session 目录」的重开点都从 0 重新编号：

- `crates/recursive-cli/src/cli/resume.rs`（resume）
- `src/http/agui.rs`（`build_agui_runtime`：AG-UI 每个 run 重建 runtime）
- `crates/recursive-cli/src/main.rs`（批处理启动）

`CheckpointRecord::turn` 是 `plan_rewind` 的唯一筛选依据（`src/rewind.rs:116`
`r.turn >= to_turn` 进 dropped 集合；`src/checkpoint_log.rs:151` `truncate_to_turn`
保留 `turn < cutoff`），所以 resume 后的记录一旦复用旧 turn 号，就落到 cutoff 之下：
它的 `touched_files` 不进恢复集合，工作区文件不被回滚，而 transcript 仍按 user 消息数
截断（`src/session/lifecycle.rs:331`）——代码与对话历史不一致，且 rewind 报成功。

## 改动

播种来源取规格里的 **A：从 `checkpoints.jsonl` 播种**（该选择写进了
`enable_checkpoints` 的 `mode` 注释）。理由：`turn` 就是日志字段、也就是 rewind 的
筛选口径，`max(turn) + 1` 是唯一能保证「新记录绝不与同文件旧记录重号」的来源；
B（transcript 的 user 消息条数）依赖调用方每次都注入 transcript（无状态的 AG-UI run
不注入），一旦不注入就又退化成 0。

- `src/checkpoint_log.rs`
  - 新增 `pub fn next_turn_index(path) -> usize`（:165）：`max(turn) + 1`；日志缺失或
    为空 → 0；日志损坏 → `tracing::warn!` 后 0（不 panic，也不因坏日志让调用方放弃
    checkpoints）。用 `max` 而非「最后一条 + 1」：修复前的日志本来就可能是非单调的，
    自愈时不能重发已有编号。非测试代码无 `unwrap()`/`expect()`（`map_or` +
    `saturating_add`）。
  - 同文件单测 3 个（不变式 #4）：`next_turn_index_zero_for_missing_or_empty_log`、
    `next_turn_index_uses_max_turn_not_last`、`next_turn_index_zero_on_malformed_log`。
- `src/runtime.rs` — `enable_checkpoints` 在注册工具之前
  `next_turn_index(&log_path)` → `store(seed, Ordering::Relaxed)`（:2261-2262）。播种在
  builder/装配阶段，严格早于第一次 `fetch_add`，故 `Relaxed` 足够（写者唯一、读者
  `checkpoint_save` 在同一条控制流上更晚执行）；注释里写明内存序理由、seeding 来源与
  「三个重开点全部经由此函数」。
- `src/runtime/tests.rs:477` — `enable_checkpoints_seeds_from_existing_log`
  （日志 0/1/2 → `turn_index()==3`；缺日志 → 0）。
- `src/http/agui.rs:2502` — `agui_thread_continues_turn_numbering`：线程已有 0/1/2
  三条记录时 `build_agui_runtime` 的 `turn_index()==3`，并断言 `checkpoints_enabled()`
  以防空断言。
- `tests/checkpoint_e2e.rs`
  - `:175` `resume_continues_turn_numbering`：3 条快照 → resume → 下一条记录 turn=3，
    且全表无重号。
  - `:246` `rewind_after_resume_restores_expected_files`：resume 后写 `z.rs`，
    `rewind --to-turn 2` 的恢复集合必须包含 `z.rs`；落地后 `z.rs` 被删、
    `a.txt`/`b.txt` 回到 turn-1 快照。

未改 `checkpoints.jsonl` 行格式（规格「不做」项，`turn_id`/`turn_seq` 留 #174）。

## 验证

- **红（未打补丁、测试先落）**：`cargo test --test checkpoint_e2e`
  - `resume_continues_turn_numbering`：
    `assertion left == right failed: resume must seed turn_index from the log (max(turn) + 1)`
    `left: 0 / right: 3`
  - `rewind_after_resume_restores_expected_files`：
    `z.rs was touched by a rewound-away turn and must be restored; got ["a.txt"]`
- **负控（临时删掉 `store(...)` 两行、测试不动）**：上述 4 个新用例全红，另加
  `enable_checkpoints_seeds_from_existing_log`（0 vs 3）与
  `agui_thread_continues_turn_numbering`（0 vs 3）。恢复后 `src/runtime.rs`
  SHA-256 = `e7aa4ff1000890d0313f7b7007e905c20029394e1783469b734ab77e84d3ef31`
  （与负控前一致）。
- **绿**：
  - `cargo test -p recursive-agent --lib checkpoint` → 71 passed / 0 failed
  - `cargo test -p recursive-agent --lib rewind` → 14 passed / 0 failed
  - `cargo test --test checkpoint_e2e` → 4 passed / 0 failed
  - `cargo test -p recursive-agent --lib` → 3304 passed / 0 failed
  - `cargo test --test invariants` → 55 passed / 0 failed
  - `cargo clippy -p recursive-agent --lib -- -D warnings` → clean
  - `cargo fmt --all -- --check` → clean
- 新依赖：无。

## Notes

- **规格漂移（逐条核实）**：
  - `enable_checkpoints` 实际在 `src/runtime.rs:2229-2294`；规格写的 2203-2243 已漂移。
  - 规格「transcript 截断按 user 消息条数计算（`src/checkpoint_log.rs:151-153`）」不准确：
    `checkpoint_log.rs:151-153` 是 `truncate_to_turn`（截 **checkpoint 日志**，按 `turn`
    字段）；按 user 消息数截 **transcript** 的是 `src/session/lifecycle.rs:331`
    `truncate_transcript_to_turn`（`lifecycle.rs:372` 的 `matches!(role, "user")`）。
    「两套编号口径不一致」的结论仍成立，只是文件指错了。
  - 规格「冷加载路径（`http/cold_load.rs`）也要播种」在本基线**不成立**：全仓
    `enable_checkpoints` 只有 3 个调用点（resume.rs:752、main.rs:3014、agui.rs:1066），
    `src/http/cold_load.rs` 里没有 `enable_checkpoints`，HTTP REST 会话在本基线根本不启用
    checkpoints（Goal 284 后只剩按需 `checkpoint_save`）。故「grep 全部调用点并逐一处理」
    = 在唯一的装配点播种。
  - 规格示例里的 `CheckpointLogReader::last_turn(dir)` 在本仓不存在；实际是自由函数
    `checkpoint_log::read_log(path)`，新 helper 因此按仓库现有形态实现为
    `next_turn_index(path)`。
  - `resume.rs`、`main.rs`、`agui.rs` 三处调用点未改动：播种沉淀到
    `enable_checkpoints` 后，三处自动受益（这也是选 A 的工程理由）。
- **已知局限（诚实记录）**：
  - `turn` 的语义是「保存那一刻已完成的 run 数」，而日志只记了保存点。
    `max(turn)+1` 保证**不重号**，但当会话最后一个动作是 `checkpoint_save`（而不是
    一次 run 结束）时会**跳号**：例如日志 1/2/3 → 播种 4 → 下一轮保存得 5，turn 4
    不出现。跳号不影响 rewind 的正确性（按号段筛选，恢复集合仍完整），但 `turn` 不再
    严格等于对话位置；若要严格对齐，需要 #174 引入 `turn_seq` 或改存「已完成轮数」。
  - 两套编号口径（日志 `turn` vs transcript user 消息数）在「每轮恰好一条 user 消息」
    的常规会话里重新对齐；#174 做格式扩展时建议把这套口径写成显式不变量。
  - 负控是一次性手工操作（改 → 跑 → 还原 + 校验 SHA-256），没有固化成自动化回归。
