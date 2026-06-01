# Goal 158 — TUI: @file 自动补全  ✅ DONE

**Roadmap**: TUI 体验提升系列 (part 1/4)

**Design principle check**:
- 仅改 `crates/recursive-tui/`，不动核心库
- 复用现有 `ui/command_menu.rs` 的菜单渲染，不新增 widget
- 不引入新依赖（用 `std::fs::read_dir` + `glob` 逻辑手写，无需 glob crate）

## Why

用户在输入框输入 `@` 后期望弹出文件补全菜单，让 agent 自动把文件路径插入提示词。
这是 fake-cc gap 文档 §2 中优先级第 2 高的功能。工程量小（1 goal），但体验提升大：
用户不再需要手动记忆并打出文件路径。

## Scope

### 1. 触发条件

在 `PromptInputState::handle_char_input` 里，当：
- 输入模式为 `Prompt`（不是 Bash / Note / Command）
- 新输入字符为 `@`
- 且 buf 当前光标前没有未完成的 `@` 词（避免重复触发）

则切换到 `InputMode::AtFileSearch { query: String }`，同时把 `@` 插入 buf。

当 `query` 变化（继续输入）时刷新候选列表。当 `Esc` / `Backspace 删到 @` 时退出该模式。

### 2. 候选文件枚举

新函数 `fn glob_workspace_files(query: &str) -> Vec<String>`：
- 从当前工作目录（`std::env::current_dir()`）递归 `read_dir` 最多 2 层
- 过滤出与 `query`（前缀匹配，忽略大小写）匹配的路径
- 排除 `target/`、`.git/`、`node_modules/` 目录
- 结果按路径字典序排序，返回前 12 条（与 CommandMenu 最大条目对齐）
- 路径以相对于 cwd 的形式返回（如 `src/agent.rs`）

此函数是纯函数（无 IO 缓存），每次 query 变化时重新调用。候选列表存在
`AppState.atfile_suggestions: Vec<String>` 里。

### 3. UI 展示

当 `InputMode::AtFileSearch { .. }` 时，在输入框**上方**弹出补全菜单：
- 复用 `ui/command_menu.rs::render_command_menu`，把候选 `Vec<String>` 转成
  `Vec<CommandItem { name: path, description: "" }>` 传入
- 选中高亮、↑/↓ 导航
- 与 `/` 命令补全行为一致（包括键位）

### 4. 选择逻辑

| 键 | 动作 |
|---|---|
| ↑/↓ | 移动选中项 |
| Tab / Enter | 把选中路径插入 buf（替换 `@<query>` 为 `@<selected_path>`），退出 AtFile 模式，恢复 Prompt 模式 |
| Esc | 取消 AtFile 模式，buf 保留 `@<query>` 不变，恢复 Prompt 模式 |
| Backspace | 若 query 非空则删 query 末字符；若 query 空则删 `@`，退出 AtFile 模式 |
| 其他字符 | 追加到 query，刷新候选 |

### 5. AppState 变更

```rust
// app.rs
pub enum InputMode {
    Prompt,
    Bash,
    Note,
    Command,
    AtFileSearch { query: String },
}

// AppState 增加
pub atfile_suggestions: Vec<String>,
pub atfile_selected: usize,
```

### 6. 不做的事

- ❌ 不做 @symbol / LSP 补全（需要 LSP 集成）
- ❌ 不做跨目录 glob（只枚举 cwd 最多 2 层）
- ❌ 不缓存文件列表（简单直接，避免 stale 缓存问题）
- ❌ 不做 @URL 或其他 @ 语义（只有文件）

## Tests

- `atfile_mode_triggered_by_at_in_prompt_mode`
- `atfile_mode_not_triggered_in_bash_mode`
- `atfile_mode_not_triggered_in_command_mode`
- `glob_workspace_files_filters_by_query_prefix`
- `glob_workspace_files_excludes_target_dir`
- `glob_workspace_files_returns_at_most_12`
- `atfile_backspace_on_empty_query_exits_mode_and_deletes_at`
- `atfile_enter_inserts_selected_path_and_exits`
- `atfile_esc_cancels_and_preserves_at_query`
- `atfile_mode_not_triggered_if_at_already_in_query_context`

## Acceptance

1. `cargo build -p recursive-tui` 通过
2. `cargo test --workspace` 全绿（含上述 10 个新测试）
3. `cargo clippy --all-targets --all-features -- -D warnings` 无警告
4. `cargo fmt --all -- --check` 通过
5. 手工冒烟：在 TUI 输入框键入 `@src`，弹出菜单显示 `src/` 下的文件；按 Tab 插入路径；按 Esc 取消
