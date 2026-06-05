# Manual edit: fix-banner-flow

**Date**: 2026-06-05
**Goal**: 解决 TUI 空白和 Banner 不上推的问题

## 问题分析

三个根因：
1. **空白**：messages area 内容从顶部渲染，`recent_display` 内容不足时底部留空白
2. **Banner 不上推**：Banner 是 stdout 输出的，在 viewport 外部，不参与信息流，永远不会被消息推动
3. **RECENT_DISPLAY_MAX 太小（50）**：触发 `insert_before` 太慢，与 Banner 不上推无关（因为 Banner 根本不在 viewport 内）

## 解决方案

### 1. Banner 移入 viewport（`src/tui/mod.rs`）

- 新增 `make_viewport_banner(workspace)` 函数，把 Logo/版本/工作区/最近会话渲染为 ratatui `Line<'static>` 列表
- TUI 启动时把 Banner 内容写入 `app.recent_display`，而非 stdout
- 移除 `print_startup_banner()` 调用（及整个函数和辅助函数 `visible_len`、`pad_to`、`compute_column_widths`）
- `RECENT_DISPLAY_MAX` 从 50 → 300（Banner 在 viewport 内，不需要快速触发 `insert_before`）

### 2. Messages area 底部对齐（`src/tui/ui/chat.rs`）

在 `render()` 中，当 `total_rows < visible_rows` 时，在 `lines` 前面预填空白行，使内容贴着 status bar 底部显示。这消除了启动时的空白，Banner 自然出现在输入框正上方。

## 效果

- 启动：Banner 直接贴在 status/input 上方，无空白
- 对话增加：Banner 被新消息自然向上推，最终滚出可视区
- 无重复：每行仍只在一处存在（`recent_display` 或 `insert_before` 的 native scrollback）
- Clippy + 测试：全部通过

### 3. Viewport 改为动态终端高度（`src/tui/mod.rs`）

把写死的 `INLINE_HEIGHT = 40` 改为 `last_size.1.max(MIN_INLINE_HEIGHT)`（实际终端高度）。

原因：写死 40 行时，终端若为 55 行，TUI viewport 只占下方 40 行，上方 15 行露出旧的 shell 历史（cargo run 等输出）。改为全高后：
- ratatui 创建 viewport 时会滚动终端，把旧内容推入 native scrollback
- TUI 占满整个可视区域
- 窗口 resize 时同步更新 viewport 高度

**Files touched**:
- `src/tui/mod.rs`
- `src/tui/ui/chat.rs`

**Tests added**: none（逻辑变更，现有测试覆盖）
