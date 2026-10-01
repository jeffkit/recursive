# Review-fix: NEEDS_FIX 处置——pipeline-69 残留分支 rebase 到 main + 契约对齐

## Date
2026-10-06

## Goal
独立评审对 `v2-pipeline-69-1001145107-cont`（vs main）给出 NEEDS_FIX：diff 全部是
对 main 新进 8 个提交的幽灵回退（AGENTS.md 已知失败模式 #2），分支自身唯一的
增量（#69 的 `RECURSIVE_ALLOW_TOOLS` env 读取）与 clap env 注入冗余、且源码级
测试断言的字面量在 main 的 choke-point 设计下必然失败。本轮把分支落到
"rebase 到最新 main + 最小重放"的形态，逐条消除评审问题。

## 处置（对照评审条目）
1. **回退 #63 / #70/#65 / 流文件 / journal**：`git rebase main`（合并基
   `bc63d74` → main 顶）。rebase 后 `git diff main` 只剩分支自身增量，无任何
   `src/tools/http_call.rs` / `finish_tool_surface` / flows / 测试 harness 的
   删除。原分支 tip 备份在 `refs/backup/pipeline-69-1001145107-cont-pre-rebase`
   （669fcd6）。
2. **loop 通道 sub-agent 逃逸 / TodoWrite 回灌 / HTTP 无 MCP**：全部随 rebase
   消失——main 的 `finish_tool_surface` + `apply_operator_allow_list`（注册
   sub-agent 之后的最后一步）与 `session_tool_registry` choke point 原样保留。
3. **env 读取与 clap 注入的冗余**：保留 `Config::from_env` 的
   `RECURSIVE_ALLOW_TOOLS` 读取（服务非 clap 嵌入方：TUI
   `config_for_preset_model`、HTTP session 重建走 `Config`，不经 clap），
   注释写明 flag 仍胜出；main.rs 的 `--allow-tools` 归并改为"有值即重切分
   （trim + 丢空段）"，env-only 与 flag-only 解析形状一致，消除
   `Some("")` 边界差异。
4. **源码级测试对不上 main 机制**：删除断言字面 `tools.retain_tools(...)` 的
   `http_entry_applies_allow_tools_narrowing`；替换为
   - `config_from_env_reads_allow_tools`（断言 `Config::from_env` 读 env var）；
   - `session_rebind_reapplies_allow_tools_in_container_tier`（断言
     `session_tool_registry` 重放 allow-list，且 `rebind_per_session_registry`
     保持纯重建、不重复过滤）。
   分支 WIP 曾把重放塞进 `rebind_per_session_registry`，与 main 的
   choke-point 重复——按 main 设计移除（`session_tool_registry` 是唯一收口，
   已有 `session_tool_registry_applies_allow_tools` 行为级测试覆盖）。
5. 顺手修 main `63e5b05` 落地时带进来的 `http_call.rs` import 顺序
   （`cargo fmt --all -- --check` 在 main 上本来就红，本分支补一刀）。

## Files touched
- `crates/recursive-cli/src/main.rs` — `--allow-tools` 归并逻辑统一重切分。
- `src/config.rs` — env 读取保留 + 意图注释；`allow_tools_from_env` 测试保留。
- `src/http/mod.rs` — 两个源码级回归测试（替换原字面量断言）。
- `src/tools/http_call.rs` — 仅 rustfmt import 顺序。
- `.dev/journal/manual-20261006-http-allow-tools.md` — 追加 rebase 修订说明。

## Tests added
`config_from_env_reads_allow_tools`、
`session_rebind_reapplies_allow_tools_in_container_tier`（src/http/mod.rs，
goal_403_http_sandbox_entry 模块）；保留 `config::tests::allow_tools_from_env`。

## Verification（rebase 后最终分支实测）
- `cargo test --workspace`：3804 passed / 0 failed（含 invariants 42 通过——
  分层不变量 #2 亦绿）。
- `cargo test -p recursive-agent --lib`：2437 passed / 0 failed。
- `cargo clippy --all-targets --all-features -- -D warnings`：干净。
- `cargo fmt --all -- --check`：干净。
- `git diff main --stat`：仅 7 个文件、+229/−5，全部为分支自身增量（3 journal
  + 3 源文件 + 1 fmt 行），零 main 工作回退。

## Notes
- main 在本轮处理中前进了 4 次（23189cd → c526280 → 97246d5），每次都重新
  rebase 后再跑门禁；最终分支基于 97246d5。
- 若 main 再次前进，重放这些增量只需：rebase + 确认
  `git diff main` 仍限于上述 7 文件。
