# Manual edit: fix-blank-viewport (v3 — dynamic viewport height)

**Date**: 2026-06-04
**Goal**: 修复 TUI viewport 消息区域在无 turn 运行时全为空白的问题
**Files touched**:
- `src/tui/mod.rs` — 恢复 flushed blocks 同步写入 `recent_display` 的逻辑，上限 300 行

**Root cause**:
今日的 `fix-duplicate-display` 修复移除了把 flushed blocks 写入 `recent_display` 的代码，
避免在超高终端窗口中内容同时出现在 native scrollback 和 viewport 产生重复。

但这个修复导致了新的回归：`recent_display` 永远为空，
messages panel（占 40 行 viewport 中的约 35 行）没有任何可渲染的内容，
用户在无 turn 运行时看到的是一大片空白 —— 即用户报告的"留给 modal 的空白"。

**Fix（最终版 — 滑动窗口模式）**:
采用"每行只存在于一个地方"的设计：
1. 每个 flushed block 先 `extend` 进 `recent_display`（不再克隆给 `insert_before`）
2. 当 `recent_display.len() > RECENT_DISPLAY_MAX` 时，将**溢出部分** `drain` 出来，
   将溢出行推给 `insert_before`（native scrollback），而非整个 batch

```rust
const RECENT_DISPLAY_MAX: usize = 300;
for lines in queued {
    app.recent_display.extend(lines);
    if app.recent_display.len() > RECENT_DISPLAY_MAX {
        let drain = app.recent_display.len() - RECENT_DISPLAY_MAX;
        let overflow: Vec<Line<'static>> = app.recent_display.drain(..drain).collect();
        terminal.insert_before(overflow.len() as u16, |buf| { ... })?;
    }
}
```

效果：
- Viewport 始终显示最近 300 行 → 无空白
- 超出 300 行的旧内容进入 native scrollback → 可向上滚动查看
- 每行内容只存在于一个地方 → 彻底无重复，与终端高度无关

中间版本（双写 + clone）在第一轮测试后发现仍会重复（高终端两者都可见），
因此迭代为滑动窗口方案。

**第三轮迭代（最终版）**：

用户反馈 v2 滑动窗口：logo 不动、内容被"吞掉"、终端滚动看不到历史。

根本矛盾：`INLINE_HEIGHT = 40`（固定值）在 60+ 行终端里，native scrollback 区域和 viewport 同时可见。任何双写（insert_before + recent_display）都会在可见区重复；纯 recent_display 则 logo 不动；纯 insert_before 则 viewport 空白。

**最终解决方案**：viewport 高度 = 终端高度（动态）
- `MIN_INLINE_HEIGHT = 20`（兜底最小值）
- 启动时：`initial_height = terminal_height.max(MIN_INLINE_HEIGHT)`
- resize 时：`cur.1.max(MIN_INLINE_HEIGHT)`
- 双写恢复：每个 block 同时写 insert_before（logo 随内容上移）和 recent_display（viewport 不空白，上限300行）
- viewport 填满整个终端 → native scrollback 永远在"上方不可见区域" → 双写无重复

**行为变化**：
- 启动 banner 立即进入 native scrollback（终端滚动可查看），不再常驻屏幕上方
- TUI 填满全屏，如 htop/vim 标准行为
- 内容随对话增长自然上移 logo

**Tests added**: none（全量测试 1112+ passed，此为纯展示层 fix）
