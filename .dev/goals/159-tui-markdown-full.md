# Goal 159 — TUI: 完整 Markdown 渲染（表格 + Syntax Highlighting）

**Roadmap**: TUI 体验提升系列 (part 2/4)

**Design principle check**:
- 仅改 `crates/recursive-tui/src/ui/markdown.rs`（Goal 150 已创建）和 `transcript.rs`
- 表格渲染使用 ratatui 的 `Table` widget
- Syntax highlighting 引入 `syntect = "5"` crate（成熟方案，纯 Rust）
- 不动核心库

## Why

Goal 150 实现了基础 4 构型（粗体、斜体、inline code、标题、bullet），
但 LLM 输出中频繁出现表格和代码块（带语言标记），均被原样渲染为文字。
这是 fake-cc gap 文档中 §3 的 🔴/🟡 项，影响日常使用体验。

## Scope

### 1. 表格渲染

在 `markdown.rs` 增加 Markdown 表格解析：
- 识别 `|col1|col2|` 样式的表格行和 `|---|---|` 分隔行
- 使用 ratatui 的 `Table` + `Row` + `Cell` widgets 渲染
- 表头加粗（`LightCyan + BOLD`），分隔行不渲染，数据行白色
- 列宽自动均分可用宽度
- 表格外边框用 `BorderType::Plain`

### 2. Syntax Highlighting（代码块）

引入 `syntect = { version = "5", default-features = false, features = ["default-syntaxes", "default-themes"] }`

在 `markdown.rs` 的 fenced code block 处理：
- 提取语言标记（` ```rust `, ` ```python ` 等）
- 用 `syntect::easy::HighlightLines` + `ThemeSet::load_defaults()`
  对代码行进行语法着色
- 把 `syntect::highlighting::Style` 转换为 ratatui `Color::Rgb(r,g,b)`
- 不支持的语言 fallback 为 `LightYellow`（Goal 150 的默认行为）
- 语法集懒加载（`once_cell::sync::Lazy`），避免每次渲染重建

### 3. 测试

- `table_three_columns_renders_cells`
- `table_header_separator_data_parses_correctly`
- `table_without_separator_falls_back_to_plain`
- `syntax_rust_keywords_get_color_spans`
- `syntax_unknown_language_uses_fallback_color`
- `syntax_empty_code_block_no_panic`
- `fenced_block_multiline_threading_unchanged`（回归：原 Goal 150 的行为不变）

## Acceptance

1. `cargo build -p recursive-tui` 通过
2. `cargo test --workspace` 全绿
3. `cargo clippy --all-targets --all-features -- -D warnings` 无警告
4. 手工冒烟：
   - 发一段含表格的消息，TUI 以 ratatui Table 渲染
   - 发一段含 ` ```rust ` 代码块的消息，关键字有颜色
   - 不含 markdown 的消息行为不变
