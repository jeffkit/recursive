# Manual edit: quality-fixes-r6

**Date**: 2026-06-22
**Goal**: 补充第6轮代码审查发现的缺失测试，并修复可移除的 clippy suppress 注解
**Files touched**:
- `src/compact.rs` — 新增3个 `safe_split_point` 测试
- `src/run_core.rs` — 新增3个 `attach_reasoning_content` 测试 + `make_test_core` 辅助函数
- `src/tools/plan_mode.rs` — 新增2个 `begin_approval` 幂等性测试
- `src/tools/web_fetch.rs` — 移除 `#[allow(clippy::new_without_default)]`，添加 `impl Default`
- `src/tools/web_search.rs` — 移除 `#[allow(clippy::new_without_default)]`，添加 `impl Default`

**Tests added**:
- `compact::tests::safe_split_backs_up_past_tool_and_assistant_with_tool_calls` — 验证退让逻辑穿过 Tool → Asst+tool_calls → 停在 index 0
- `compact::tests::safe_split_backs_up_when_landing_directly_on_assistant_with_tool_calls` — 验证直接落在 Asst+tool_calls 时退让
- `compact::tests::safe_split_no_backup_when_split_is_already_valid` — 验证落在 User 消息时不退让
- `run_core::tests::attach_reasoning_content_sets_last_message_when_some` — Some 时正确附加到最后消息
- `run_core::tests::attach_reasoning_content_does_not_modify_when_none` — None 时不修改消息
- `run_core::tests::attach_reasoning_content_preserves_existing_content_when_none` — None 时不覆盖已有 reasoning_content
- `plan_mode::tests::begin_approval_sets_plan_and_clears_stale_response` — 验证清除旧 response 的 TOCTOU 不变式
- `plan_mode::tests::begin_approval_called_twice_no_ghost_approval` — 验证连续调用两次不留幽灵审批

**Notes**:
- `attach_reasoning_content` 是 `pub(self)` 私有方法，通过同文件内的 `make_test_core` 辅助函数构造最小 RunCore 来测试
- `WebFetch`/`WebSearch` 的 `new()` 包含构造期 expect（TLS 后端），`Default::default()` 直接委托 `Self::new()` 保持语义一致
- `cargo clippy --all-targets --all-features -- -D warnings` 零告警；`cargo test --workspace` 全部通过
