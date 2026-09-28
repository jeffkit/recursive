# Journal — Goal 397 / issue #24: HTTP 会话冷加载（读路径）

- **Date**: 2026-09-27
- **Goal**: `.dev/goals/397-http-session-cold-load.md`（issue jeffkit/recursive#24）
- **Worktree / branch**: `.worktrees/goal-397-http-cold-load` / `feat/goal-397-http-cold-load`
- **Base**: main@9ba806f（Goal 398 已并入；handlers.rs / 测试 fixtures 因此带 admission 字段）

## What landed

重启后从 `StorageBackend` 恢复 HTTP 会话的懒加载读路径：

1. **`src/http/cold_load.rs`（新）**
   - `normalize_stored_transcript(Vec<Message>) -> Option<Vec<Message>>`：剥 `msgs[0]` 的
     stored system 消息（运行时会自建 prompt，`seed_transcript` 插在其后——否则两条
     system）→ 剥开头连续孤儿 `Role::Tool`（invariant #8）→ 剥完为空返回 `None`
     （保持 404，不造幽灵会话）。空判定在规范化**之后**。
   - `get_or_load_session(&Arc<AppState>, id) -> Result<Arc<SessionState>, ApiError>`：
     短读锁查表 → 无锁 `load_transcript().await` → 无锁重建 runtime → 短写锁插入，
     并发双插以先插入者为准（输家丢弃新 runtime）。IO 全程不持表锁。
   - `build_restored_runtime`：与 `create_session` 同款装配（assemble_system_prompt /
     max_steps / wall_timeout_secs），`seed_transcript` + `set_session_id`。
2. **`AppState.storage: Arc<dyn StorageBackend>`（新字段）**：生产环境在
   `crates/recursive-cli/src/main.rs` 用 `LocalStorageBackend::new(user_workspace_dir(
   &config.workspace)?)` 构造——transcript 落在
   `$RECURSIVE_HOME/workspaces/<hash>/.recursive/sessions/<id>.jsonl`。Goal 396 写路径
   可复用同一字段。
3. **接线**：`GET /sessions/:id`、`POST /sessions/:id/messages` 改走
   `get_or_load_session`；`POST /sessions/:id/interrupt` 有意**不**冷加载（只读路径，
   重启后没有进行中的 turn 可取消——见 handler doc comment）。
4. **`SessionState` 加 `#[derive(Clone)]`**：全部可变状态都是 Arc 字段，clone 共享
   runtime/counters/gate——`get_or_load_session` 借此在不持表锁的情况下把手柄交给
   handler（issue 签名要求返回 `Arc<SessionState>`）。
5. **`ApiError` 加 `#[derive(Debug)]`**：测试断言需要（`expect`/`panic!("{e:?}")`）。
6. **测试 fixtures**（`http_common` ×2、`agui_e2e`、`v050_integration`、`tests/http.rs`
   ×7、`handlers.rs` 内联测试 ×5）：补 `storage` 字段，用按进程隔离的临时目录；
   `http_common` 新增 `sample_state_with_storage()`。

## 会话元数据（最小实现，按 issue）

`created_at` 用当前时间合成（`StorageBackend` trait 无 mtime 读取口，不猜文件系统）、
`title = None`，doc comment 写明「会话元数据的持久化不在本 goal」。未做
`save_memory("session-meta/<id>")` 可选项——留给元数据持久化 goal 一并定 key 约定。
`permission_mode` / `max_steps` 覆盖等 per-session 请求参数同样不恢复（从未持久化），
代码注释里写明。

## 依赖状态（重要——本 goal 的两个依赖都还开着）

- **#23 / Goal 396（写路径）未落地**：今天没有任何生产代码调 `save_transcript`，
  所以冷加载在真实部署中暂时不可达（storage 恒空 → 行为与现状完全一致，全部 404）。
  本 goal 的测试按 issue 自己规定的缝（「落盘 → 清空宿主表（模拟重启）」）用
  `LocalStorageBackend::save_transcript` 直接种子。#23 落地后本读路径即刻生效，
  无需返工；`AppState.storage` 字段就是给它的接线点。
- **#21 / Goal 394（fork 隔离）未落地**：`build_restored_runtime` 沿用与
  `create_session` / `fork_session` 完全相同的 `state.tool_registry.clone()` 约定
  （不新增共享面、不更糟），并在代码里留了升级点注释。#394 落地时三处一起换
  `ToolRegistry::fork_session()`。

## Tests added（按名可跑）

- `cargo test --lib cold_load`（8 个，`src/http/cold_load.rs`）：
  `normalize_strips_leading_system_message`、`normalize_strips_leading_orphan_tool_results`、
  `normalize_keeps_valid_tool_pairing_intact`、`normalize_system_then_orphans_is_empty_after_strip`、
  `cold_load_restores_single_system_and_valid_pairing`、`cold_load_empty_storage_keeps_404_and_no_ghost`、
  `cold_load_memory_hit_skips_storage`、`cold_load_continues_conversation_with_paired_growth`
- `cargo test --test http_cold_load`（6 个，新文件 `tests/http_cold_load.rs`）：
  `get_restores_session_after_restart`、`post_continues_restored_conversation_with_legal_pairing`、
  `unknown_id_stays_404_and_creates_no_ghost`、`orphan_tool_prefix_is_stripped_not_400`、
  `system_and_orphans_only_stays_404`、`list_sessions_stays_memory_only`
  （冷加载是懒的：list 只看内存表，访问过的会话才出现——测试 pin 了这一语义。）
- invariant #8 回归：`cargo test --test invariants tool_call_pairing` 12/12 绿。
  （注意：goal 里写的 `cargo test --test tool_call_pairing` 不是合法 target——pairing
  测试在 `invariants` target 下，正确命令如上。）

