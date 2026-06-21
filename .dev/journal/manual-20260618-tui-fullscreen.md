# Manual edit: tui-fullscreen

**Date**: 2026-06-18
**Goal**: 把 TUI 从 "inline 全高假全屏 + logo/最近会话压在输入框上方" 改造成真正的全屏体验。
用户反馈旧设计为追求 claude-code 观感引入大量界面体验问题（启动一屏空白、banner
与 transcript 共用滚动面导致反复 scroll/render bug、最近会话长期占据正文区）。

**改动**：
- `src/tui/mod.rs`：viewport 从 `Viewport::Inline(终端全高)` 改为 alternate-screen
  全屏（`EnterAlternateScreen` + 默认 `Terminal::new` = `Viewport::Fullscreen`）。
  删除 `make_inline_terminal`、`MIN_INLINE_HEIGHT`、`make_viewport_banner`、resize
  重建逻辑、`last_modal_count` pre-draw sentinel hack（注释还在讲早已删除的
  `insert_before`）。`RawModeGuard` drop 时 `LeaveAlternateScreen` 还原终端。
- `src/tui/ui/chat.rs`：移除 `recent_display`（banner）拼接与底对齐 padding，transcript
  改顶对齐（内容从顶部往下生长，输入框固定底部，`scroll_offset==0` 仍吸底）。新增
  `render_empty_state`：blocks 为空时画居中的 logo wordmark + `vX · model` + 提示行
  （不含会话列表）。
- `src/tui/ui/status.rs`：状态栏并入 `v<version>` 与缩写后的工作区路径（`~/...`），
  即原 banner 头部的身份信息。新增 `abbreviate_workspace` + 2 个单测。
- 最近会话：保留既有 `/resume` 选择器（`cmd_resume` → `UserAction::ResumeSession`），
  仅从启动 banner 移除会话列表——正文区不再常驻会话噪声。

**死代码清理**（架构评审 NEW-TUI-2/3 标记的 P0）：
- 删 `App::flush_ready_blocks`（event_loop.rs）及其唯一消费的 `render_block` import。
- 删 `App` 字段 `last_printed_idx` / `print_queue` / `recent_display`（mod.rs）及
  `App::new` 初始化、`reset_transcript` 中的清理调用（state.rs）。
  这三者自 `insert_before` 被移除后已是 dead state（只写不读）。

**Files touched**: `src/tui/mod.rs`, `src/tui/ui/chat.rs`, `src/tui/ui/status.rs`,
`src/tui/ui/mod.rs`（doc）, `src/tui/app/mod.rs`, `src/tui/app/state.rs`,
`src/tui/app/event_loop.rs`
**Tests added**: `status::tests::status_bar_includes_version_and_workspace`,
`status::tests::abbreviate_workspace_replaces_home_prefix`
**质量门**: `cargo build` 干净；`cargo test --lib` 1256 passed；
`cargo clippy --all-targets --all-features -- -D warnings` 干净；`cargo fmt --all --check` 通过；
`cargo test --test tui_backend_smoke` 4 passed。
**Notes**:
- 切全屏后失去了 "inline、内容留在终端 scrollback" 的 claude-code 手感——这是用户
  主动放弃的。退出 TUI 后 shell 干净，不再有 banner/transcript 残留。
- resize 不再需要手动轮询/重建：fullscreen 下 `terminal.draw()` 每帧 autoresize。
- 空状态判定 `blocks.is_empty() && !turn.running`；`submit_prompt` 提交即 push User
  block，故首条消息后 splash 立即消失；`/clear` 后有 "Conversation cleared." System
  block，blocks 非空，不会回到 splash。
