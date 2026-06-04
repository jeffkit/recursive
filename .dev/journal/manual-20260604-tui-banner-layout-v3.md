# Manual edit: tui-banner-layout-v3

**Date**: 2026-06-04
**Goal**: 修复 TUI 启动页布局丑（200 列终端下右栏空 145 列；改窗口大小 viewport 不跟着重排）
**Files touched**: `src/tui/mod.rs`
**Tests added**: `tui::tests::*` 8 个新单测（visible_len / pad_to / compute_column_widths）

**Notes**:
- 把 banner 列宽算法抽成 `compute_column_widths(term_width) -> (left, right)` 纯函数，方便单测
- 列宽策略从 `clamp(36, 52)` 改为 `max(30)` —— 40/60 固定比例，左栏不再有 52 的硬上限
  - 200 列 → left=80, right=119（之前 left=52, right=147）
  - 100 列 → left=40, right=59
  - 60 列 → left=30 (floor), right=29
- 在 `run_with_backend` 的事件轮询里加 `crossterm::terminal::size()` 轮询，size 变了就 `make_inline_terminal` 重建 viewport —— TUI 启动后改窗口大小能跟着重排
- 复用了 `visible_len` / `pad_to` / `make_inline_terminal`，没动 chat / status / input 的渲染逻辑（这些本来就用 `area.width`，是 viewport 重建后自然就全宽了）
- 质量门：`cargo test --lib` 1116 全过（单线程下），`cargo clippy --all-targets --all-features -- -D warnings` 干净，`cargo fmt --all --check` 通过
- 并发 `cargo test` 里 `config::tests::memory_home_dependent_tests` 偶发失败（env var 竞争），main 上同样会复现，AGENTS.md 里已记录的已知问题，与本改动无关
- 手动 smoke：COLUMNS=80/120/200 三种宽度下 banner 渲染都正常，列比例符合 40/60
