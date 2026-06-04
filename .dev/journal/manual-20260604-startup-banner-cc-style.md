# Manual edit: startup-banner-cc-style

**Date**: 2026-06-04
**Goal**: 将 TUI 启动画面改为 Claude Code 风格的两栏布局
**Files touched**: `src/tui/mod.rs`
**Tests added**: none（纯渲染函数，依赖既有 cargo test 全绿）
**Notes**:
- 左栏：3 行 ASCII logo + 版本·模型 + workspace 路径（~ 折叠）
- 右栏：Recent sessions 标题 + 最近 5 条 TUI 会话（有 last_prompt 的）
- 新增 `visible_len`（ANSI 码剥除 + unicode-width）、`pad_to` 两个纯函数
- 改用 `list_sessions_sorted_by_updated_at` 取替旧的 `list_sessions().reverse()`
- left_col 用 `.clamp(36, 52)` 避免 clippy manual_clamp 警告
- cargo test --features tui 全绿，clippy --all-features -D warnings 无告警
