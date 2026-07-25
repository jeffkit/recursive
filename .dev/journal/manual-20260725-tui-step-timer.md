# Manual edit: tui-step-timer

**Date**: 2026-07-25
**Goal**: 修复 TUI 中累积时间显示问题，使每个步骤显示自己的时间而不是从回合开始累积的时间
**Files touched**:
- `crates/recursive-tui/src/cost.rs`
- `crates/recursive-tui/src/app/event_loop.rs`
- `crates/recursive-tui/src/ui/chat.rs`

**Tests added**: 2 new tests in `cost::tests`:
- `turn_state_finish_clears_running_flag` (updated to verify `step_started_at` field)
- `start_step_resets_step_timer` (verifies step timer resets independently)

**Changes**:

1. **TurnState 结构** (cost.rs):
   - 添加 `step_started_at: Option<Instant>` 字段用于跟踪当前步骤的开始时间
   - 在 `start()` 方法中初始化 `step_started_at` 
   - 在 `finish()` 方法中清空 `step_started_at`
   - 添加新的 `start_step()` 方法来重置步骤计时器

2. **步骤切换时重置计时器** (event_loop.rs):
   - 在更新 `spinner_verb` 时调用 `self.turn.start_step()` 重置步骤计时器
   - 这样每个工具调用、思考等步骤都有独立的时间显示

3. **显示步骤时间而非总时间** (chat.rs):
   - 修改 spinner 显示逻辑，使用 `step_started_at` 而不是 `started_at`
   - 这样用户看到的是当前步骤的耗时，而不是从回合开始的累积时间

**Notes**: 
- 现在状态栏仍然显示回合的总时间（使用 `started_at`），而具体步骤（如 Thinking、Editing、Coding tool）显示各自的时间（使用 `step_started_at`）
- 所有测试通过（756 tests），clippy 无警告，格式正确