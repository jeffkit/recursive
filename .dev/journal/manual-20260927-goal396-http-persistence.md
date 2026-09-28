# Manual — Goal 396: HTTP 会话持久化写路径接通

- **Date**: 2026-09-27
- **Goal**: `.dev/goals/396-http-session-persistence-writethrough.md`（issue #23）
- **Worktree**: `.worktrees/goal-396-http-persistence`（branch `feat/goal-396-http-persistence`，自 main `9ba806f`）

## Files touched

| File | Change |
|------|--------|
| `src/runtime/builder.rs` | 新增 `storage()` / `session_store()` 两个转发 setter（转发到 `AgentKernelBuilder::with_storage` / `with_session_store`，对齐 `compactor` 转发模式）+ 单测 |
| `src/http/mod.rs` | `AppState.storage: Arc<dyn StorageBackend>` 字段；reaper 循环体抽成 `evict_idle_sessions(&AppState) -> Vec<String>`（pub(super)，Goal 395 提取时可整体搬走），驱逐落盘在**锁外**；新增 `flush_all_sessions(&AppState) -> usize`（优雅关停全量落盘并 drain）；+ `goal_396_persistence_tests` |
| `src/http/handlers.rs` | `create_session` / `fork_session` 注入 `.storage(state.storage.clone())`；`delete_session` close 后快照 transcript、移出 map、**锁外**落盘；修正「after each turn 自动保存」的不实注释 |
| `crates/recursive-cli/src/main.rs` | http 启动：默认 `LocalStorageBackend::new(user_workspace_dir(&config.workspace)?)`；`RECURSIVE_REDIS_URL` / `RECURSIVE_S3_BUCKET` 被设置时打印「recognized but not yet wired in http mode」；关停后调用 `flush_all_sessions` 并打印落盘数 |
| `tests/http_common/mod.rs` | 新增 `MemoryStorage`（内存 fake：记录 save、可挂 sessions-map 探针断言锁外、支持 `load_transcript` 往返） |
| `tests/http.rs` | 集成测试 ×2：DELETE 落盘往返 + invariant #8 配对；两会话删除互不串 |
| `tests/agui_e2e.rs`、`tests/v050_integration.rs` | 复用 `http_common` 的 `memory_storage()` 补新字段 |
| `README.md` | cloud cheatsheet 对齐事实：本地落盘语义（关停/驱逐时保存，最多丢最后一次落盘之后的轮次）、Redis/S3 未接线、`restore_from_storage()` 不存在、冷加载属 Goal 397 |

## 关键决策

1. **`transcript()` 访问器已存在**（`src/runtime.rs:804`）——按 goal 的优先方案，未新增任何 runtime API；`src/kernel.rs` / `src/runtime.rs` 零改动。
2. **Goal 395（SessionHost 抽离，issue #22）尚未落地**——宿主层逻辑仍在 `src/http/mod.rs`（sessions map + reaper task）。本 goal 的落盘钩子挂在**当前**宿主路径上（reaper sweep / DELETE / 关停 flush），并把 sweep 写成 `evict_idle_sessions(&AppState)` 自由函数，395 抽离时可原样搬移。两个新增测试跟随该函数。
3. **锁外语义的验证方式**：fake backend 在 `save_transcript` 入口（任何 await 之前）对宿主的 sessions map 做 `try_write()`，记录成败；若宿主在持锁状态下落盘，探针拿不到写锁 → 测试失败。复用了 395 设计里的锁范围要求的同一思路（探针而非断言时序）。
4. **频率语义**：默认只在 DELETE / idle 驱逐 / 优雅关停各落一次（`save_transcript` 全量覆盖）。`RECURSIVE_HTTP_TRANSCRIPT_FLUSH_SECS` 周期防抖**未实现**（goal 允许：成本高可留 TODO）——TODO: 长会话被 kill -9 时会丢自上次落盘后的全部轮次；若需要，加一个 reaper tick 内的定时防抖落盘即可，无需新任务结构。
5. **写放大取舍（goal 要求记录）**：`save_transcript` 每次全量覆盖整条 transcript，长会话（MB 级）在驱逐/关停时有一次明显写放大；相对每轮 O(N²) 的全量重写这是有意的取舍。落盘只发生在会话终止路径，不在热路径。
6. **POST /run 与 /agui 不落盘**：二者是瞬态 runtime，不创建 SessionState，不在「会话持久化」范围内。
7. **busy 会话驱逐不落盘**（沿用现状）：reaper 对 runtime 锁 try_lock 失败的会话照旧移除但不落盘，并有 warn 日志；测试 `evict_skips_persistence_for_busy_session` 钉住该行为。
8. **Redis/S3 只打日志不构造**：goal 明确半接线的云存储比不接线更危险；启动日志措辞「recognized but not yet wired in http mode」。

