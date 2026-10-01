# Manual: `recursive http` 忽略 RECURSIVE_ALLOW_TOOLS（#69 / #65）

- Date: 2026-10-06
- Goal: 让 `--allow-tools` / `RECURSIVE_ALLOW_TOOLS` 在 `recursive http` 上生效（收窄工具表）。
- Files touched:
  - `crates/recursive-cli/src/main.rs`（Cmd::Http 分支，register_subagent_if_enabled 之后）
  - `src/http/mod.rs`（goal_403_http_sandbox_entry 模块新增源码级回归测试）
- Root cause: `Cmd::Loop` 分支在 `build_tools` 后调用 `tools.retain_tools(&config.allow_tools)`（main.rs:2089），而 `Cmd::Http` 分支从未调用，导致启动 registry（进而 `/tools`、`/run`、`/sessions/*` 的 `session_tool_registry()`）不受收窄。
- Fix: 在 HTTP 分支注册 subagent 工具之后执行同一收窄（放最后，避免 subagent `agent` 工具先被裁掉/留下不一致状态；与 CLI 顺序一致：先注册、再裁剪）。
- Tests added: `http_entry_applies_allow_tools_narrowing`（src/http/mod.rs，源码级断言 HTTP 分支包含 retain_tools 调用）。
- Verification:
  - 手动复现：`RECURSIVE_ALLOW_TOOLS=Skill,HttpCall recursive http` → `GET /tools` 返回仅 `Skill`（HttpCall 尚未实现，忽略，正确）。
  - `cargo test -p recursive-cli` 125 passed；`cargo test -p recursive-agent --lib` 全绿。
  - `cargo test --workspace` 中 `tools::agent::tests::execute_single_wall_timeout_returns` 一次失败为 wall-time 计时型 flake（stash 掉本改动后单跑也通过；改动后单跑也通过），与本次改动无关。
  - `cargo clippy --all-targets --all-features -- -D warnings` 干净；`cargo fmt --all` 已跑。
- Notes: 容器 tier 的 per-session registry 重建（rebind_per_session_registry）从 startup registry 继承权限/配置，但重建走 ContainerToolSetProvider 全量 registry —— 本次修复收窄的是 AppState.tool_registry（非容器 tier 共享句柄 + tool_infos）。容器 tier 下 per-session 重建是否会绕过收窄属 #63/容器议题，未在本次处理。
