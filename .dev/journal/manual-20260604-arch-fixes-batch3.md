# Manual edit: arch-fixes-batch3

**Date**: 2026-06-04
**Goal**: 完成架构审查 Batch 3 修复（Issue #1, #26/#38, #2）
**Files touched**:
- `src/kernel.rs` — 删除 SideEffect 枚举及死代码
- `src/lib.rs` — 移除 SideEffect re-export
- `src/event.rs` — AgentEvent::ToolResult 新增 is_error: bool 字段
- `src/run_core.rs` — 使用 is_error 替换 "ERROR: " 字符串前缀检测；定义 DENIAL_LIMIT_SENTINEL 常量
- `src/tui/backend.rs` — 消费 is_error 替代字符串 prefix check
- `src/http/handlers.rs` — 消费 is_error 替代字符串 prefix check
- `src/permissions/mod.rs` — 新增 PermissionMode::Strict 变体
- `src/tools/mod.rs` — Strict 模式拒绝未知工具；非 Strict 模式下对隐式放行发出 tracing::warn!
- `tests/http.rs`, `tests/integration.rs`, `src/tui/backend.rs`, `src/event.rs`, `src/http/handlers.rs` — 更新测试以包含 is_error 字段
**Tests added**: 已有测试覆盖；更新了 map_tool_result_error_prefix_marks_failure 以反映 is_error 语义
**Notes**:
- Issue #1: SideEffect 枚举已无消费者，完全删除，无行为变更
- Issue #26/#38: is_error flag 集中了错误检测逻辑，消除了 TUI/HTTP 中 brittle 的字符串前缀匹配
- Issue #2: Strict 模式向后兼容（Default 行为不变），新增 warn! 日志使隐式放行在 logs 中可见
