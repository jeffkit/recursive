# Manual edit: fix-duplicate-display

**Date**: 2026-06-04
**Goal**: 修复 TUI 对话内容重复显示问题 + `/clear` 清空后旧内容残留问题
**Files touched**:
- `src/tui/mod.rs` — 移除将 flushed blocks 写入 `recent_display` 的逻辑
- `src/tui/app/state.rs` — `reset_transcript` 中补充重置 `last_printed_idx`、`recent_display`、`print_queue`

**Tests added**: none (已有全量测试覆盖，全部通过)

**Root cause（重复显示）**:
每当一个 turn 完成，`flush_ready_blocks` 会把 block 的渲染行推入 `print_queue`。
主循环随后做了两件事：
1. 将这些行 `extend` 进 `app.recent_display`（viewport 内展示）
2. 调用 `terminal.insert_before(...)` 将同样的行推进 native scrollback（viewport 上方）

当终端窗口足够高，能同时显示 scrollback 区域和 40 行 inline viewport 时，
同一段对话内容就同时出现在两个区域，用户看到内容"重复了"。

**Fix（重复显示）**:
移除 `mod.rs` 中把 queued batches 写入 `recent_display` 的循环。
Viewport 只渲染 in-flight（streaming 中）的内容；已完成的 turn 只存在于 native scrollback。
这与 Anthropic Claude Code 的设计一致。

**Root cause（/clear 失效）**:
`reset_transcript` 清空了 `blocks` 并推入新的 System block，
但没有重置 `last_printed_idx`（仍指向旧的、更大的索引值）。
下次 `flush_ready_blocks` 运行时 `last_printed_idx >= blocks.len()`，
新的 System block 永远无法被 flush，用户看不到"Conversation cleared."提示；
同时 `recent_display` 保留旧内容继续展示。

**Fix（/clear）**:
在 `reset_transcript` 中额外执行：
- `self.last_printed_idx = 0`
- `self.recent_display.clear()`
- `self.print_queue.clear()`
