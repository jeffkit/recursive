# Manual edit: tui-markdown-fixes

**Date**: 2026-06-04
**Goal**: 修复 TUI markdown 渲染三个问题：表格宽度溢出、代码块无语法高亮、标题层级无差异
**Files touched**:
- `src/tui/ui/markdown.rs` — render_table 加 max_table_width 参数 + 列宽按比例收窄 + 单元格截断，代码块改用 highlight_code_line，标题按 H1-H6 差异化着色
- `src/tui/ui/transcript.rs` — render_blocks/render_block/render_assistant 加 width: u16 参数，render_assistant 将真实宽度传给 render_markdown
- `src/tui/ui/chat.rs` — 将 messages_area.width 传入 render_blocks
- `src/tui/app/event_loop.rs` — flush_ready_blocks 加 width: u16 参数
- `src/tui/mod.rs` — 调用 flush_ready_blocks 时传入 last_size.0（终端宽度）
**Tests added**: none (现有测试更新了调用签名，全量通过)
**Notes**:
- 表格总宽度 = gutter.len() + 1 + sum(col_widths) + 3*ncols；超出则按比例缩减，每列最小1字符，截断用 …
- 代码块前缀颜色从 Cyan → DarkGray（高亮内容自带颜色，前缀用灰色更协调）
- H1=LightCyan, H2=Cyan, H3=LightBlue, H4-H6=Blue；均 bold
