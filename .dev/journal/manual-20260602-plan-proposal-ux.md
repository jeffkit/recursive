# Manual edit: plan-proposal-ux

**Date**: 2026-06-02
**Goal**: Fix plan proposal UI/UX — dark mode visibility, modal not closing, inline display
**Files touched**:
- `src/tui/ui/modal.rs` — Fix 1: render_plan_review hint 颜色 dim→Cyan/Green/Red (暗黑模式可见)
- `src/tui/app.rs` — Fix 2: handle_plan_review_key 乐观关闭 (y 立即 pop modal); Fix 3: PlanProposed 改用内联 TranscriptBlock::PlanProposal 而非弹窗; 新增 TranscriptBlock::PlanProposal 变体; 更新相关测试
- `src/tui/ui/transcript.rs` — Fix 3: 新增 render_plan_proposal() 渲染内联带边框的计划块 + plan_args_preview 辅助函数
- `src/tui/ui/chat.rs` — Fix 4: 布局增加 1 行 plan approval banner (plan_awaiting_approval=true 时可见); render_plan_approval_banner() 渲染黄底高对比度行动提示

**Tests added**: 更新了 plan_proposed_event_opens_plan_review_modal 和 plan_review_y_dispatches_confirm_plan_action 两个测试以反映新行为

**Notes**:
- Plan 现在在主消息流中显示为带 ╔ 边框的内联块，不再弹窗
- 底部批准栏高对比度：黄底黑字 + 绿色 Approve / 红色 Reject
- 乐观关闭解决了"按 y 弹窗不消失"的问题 (原本需要等待 PlanConfirmed 事件往返)
- "先询问用户是否进入 plan 模式"属于 LLM 行为调整，非 TUI 变更，不在本次范围内
