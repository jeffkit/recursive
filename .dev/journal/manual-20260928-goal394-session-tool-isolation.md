# Journal — Goal 394 会话级工具状态隔离（fork_session）

- **Date**: 2026-09-28
- **Goal**: `.dev/goals/394-per-session-tool-state-isolation.md`（issue #21）
- **Branch**: `feat/goal-394-session-tool-isolation`（从 main `0ea7bd2` + 抢救出的 WIP 快照 `b7d4b40` 重建，未原样使用 stash 快照）

## Files touched

- `src/tools/registry.rs` — `SessionToolState`、`Tool::fork_box`（dyn 兼容，默认 None=无会话态）、`ToolRegistry::fork_session()`（真正重建会话态）、`fork()` 文档诚实化（仍是 clone，指向 fork_session）、struct 文档修正；新增 8 个行为 pin 测试。
- `src/tools/fs.rs` / `src/tools/edit.rs` — `ReadFile` / `WriteFile` / `EditTool` 覆写 `fork_box`，重建 guard slot + session_roots（构造期注入的 `Arc<Mutex<ReadFileState>>` 只换 registry 字段是无效的，必须重建工具实例）。
- `src/tools/glob.rs` / `search.rs` / `estimate_tokens.rs` / `count_lines.rs` / `client_fs.rs` — 持有 session_roots 的工具覆写 `fork_box`。
- `src/tools/run_background.rs`（`RunBackground`/`CheckBackground`）/ `watch_file.rs` / `stop_loop.rs` — 持有 bg_manager 的工具覆写 `fork_box`；这四个工具补了 `#[derive(Clone)]`（原来没有 Clone，fork_box 里的 `self.clone()` 会克隆引用而不是值）。
- `src/tools/agent.rs`（2 处）/ `src/multi.rs`（1 处）— 子代理注册表从 `fork()`（=clone）切到 `fork_session()`。
- `src/tools/mod.rs` — re-export `SessionToolState`。

## 重建 vs 共享清单（及理由）

**重建**（每 fork 一份）：
- `read_file_state` — 新 slot，内容快照自父（fork 语义：子继承 fork 时已读的记录），`Arc` 全新；Read/Write/Edit 随 fork 重新接线。
- `touched` — 新空 slot（checkpoint/审计归属归 fork 自己）。
- `bg_manager` — 新空 manager（运行中的任务属于创建它的会话，不继承）；RunBackground/CheckBackground/WatchFile/StopLoop 重新接线。
- `session_roots` — 新 slot，按父当前 roots 播种（fork 前已授予的保持可用，fork 后的 `/add-dir` 扩容留在会话内）。

**共享**（有意，勿当 bug 修）：
- `transport`（401/402 的执行环境绑定，不可变配置）
- `permissions` / `permission_mode` / `permission_hook` / `policy` / `auto_classifier` / `headless` / `hook_runner`（进程级权限配置）
- MCP client / elicitation（启动期外部资源，fork 不重连）
- `aliases`（静态映射）
- scratchpad / memory / facts / todo（工作区级，非会话级）

## Tests added

`cargo test --lib fork`（11 个，含原 Goal-247 的 fork 测试）：
- `fork_session_allocates_fresh_state_slots`（slot 指针互不相同）
- `fork_session_isolates_read_guard_between_forks`（A 的读不进 B 的 guard）
- `fork_session_read_guard_is_enforced_within_and_isolated_across`（A 读→A 可编辑；B 未读→拒；B 自读→放行）
- `clone_shares_read_state_but_fork_session_snapshots_it`（clone 别名共享 vs fork 快照不别名、fork 后父读不可见）
- `fork_session_isolates_touched_files`（A 写不入 B/父）
- `fork_session_isolates_background_jobs`（A 的 job 在 B 是 unknown）
- `fork_session_isolates_sandbox_root_expansions`（fork 前 fork 看不到扩容；fork 后 fork 播种可见）
- `legacy_fork_still_shares_state`（pin 住 fork()=clone 的诚实语义）

## Notes / traps（踩过的坑）

1. **工具实例必须随 fork 重建**：guard slot 是构造期注入的，这正是 goal 预警的最容易漏的点；用 dyn 兼容的 `fork_box`（每个有会话态的工具自己 opt-in）替代 WIP 里集中式 `fork_tool` + `had_read_state` 参数的写法，新增工具漏接线的风险落在工具自己的 impl 旁边。
2. **`build_standard_tools_with_roots` 不给 registry 自身挂 session_roots 字段**（只挂给工具；CLI builder 之后手动 `with_session_roots`）。fork_box 因此只在该工具有 slot 且 fork state 有新 slot 时才重接线，绝不把 slot 摘掉（否则会静默收紧沙箱）。
3. WatchFile/StopLoop/RunBackground/CheckBackground/ClientReadFile/ClientWriteFile/EstimateTokens 原来没有 `#[derive(Clone)]`。
4. **scope 决策**：HTTP 侧 clone 点（`handlers.rs` create/fork）**刻意未切**——393/395/396/397 四个在途分支都在重写 handlers.rs，切换留到消费分支落地（#24 冷加载落地后可切 `fork_session()`）；goal 的 Acceptance 里「HTTP 调用 ≥5 处」相应放宽，registry 层定义+测试已齐。

## Gates

- `cargo test --workspace` — 38 个 suite 全 ok，0 failed
- `cargo clippy --all-targets --all-features -- -D warnings` — 干净
- `cargo test --test invariants` — 41 passed（含 sandbox 逃逸用例）
- `cargo fmt --all` — 已跑
