# Goal 211 — 拆分 src/tui/app.rs：按职责分解为 4 个子模块

**Roadmap**: 代码健康 — 大文件专项整治（第一批，按行数降序）

**设计原则检查**:
- 纯代码组织重构，不改变任何运行时行为
- 只在 `src/tui/` 目录下操作，不碰核心库
- ❌ 不修改 `agent.rs::Agent::run` 主循环
- ❌ 不改变公开 API（pub 接口保持兼容）

## 背景

`src/tui/app.rs` 当前 **3915 行**，远超单文件可维护上限。该文件混杂了
五类职责：Transcript 数据模型、输入状态机、成本估算、文件/历史补全、
以及 `App` 主状态机本身。后续每次修改 TUI 都要在 3900 行里定位，
且 diff 噪声极大。

## 目标

将 `src/tui/app.rs` 拆分为 **4 个新模块** + 瘦身后的 `app.rs`：

| 新文件 | 迁移内容 | 预估行数 |
|--------|---------|---------|
| `src/tui/model.rs` | `DiffLineKind`, `DiffLine`, `DiffHunk`, `TranscriptBlock`, `AppScreen` | ~130 |
| `src/tui/input_state.rs` | `InputMode`, `PromptInputState`（含所有方法）, `DoublePressTracker`, `strip_history_prefix`, `double_press_window` | ~400 |
| `src/tui/cost.rs` | `UsageStats`（含方法）, `TurnState`（含方法）, `default_pricing_table`, `estimate_cost`, `detect_model_name` | ~300 |
| `src/tui/completion.rs` | `default_offline_tool_catalog`, `search_history`, `glob_workspace_files`, `collect_files` | ~250 |
| `src/tui/app.rs` | 只保留 `App` struct + `impl App`（主状态机）| ~2800 |

拆分后 `app.rs` 预计从 3915 行 → ~2800 行，4 个新文件合计 ~1080 行。
整体代码量不变，只是组织得更清晰。

## 实施细节

### 1. 新建 `src/tui/model.rs`

从 `app.rs` 剪切以下类型（保留所有注释和 derive）：
- `AppScreen` enum
- `DiffLineKind` enum
- `DiffLine` struct
- `DiffHunk` struct
- `TranscriptBlock` enum（包含所有 variant）

在文件顶部加 `#![allow(unused)]`（如有需要），正确 re-export 到 `src/tui/mod.rs`。

### 2. 新建 `src/tui/input_state.rs`

从 `app.rs` 剪切：
- `pub const DOUBLE_PRESS_WINDOW`
- `pub fn double_press_window()`
- `DoublePressTracker` struct
- `InputMode` enum + `impl InputMode`
- `PromptInputState` struct + `impl PromptInputState` + `impl Default for PromptInputState`
- `fn strip_history_prefix()`（私有辅助）

### 3. 新建 `src/tui/cost.rs`

从 `app.rs` 剪切：
- `UsageStats` struct + `impl UsageStats`
- `TurnState` struct + `impl TurnState` + `impl Default for TurnState`
- `pub fn default_pricing_table()`
- `pub fn detect_model_name()`
- `pub fn estimate_cost()`

### 4. 新建 `src/tui/completion.rs`

从 `app.rs` 剪切：
- `pub fn default_offline_tool_catalog()`
- `pub fn search_history()`
- `pub fn glob_workspace_files()`
- `fn collect_files()`（私有辅助）

### 5. 更新 `src/tui/mod.rs`

在模块声明区加入：
```rust
pub mod model;
pub mod input_state;
pub mod cost;
pub mod completion;
```

并重新 `pub use` 这些模块中被 `app.rs` 外部引用的类型（主要是
`TranscriptBlock`、`AppScreen`、`InputMode`、`UsageStats` 等）。

### 6. 更新 `src/tui/app.rs`

- 在文件顶部加对应 `use crate::tui::{model::*, input_state::*, cost::*, completion::*};`
- 删去已迁移的类型定义（只保留 `App` struct + `impl App`）
- 保留所有测试（`#[cfg(test)] mod tests`），不动逻辑

### 7. 扫描并修复其他引用

搜索整个 codebase 中对这些类型的引用（特别是 `src/tui/ui/` 下的渲染模块），
确保 `use` 路径更新正确：
- `crate::tui::app::TranscriptBlock` → `crate::tui::model::TranscriptBlock`
- `crate::tui::app::InputMode` → `crate::tui::input_state::InputMode`
- `crate::tui::app::UsageStats` → `crate::tui::cost::UsageStats`
- 等等

## 验收标准

1. `cargo build -p recursive --features tui` 通过（无新错误）
2. `cargo test --workspace` 全绿，不新增失败
3. `cargo clippy --all-targets --all-features -- -D warnings` 干净
4. `cargo fmt --all -- --check` 干净
5. `src/tui/app.rs` 行数 **≤ 2900**
6. 四个新文件存在且各自职责单一

## 明确不在范围内

- ❌ 不拆分 `src/tui/app.rs` 的 `App::handle_ui_event`（留给后续 goal）
- ❌ 不修改任何 `src/tui/ui/` 渲染逻辑
- ❌ 不修改 `src/tui/backend.rs` / `src/tui/events.rs`
- ❌ 不做功能变更

## 注意事项

- `src/tui/ui/transcript.rs` 是渲染层，与新建的 `src/tui/model.rs`（数据层）同名但层次不同，不冲突
- 迁移时保持 `pub` 可见性不变，不要缩小可见性
- 所有注释（文档注释 + 内联注释）随代码一并迁移
- 若有循环依赖（如 model 需要 input_state），调整迁移边界