## Tests added

- `src/runtime/builder.rs`: `storage_and_session_store_forward_to_kernel`（Arc::ptr_eq 证明转发到 kernel 的是同一个 Arc）。
- `src/http/mod.rs::goal_396_persistence_tests`:
  - `evict_persists_each_sessions_transcript_outside_the_lock`（两会话各自落盘互不串 + 探针证明锁外）
  - `evict_skips_persistence_for_busy_session`
  - `flush_all_persists_and_drains_every_session`
- `tests/http.rs`:
  - `delete_session_persists_transcript_with_tool_pairing`（建会话 → 1 轮含 tool call → DELETE → `load_transcript` 读回无损 + invariant #8 配对断言）
  - `delete_persists_each_session_transcript_separately`

## Quality gates

- `cargo test --workspace`：**全绿**（exit 0；lib 2297、cli 818、http 100、v050 60 等全部通过，新增 7 个测试全过）。
- `cargo clippy --all-targets --all-features -- -D warnings`：首轮在 `tests/http_common/mod.rs:106` 报 `clippy::manual_map`（fake backend 的探针写法），改为 `as_ref().map(...)` 后**全绿**（exit 0）。
- `cargo fmt --all`：已执行，无 diff 变化。

## 手工实测

命令（journal 要求贴命令与结果）：

```bash
cargo build -p recursive-cli --bin recursive
bash /tmp/goal396-manual-smoke.sh
```

smoke 脚本做的事：临时 `RECURSIVE_HOME` 启动 `recursive http --addr 127.0.0.1:3961`（带假 `RECURSIVE_REDIS_URL`）→ 建会话 → DELETE → 在 `$RECURSIVE_HOME` 下找 `<session-id>.jsonl` → 逐行 parse 成 Message。结果 **SMOKE OK**：

- 启动日志按预期打印 `storage: RECURSIVE_REDIS_URL is set — RedisSessionStore recognized but not yet wired in http mode; using LocalStorageBackend`。
- DELETE 后 transcript 落在 `$RECURSIVE_HOME/workspaces/<hash>/.recursive/sessions/<id>.jsonl`。
- 文件逐行 parse 通过（本例仅 system 消息，未发对话轮）。
- 优雅关停路径的批量落盘由单测 `flush_all_persists_and_drains_every_session` 覆盖。

## e2e 回归

在 worktree 内执行（新 worktree 需先 `cd e2e/plugins && npm install && npm run build`，否则 argus-init 报 `PLUGIN_LOAD_ERROR`——首次运行即因此失败，构建插件后重试成功）：

- `sh .dev/scripts/e2e-run.sh http-api` → **passed 21/21**（覆盖 DELETE/驱逐/completion 等宿主路径回归）。
- `sh .dev/scripts/e2e-run.sh resume` → **failed（0/2：1 failed 1 skipped）——基线即红，非本 goal 引入**：
  - 失败点：resume 轮的 LLM 请求被 aimock 拒绝 `404 no_fixture_match`（fixture `11-session-resume.json` turn-1 的 `userMessage`/`turnIndex`/`hasToolResult` 组合匹配不上）；容器内手工复现确认第一轮 `--max-steps 1` 一切正常，仅 resume 请求 404。
  - **基线证据**：同一命令在**未改动**的 main checkout（9ba806f）上同样 0/2。该套件是纯 CLI `run`/`resume` 路径，本 goal 只动 HTTP 宿主层，无交集。
  - 已提内部 issue（jeffkit/recursive #1，author recursive-agent）跟踪 fixture/aimock 语义漂移的调查与重录。

## Notes

- `AppState` 新字段波及 12 处构造点（lib tests ×5、tests/http.rs ×7、http_common fixtures ×2、agui_e2e ×1、v050 ×1、CLI ×1），全部补齐；fixtures 一律用内存 fake，测试不再碰真实文件系统。
- 坑：`#[path]` 在命名模块内相对**该模块自己的目录**解析——`tests/v050_integration.rs` 内嵌 mod 里 `#[path = "http_common/mod.rs"]` 会找 `tests/v050_integration/http_common/`；把 mod 声明提到文件顶层即可（agui_e2e 即如此）。
