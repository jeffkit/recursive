# Issue #69 — RECURSIVE_ALLOW_TOOLS 验证：分支残留 vs origin/main 终版

## Date
2026-10-01

## Goal
Issue #69（#65 同题重述）报告 `recursive http` 上 `RECURSIVE_ALLOW_TOOLS` 未生效
（`GET /tools` 返回 26 个工具）。核查目标：确认该问题在 origin/main 上的实际状态，
并对当前 pipeline-69 分支的残留代码给出处置结论。

## 根因与终版修复（已在 origin/main 落地）
- 根因有两层：(1) `Cmd::Http` 分支从不调用 `retain_tools`；(2) 更隐蔽的一层是
  sub-agent 工具（`agent`/`send_message`/`list_workers`）在所有 channel 的裁剪
  **之后**注册，逃过 allow-list。
- 终版修复 = origin/main `b3f54fe`（#70/#65 共修）：`finish_tool_surface` 统一
  横切装配尾（MCP + touched-files + coordinator 裁剪），`apply_operator_allow_list`
  作为 sub-agent 注册后的**最后一步**；`session_tool_registry()` 在 per-session
  rebind 后重放 allow-list + coordinator 裁剪；`retain_tools` 打
  `surface_filtered` 标记，`AgentRuntimeBuilder::build` 与 `set_event_sink`
  据此不再回灌被过滤的 `TodoWrite` / `exit_plan_mode`。

## 本分支（v2-pipeline-69-1001145107-cont @ 6078e36）的残留
三个 WIP commit（8c8ff99 / 36de62a / 6078e36，均 terminal engine_error）各自做了
一部分、互有交叠且弱于 main：
- 6078e36：`Config::from_env` 直接读 `RECURSIVE_ALLOW_TOOLS`——与 CLI 的
  `#[arg(env = "RECURSIVE_ALLOW_TOOLS")]` 冗余（clap 已注入 env），且 main 上
  刻意**没有**这层双读。
- 8c8ff99 + 36de62a：HTTP 分支 `retain_tools` + 容器 tier rebind 重放——均被
  main 的 `finish_tool_surface`/`apply_operator_allow_list`/choke-point 版本
  覆盖且更完整（main 还修了 sub-agent 逃逸、TodoWrite 回灌）。
- `git diff HEAD origin/main` 显示这些增量在 main 上全部以更强形式存在；
  本分支无任何 main 缺失的改动。#69 描述的复现（26 个工具）在 main 的
  `b3f54fe` 之后不可能复现。

## 验证（本分支工作树上实测 + 全门禁）
- 实测复现 issue 原始命令：`RECURSIVE_ALLOW_TOOLS=Skill,HttpCall …
  target/debug/recursive http --addr 127.0.0.1:8791` →
  `GET /tools` 返回 **1 个工具（Skill）**，与 issue 期望一致
  （HttpCall 未实现属 #63，main `23189cd` 已补）。26→1，不再复现。
- 定向测试：`config::tests::allow_tools_from_env`、
  `http::goal_403_http_sandbox_entry::*`（5）、`http::handlers`/`http::` 85、
  `runtime::builder` 全绿；`cargo test -p recursive-cli` 125 passed。
- `cargo test --workspace`：57 个 test target 全部 `0 failed`。
- `cargo clippy --all-targets --all-features -- -D warnings`：干净（21m40s）。
- `cargo fmt --all -- --check`：干净。

## 处置结论
- **代码层面无需再改**：#69 的修复已由 main `b3f54fe`（+#63 的 `23189cd`）完整
  落地；本分支是三条失败 pipeline 残留的重复/弱化实现，不应再合并。
- 建议动作：以本 journal + 验证结论关闭 #69（及 #65，若未关）；清理
  pipeline-69 的三个 WIP 分支/快照（`v2-pipeline-69-*` / `pipeline-69-*`），
  避免后续误合并（AGENTS.md 已知失败模式 #2：跨 PR 落地会产生幽灵删除，
  本分支与 main 的 diff 中确有对 `src/tools/web_fetch.rs` 等文件的回退痕迹）。

## Files touched
仅本 journal；无源码改动。

## Tests added
无（验证型条目；main 上已有 8 个 #65 相关测试：
`finish_tool_surface_registers_mcp_then_applies_allow_list` /
`http_entry_applies_the_shared_tool_surface_tail` /
`session_tool_registry_applies_allow_tools` /
`build_does_not_reinject_a_filtered_todo_write` /
`retain_tools_marks_surface_filtered` /
`set_event_sink_respects_a_filtered_todo_write` /
`set_event_sink_respects_a_filtered_exit_plan_mode` 等）。
