# #154 — headless 审批 hook 失败即拒绝；permissionDecision 被消费

- **Date**: 2026-10-10
- **Goal**: issue #154（P0，簇 #187「权限与工具身份」）
- **Files touched**:
  - `src/hooks/config.rs` — `HookCommand.fail_mode: Option<HookFailMode>`（`None` = 按事件取默认）
  - `src/hooks/external.rs` — `HookEvent::is_approval/default_fail_mode`；`into_hook_result(event)`
    对审批事件缺 `action` 不再默认 `Continue`；`run_hook` 按事件解析 fail_mode 并对显式
    `fail_mode="open"` 记 warning；prompt-hook 无 LLM 走 fail_mode 而非硬编码 continue；
    `dispatch` 在 `permission_decision.is_some()` 时短路，避免决策被后续 hook 覆盖
  - `src/tools/policy_domain/permission_pipeline.rs` — Phase 6 消费 `permission_decision`
    （Deny → 拒；Allow → 放行；否则按 action），拒绝时带 hook 的 `permissionDecisionReason`
  - `src/tools/mod.rs` — headless 端到端审批测试
- **Tests added**:
  - `src/hooks/config.rs`: `hook_command_explicit_open_is_preserved`；`hook_command_defaults`
    改为断言 `None`
  - `src/hooks/external.rs`: `default_fail_mode_is_closed_for_approval_events_only`、
    `approval_event_missing_action_does_not_continue`、`approval_event_empty_output_does_not_continue`、
    `notification_event_missing_action_continues`、
    `dispatch_returns_permission_decision_even_when_action_continues`；
    改写 `dispatch_treats_timeout_as_continue` / `dispatch_treats_bad_output_as_continue` →
    `..._as_closed_on_approval_event`，新增 `..._as_continue_on_notification_event`；
    `no_llm_prompt_hook_returns_continue` → `no_llm_prompt_hook_fails_closed_on_approval_event`
  - `src/tools/mod.rs`: `headless_permission_decision_deny_blocks_without_action`、
    `headless_permission_decision_allow_proceeds`、`headless_hook_missing_action_is_denied`、
    `headless_hook_timeout_fails_closed`、`headless_hook_nonzero_exit_fails_closed`、
    `headless_hook_malformed_json_fails_closed`
- **Notes**: 只在审批事件（`PermissionRequest` / `PreToolCall`）上收紧；`UserPromptSubmit`
  等通知类事件保持 fail-open。显式 `fail_mode = "open"` 仍被尊重（保留兼容），此时记 warning。
  未改 hook 发现路径与超时值（#88 范围）。
