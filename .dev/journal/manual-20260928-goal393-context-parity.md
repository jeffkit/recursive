# Journal — manual-20260928-goal393-context-parity

- **Date**: 2026-09-28
- **Goal**: [393] HTTP 会话上下文管理对齐 CLI（compactor / microcompactor / transcript cap），issue #20
- **Branch / Worktree**: `feat/goal-393-context-parity` @ `.worktrees/goal-393-context-parity`（base: main `4e14b54`）

## Files touched

| File | Change |
|------|--------|
| `src/runtime/context_management.rs` | **新增**。前端无关装配 helper `apply_context_management(builder, &Config) -> AgentRuntimeBuilder`：compactor（`RECURSIVE_COMPACT_THRESHOLD`，`0/off/false`=禁用、非法值=禁用、未设=按模型上下文自动）+ token 阈值 + microcompactor（`build_microcompactor_from_env`，opt-in）+ transcript cap（`RECURSIVE_MAX_TRANSCRIPT_CHARS`，未设=无上限）。附合并式 env 矩阵单测 |
| `src/runtime.rs` | `mod context_management;` + `pub use …apply_context_management;`（+5 行，runtime.rs 1476→1481，预算内） |
| `src/runtime/builder.rs` | 3 个 `#[cfg(test)]` 读访问器（compactor / microcompactor / max_transcript_chars），goal 授权的 builder 层断言方式 |
| `src/kernel.rs` | `#[cfg(test)] max_transcript_chars_for_test()`（cap 存在 kernel_builder 里，runtime builder 无自己的字段） |
| `crates/recursive-cli/src/cli/builder.rs` | 删除内联的 compactor/microcompactor 装配块（旧 :480-517），改调共享 helper；`--max-transcript-chars` flag 在 helper 之后应用，保持 flag > env 优先级 |
| `src/http/handlers.rs` | 新增私有 `build_session_runtime(state, …)`；四处构建点全部接入：`POST /run`（旧 :180）、`create_session`（旧 :305）、session fork（旧 :617，goal 里说的「第四处」）、`/agui`（旧 :1711，保留 seed_transcript）。修正 `create_session` 的「auto-saved to storage」失实注释 |
| `src/http/mod.rs` | 修正 reaper 的「transcript is saved to the storage backend」失实注释（当前 close() 只发 SessionEnd hook；持久化是 Goal 396） |
| `tests/http_context_parity.rs` | **新增**独立集成测试二进制：HTTP 会话跨轮压缩并继续对话的端到端行为 |
| `README.md` | HTTP 环境变量表补 3 行（goal 授权的最小改动）：compaction / microcompaction / transcript cap 变量现已对 HTTP 会话生效 |

## 装配前后对比（消除漂移的核对表）

| 装配项 | CLI 之前 | CLI 之后 | HTTP 之前 | HTTP 之后 |
|--------|---------|---------|-----------|-----------|
| compactor（auto/显式/禁用） | builder.rs 内联 | helper | **无**（溢出即致命错误） | helper，逐项同 CLI |
| `threshold_prompt_tokens` | 内联派生 | helper | **无** | helper |
| microcompactor | 内联 `build_microcompactor_from_env` | helper | **无** | helper（opt-in，默认关） |
| transcript cap | clap flag/env | helper(env) + flag 覆盖 | **无（无界）** | helper(env)，未设仍无界 |
| reinjector（file/skill） | 有 | 保留在 CLI（会话级 read_state，**待 Goal 394**） | 无 | 不接，同左 |
| event sink / hooks / streaming | CLI 特有 | 保留在 CLI | HTTP 特有（SSE） | 不变 |
| wall_timeout_secs | 有（Goal 399） | 不变 | 有 | 收敛进 `build_session_runtime` |

注：microcompactor 实际是**默认关闭**（`build_microcompactor_from_env` 未设返回 None；goal 文里「默认 12」是旧行为，micro.rs 的 doc 早已改为 opt-in）。helper 委托同一函数，不引入新的语义。

## Tests added

1. `src/runtime/context_management.rs::context_management_env_matrix` — 未设/`=0/off/false`/`=N` × compactor、cap set/禁用/非法、microcompactor 显式开/关，**合并为一个测试**（env 进程级，`env_lock` + `PinnedRecursiveHomeNoLock`，模式同 `config.rs::shell_timeout_default_and_env_override`）。
2. `src/http/handlers.rs::build_session_runtime_installs_compactor_and_transcript_cap` — HTTP 构建点确实装上了 compactor 与 cap（builder 层断言，`AgentRuntime` 无公开访问器，未为此动 runtime.rs 预算）。
3. `tests/http_context_parity.rs::http_session_compacts_cross_turn_and_continues` — `RECURSIVE_COMPACT_THRESHOLD=10` 的 HTTP 会话：第 5 轮后 transcript 头部变成压缩摘要（marker A）、第 6 轮照常对话并触发第二次压缩（marker B）、transcript 长度有界。**独立二进制**的原因：env 是进程级，不能与 `tests/http.rs` 的 ~100 个测试同进程。

## Gates

- `cargo test --workspace` ✅（3491 passed / 0 failed，含 doc-tests）
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` ✅（中途抓到一次 `await_holding_lock`：集成测试不能跨 `.await` 持 env 锁，已改为 sync 作用域内 set/restore——该二进制只有这一个测试，异步段无人竞争）
- `cargo fmt --all --check` ✅
- e2e（replay，无 key）✅：`sh .dev/scripts/e2e-run.sh http-api` → **21/21 passed**；
  `sh .dev/scripts/e2e-run.sh compaction` → **2/2 passed**。首次运行未加 `--no-build`
  （HEAD 移动后镜像 tag 失效规则）。

## New failure mode found (候选记入 AGENTS.md)

7'. **Fresh worktree 缺 `e2e/plugins/dist` → argus-init `PLUGIN_LOAD_ERROR`（exit 5）。**
   `.worktrees/<name>` 是新 checkout，`e2e/plugins/dist/index.js` 是 gitignore 的编译产物，
   首次在该 worktree 跑 e2e 时 argus-init 直接失败，e2e-run.sh 吞成一句
   `[e2e-run] argus-init failed`。Remedy：
   `cd e2e/plugins && npm ci && npm run build`，再重跑。与失败模式 4/5/6 无关，
   三者都排不掉这个错。

## Notes

- **e2e suite id 更正已采纳**（okguitar 在 #20 的评论 + 28362a0）：验收用 `http-api` / `compaction`，不是文件名前缀。goal 文件本身的 id 修正已随 `integration/goal-392-405`（PR #35）落地，本分支不重复 patch。
- **GitNexus**：本会话未挂载 GitNexus MCP 工具，无法跑 `gitnexus_impact` / `gitnexus_detect_changes`。手工 blast radius：`apply_context_management` 为新符号；改动的既有符号 = CLI `build_runtime`（调用方：cli main / acp）、HTTP `run_agent` / `create_session` / `fork_session` / `agui_run`（调用方：对应路由 handler）；不动 `run_core.rs` / `kernel.rs` 生产路径 / `compact/**` 算法 / finish reason 语义（invariant #1/#7 不触及）；transcript 变更只经既有 `Compactor::apply_to_transcript`（invariant #8 由其既有 safe_split_point 保证）。风险级别：低（装配等价搬移 + 新增调用）。
- TUI（`crates/recursive-tui/src/runtime_builder.rs`）有自己的 compactor 装配契约（`n=` env），goal scope 未含 TUI，保持不动——下一个去漂移的候选。
- Journal 文件名用今天的日期（20260928），goal 验收里写的是执行日 20260927，内容同一。