## Gates

- `cargo test --workspace`：全绿（lib 819+8 通过、0 failed；含新测试后总数见 CI）。
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`：绿
  （修了两处测试代码 `clone_on_copy`：`Role` 是 Copy）。
- `cargo fmt --all`：已跑。

## 手工实测（issue 验收的「重启实测」，因 #23 未落地改为等效种子法）

```bash
# 1. 起 server（隔离 RECURSIVE_HOME，无鉴权 escape hatch 只用于本地测试）
RECURSIVE_HOME=/tmp/recursive-coldload-home.XXX RECURSIVE_API_KEY=test-key \
RECURSIVE_MODEL=test-model RECURSIVE_HTTP_AUTH_INSECURE_OK=1 \
./target/debug/recursive http --addr 127.0.0.1:18471 &
# 2. server 起来后 workspace hash 目录出现（本例 84eaa11b7f13），
#    按 LocalStorageBackend 布局种入「上一进程落盘」的 transcript：
SEED=/tmp/recursive-coldload-home.XXX/workspaces/84eaa11b7f13/.recursive/sessions
printf '%s\n' '{"role":"system","content":"stored system prompt","tool_calls":[],"is_compaction_summary":false}' \
              '{"role":"user","content":"ping from a previous process","tool_calls":[],"is_compaction_summary":false}' \
              '{"role":"assistant","content":"pong, restored","tool_calls":[],"is_compaction_summary":false}' \
  > "$SEED/cold-manual-1.jsonl"
# 3. 冷加载：
curl -s http://127.0.0.1:18471/sessions/cold-manual-1 | jq '{count: (.messages|length), roles: [.messages[].role] | unique, first_prompt, last_prompt}'
#    → messages=3（1 条 runtime 自建 system + 2 条种子），first/last_prompt 均为种子内容
#    → role=="system" 恰好 1 条且内容是运行时 prompt（stored 的被剥掉）✓
# 4. 未知 id 仍 404，且 GET /sessions 不出现幽灵：
curl -s -w '%{http_code}' http://127.0.0.1:18471/sessions/no-such-xyz   # → 404
curl -s http://127.0.0.1:18471/sessions | jq '.total'                   # → 1（仅访问过的）
```

真·端到端版本（建会话 → 发 1 轮 → 重启 → GET）在 #23 落地后即可跑通，命令不变，
只是第 2 步的种子文件会由 server 自己写出来。

## e2e 回归

- `sh .dev/scripts/e2e-run.sh http-api`、`sh .dev/scripts/e2e-run.sh resume`
  （注意：issue 命令里的 `08-http-api` / `11-session-resume` 是**文件名**，filter 认
  e2e.yaml 的 suite `id`——`http-api` / `resume`；用文件名会 SUITE_NOT_FOUND，见
  CLAUDE.md e2e 规则。）结果附于 issue 回复。

## Notes / traps hit

- Goal 398 在我实现期间并入了 main（`AppState` 新增 `admission`、fixtures 变动）——
  本 worktree 从新 main 起（9ba806f），无冲突。
- `enqueue()` 返回 `Option<RuntimeOutcome>`，测试里无需解包即可断言 transcript 增长。
- 冷加载是**每 miss 一次**的惰性重建，不在热路径上：命中内存零开销；未命中且
  storage 为空 = 一次 `path.exists()` + 404，与现状语义一致。
- 并发双插的输家会白建一个 runtime——量级为单会话构建成本，issue 明确「以先插入者
  为准」即可，不做 per-id 单飞锁。

## 续跑备注（2026-09-28，issue #24 评论后）

- Acceptance 的 e2e suite id 已更正为 `http-api` / `resume`（goal 文件已改，与 PR #35
  分支的 28362a0 同文本，rebase 无冲突；`11-session-resume.yaml` 的 id 是 `resume`，
  不是 `session-resume`）。
- 上一轮 e2e 回归被中断，未出结果；stranded 的 `wt-9ba806f-aimock`、空的
  `argusai-*` 网络与本 worktree 的 `e2e/.argusai` 已清理。

## e2e 回归结果（2026-09-28 补跑，按更正后的 suite id）

- `sh .dev/scripts/e2e-run.sh http-api` → **passed 21/21**（含本 goal WIP 的镜像重build：
  WIP 源码 21:54 晚于旧镜像 21:06，未用 `--no-build`）。
- `sh .dev/scripts/e2e-run.sh resume` → **failed（1 failed / 1 skipped）**，
  判定为**既有失败、与本 goal 无关**：
  - 判别实验：在无本 WIP 的主 checkout（4e14b54）跑同一 suite，签名完全一致
    （`Resumed run produced a valid completed session` → session status "crashed"）。
  - 根因（容器内手工复现，stderr 直捕）：resume 轮的 LLM 调用被 aimock 拒——
    `HTTP 404 {"message":"No fixture matched","code":"no_fixture_match"}`；
    transcript 终止于 `Continue from where you left off.`（user）之后，无 assistant
    消息、无 token 消耗。即 resume 后的请求形状（4 条 seeded + continue）与
    `turnIndex`/`hasToolResult` fixture 条件不匹配，属 fixture 与 resume 形状漂移。
  - 本 goal 明确不动 `src/session/**` / CLI resume 路径（Files NOT to touch），
    该 suite 失败不阻塞本 goal；已单开 issue jeffkit/recursive#38 跟踪修复。
- 质量门复验（当前 WIP 状态）：`cargo fmt --all --check` ✓、
  `cargo clippy --workspace --all-targets --all-features -- -D warnings` ✓、
  `cargo test --workspace` 全绿 0 failed。
