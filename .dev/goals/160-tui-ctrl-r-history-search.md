# Goal 160 — TUI: Ctrl+R 历史模糊搜索

**Roadmap**: TUI 体验提升系列 (part 3/4)

**Design principle check**:
- 仅改 `crates/recursive-tui/`，不动核心库
- 搜索算法自己实现（前缀/子串匹配），不引入 fzf/nucleo 依赖
- 复用 `ui/command_menu.rs` 渲染菜单

## Why

fake-cc gap 文档 §2 / §5 中，`Ctrl+R` 历史模糊搜索标注为 🔴。
用户需要滚动浏览历史时 ↑/↓ 不高效；模糊搜索能显著提升多次会话操作效率。

## Scope

### 1. 新 InputMode

```rust
InputMode::HistorySearch { query: String, matches: Vec<usize>, selected: usize }
```

`matches` 存储历史 Vec 中匹配的下标，`selected` 是当前高亮项。

### 2. 触发

在 `AppState::handle_key`：Prompt 模式下 `Ctrl+R` → 切换到 `HistorySearch { query: "", matches: all_indices, selected: 0 }`。

### 3. 搜索算法

`fn search_history(history: &[String], query: &str) -> Vec<usize>`：
- 若 query 空 → 返回所有下标（倒序，最近最前）
- 否则大小写不敏感子串匹配
- 每条历史保留其在原 Vec 中的下标（用于选中后回填）
- 返回按"是否以 query 开头"优先排序（前缀优先），结果最多 12 条

### 4. UI

复用 `render_command_menu`，传入匹配的历史条目（截断到 60 字符）作为补全列表。
搜索框显示在菜单上方，格式：`🔍 <query>█`

### 5. 键位

| 键 | 动作 |
|---|---|
| 普通字符 | 追加到 query，刷新 matches |
| Backspace | 删 query 末字符 |
| ↑/↓ | 移动 selected |
| Enter | 把选中历史填入 buf，退出 HistorySearch 模式 |
| Ctrl+R | 切换到上一条匹配（与 bash Ctrl+R 行为对齐） |
| Esc | 取消，恢复 Prompt 模式 |

### 6. 历史持久化（不做）

历史持久化依赖 session 存储（Goals 151-157），本 goal 不做，当前会话内有效。

### 7. Tests

- `history_search_empty_query_returns_all_reversed`
- `history_search_prefix_match_ranked_first`
- `history_search_case_insensitive`
- `history_search_returns_at_most_12`
- `ctrl_r_in_prompt_mode_enters_history_search`
- `ctrl_r_in_bash_mode_no_op`
- `history_search_enter_fills_buffer`
- `history_search_esc_cancels`
- `history_search_backspace_on_empty_exits_mode`

## Acceptance

1. `cargo test --workspace` 全绿
2. `cargo clippy -- -D warnings` 无警告
3. 手工冒烟：输入几条消息，Ctrl+R 弹菜单，输入关键字过滤，Enter 填入
