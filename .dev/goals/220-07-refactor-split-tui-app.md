# Goal 220 — Refactor: 拆分 `tui/app.rs` (3303 → 4 文件)

**Roadmap**: Phase 20 — 0.7 Refactor & Hardening
**依赖**: Goal 219（先清掉 deprecated 路径，避免拆 tui 时还要双修）
**类型**: A — 架构级重构（人/Claude 主导）

## Why

`src/tui/app.rs` 3303 行，是仓库内最大单文件。`grep -c "^impl"` 显示只有一个 `impl App` 块——典型 mega-impl：状态机 + event loop + keymap 解析 + 命令注册 + cost 累加 + 权限弹窗 + markdown 渲染 + completion 全塞在一起。

副作用：
- 修改 1 行需要读 3000+ 行上下文
- 拆分后 `App` struct 的 16 个字段（`grep -A20 "^pub struct App"`）可按职责归到不同子结构
- 无法独立 unit-test keymap 解析、command dispatch、render 等子模块

## Design

### 拆分目标

```
src/tui/
  mod.rs                      ← pub use 出口（保留现有 re-export）
  app/
    mod.rs                    ← App 顶层 struct 协调（≤ 300 行）
    state.rs                  ← App 16 字段 + 状态机迁移函数（≤ 600 行）
    event_loop.rs             ← 主循环、StepEvent 订阅、tick 处理（≤ 800 行）
    commands.rs               ← 命令注册、dispatch、keymap 绑定（≤ 700 行）
    render.rs                 ← ratatui 渲染逻辑、widget 状态（≤ 700 行）
```

### 公开 API 保持不变

```
pub use app::{App, PendingPermission};
pub use app::{preview_args, verb_for_tool, parse_apply_patch_input, parse_v4a_patch};
```

外部 `use crate::tui::App` / `use crate::tui::backend::*` 全部不破坏（Goal 226 再做 crate 级别迁移）。

### 字段迁移建议

| 字段 | 当前位置 | 迁移到 |
|---|---|---|
| `transcript: Vec<RenderEntry>` | `app.rs` | `state.rs::AppState` |
| `status_line: String` 等 UI 状态 | `app.rs` | `state.rs` |
| `event_rx: UnboundedReceiver<...>` | `app.rs` | `event_loop.rs::Loop` |
| 命令注册表 / keymap | 内嵌 | `commands.rs::CommandRegistry` |
| 后端抽象 | `backend.rs`（独立） | 保留 |

App 顶层变成协调器：持有 `AppState + Loop + CommandRegistry`，方法 `tick()` / `handle_key()` / `render()` 委派到子模块。

## 验收标准

- `tui/app.rs` ≤ 50 行（仅 `mod.rs` 引用）
- 4 个新文件总和 ≈ 原 3303 行 ± 100 行
- `cargo test --workspace` 全绿
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- TUI 启动后键入 prompt、回车、看到响应——基本交互路径在手动 smoke test 中工作
- `tui/app/` 子目录与现有 `tui/ui/` 子目录并列存在（两者职责不同：`ui/` 是 widget 渲染原语，`app/` 是 App 状态与循环）

## Non-goals

- 不改 TUI 公共 API
- 不动 `tui/backend.rs`（已独立）
- 不动 `tui/ui/` 下的 widget 代码
- 不做 sub-crate 化（那是 Goal 226）
