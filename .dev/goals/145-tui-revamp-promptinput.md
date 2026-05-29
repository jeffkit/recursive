# Goal 145 — TUI Revamp Step 3: 多模式 PromptInput（! / # / / + 多行 + 历史）

**Roadmap**: Phase 11 — TUI 大改造对齐 fake-cc 风格 (part 3/5)

**Design principle check**:
- 仅改 `crates/recursive-tui/`，不动核心库
- `!`（bash 模式）走 `run_shell` 工具，不经 LLM —— 通过给 backend 发
  `UserAction::RunShell(cmd)` 实现，对核心 API 无侵入
- `#`（备注模式）纯本地：作为 `TranscriptBlock::System` 写入，不发给 LLM
- `/`（命令模式）只在本步把"输入触发"做出来，命令实际执行留给 Step 4

## Why

fake-cc 的 PromptInput 是其交互"灵魂"：一个输入框承载 4 种模式，让用户
不用切屏就能在"和 Agent 对话 / 直接跑命令 / 写笔记 / 调命令面板"之间无缝
切换。当前 TUI 是单行单模式，编辑只支持末尾追加 + Backspace，没有历史，
也没有多行支持。本步是用户体感最显著的升级。

## Scope (do exactly this, no more)

### 1. 输入模式定义

在 `app.rs` 增加：

```rust
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum InputMode {
    Prompt,   // 默认：发给 Agent
    Bash,     // 以 ! 开头：直接 run_shell
    Note,     // 以 # 开头：仅本地保存
    Command,  // 以 / 开头：弹命令补全菜单（菜单本身在 Step 4 实现）
}

pub struct PromptInputState {
    pub mode: InputMode,
    pub buffer: String,        // 不含模式前缀
    pub cursor: usize,         // byte offset 到 buffer
    pub history: Vec<String>,  // 已提交记录（最多 200 条）
    pub history_idx: Option<usize>, // 当前回溯位置
    pub draft: String,         // 回溯时保存的当前草稿
}
```

### 2. 模式切换规则

#### 自动识别（输入第一个字符时）

- 如果 `buffer` 为空且按下 `!`：进入 `Bash` 模式（`!` 不写入 buffer，用作
  模式标记）
- 同理 `#` → `Note`，`/` → `Command`
- 任何其他字符 → 保持 `Prompt`

#### 显式切换

- `Shift+Tab`：循环 `Prompt → Bash → Note → Prompt`（跳过 Command —— 它
  只能用 `/` 进入；这是 fake-cc 的实际行为）
- 当 `buffer` 非空切换模式时：仅改变 `mode` 字段，buffer 保留（用户可
  以在 prompt 文本前加 `!` 改成 bash）

#### 退出模式

- 在 Bash/Note/Command 模式下按 Backspace 且 `buffer` 为空 → 退回 Prompt
- 提交后自动回到 `Prompt`

### 3. 模式指示符（输入框左侧）

在 `ui/input.rs` 渲染输入框，最左边一个字符：

| 模式 | 字符 | 颜色 |
|---|---|---|
| Prompt | `❯` | Cyan |
| Bash | `!` | LightYellow |
| Note | `#` | DarkGray |
| Command | `/` | Magenta |

格式：`<指示符> <空格> <buffer 内容> <光标>`。光标用 `▌` 或 ratatui 的真
光标定位（`frame.set_cursor_position`）；推荐后者，避免与终端原生光标行为
冲突。

### 4. 多行 + 编辑

- `Enter` → 提交（详见下方"提交逻辑"）
- `Shift+Enter` 或 `Alt+Enter` → 在 `cursor` 位置插入 `\n`，光标 +1
- `Left` / `Right` → 移动光标（按 char 边界，使用 `unicode-segmentation`
  或简单 `char_indices` 处理）
- `Home` / `End` → 移到当前行行首/行尾
- `Backspace` / `Delete` → 删除前一/后一字符
- `Ctrl+A` / `Ctrl+E` → 行首/行尾（`Ctrl+E` 与 Goal 144 的"展开 ToolResult"
  键冲突 —— 解决：当 `buffer` 非空时 `Ctrl+E` 走输入框；空时走 transcript）
- 字符输入插入到 `cursor` 位置而非追加到末尾

输入框高度自适应：取 `min(buffer 行数 + 1, 6)`，超过 6 行内部滚动。

### 5. 历史回溯

- `↑` / `↓`（在输入框聚焦且 `buffer` 为空 **或** 光标在 buffer 第一/最后
  一行时）→ 在 `history` 中翻
- 进入回溯前先把当前 `buffer` 存到 `draft`
- 翻到底/翻出去 → 恢复 `draft`
- 历史用 ringbuffer 存最近 200 条，按 `mode` 分别存还是混合？**混合存**
  即可（fake-cc 也是混合 + 用模式过滤显示，本步先简化为混合不过滤）

历史持久化：本步**不持久化**，仅当前会话有效。持久化留给二期。

### 6. 提交逻辑

按 `Enter` 时根据 `mode` 派发：

| 模式 | 动作 |
|---|---|
| Prompt | `action_tx.send(UserAction::SendMessage(buffer))` |
| Bash | `action_tx.send(UserAction::RunShell(buffer))` |
| Note | 直接 push 一个 `TranscriptBlock::System { text: format!("# {}", buffer) }` 到 transcript（不走 backend） |
| Command | Step 4 实现；本步先 push `TranscriptBlock::System { text: format!("(commands not yet implemented: /{})", buffer) }` |

提交后：`history.push(prefix + buffer)`（保留模式前缀以便回溯时恢复
模式）、`buffer.clear()`、`cursor = 0`、`mode = Prompt`、`history_idx = None`。

### 7. Backend 新增 RunShell action

在 `backend.rs::UserAction` 增加 `RunShell(String)`。worker 收到后：

```rust
// 用 runtime 的 ToolRegistry 直接调用 run_shell 工具
let registry = runtime.kernel().tools();
let call = ToolCall { id: "ui-bash".into(), name: "run_shell".into(),
                     arguments: json!({"cmd": cmd}).to_string() };
let result = registry.dispatch(&call).await;
// 把 result 推成 UiEvent::ToolCall + UiEvent::ToolResult
```

不进入 LLM 转录（不 push 到 `runtime.transcript`），bash 模式是"用户的便利
shell"，不污染 Agent 对话。

如果当前 LLM provider 不可用（offline），bash 模式仍然能工作 —— 这是个
意外的好处，要确保实现。

### 8. Footer hint

在输入框下方多渲染一行 hint（淡灰色，1 行）：

```
 ⏎ submit  shift+tab mode  ↑↓ history  ctrl+c interrupt  esc clear
```

根据 `mode` 略微调整：

- Bash 模式：`⏎ run shell  …`
- Note 模式：`⏎ save note  …`
- Command 模式：`⏎ run command  tab autocomplete  …`

### 9. 测试

- `prompt_input::shift_tab_cycles_modes`
- `prompt_input::leading_bang_enters_bash_mode_when_buffer_empty`
- `prompt_input::leading_hash_enters_note_mode`
- `prompt_input::leading_slash_enters_command_mode`
- `prompt_input::backspace_on_empty_exits_to_prompt_mode`
- `prompt_input::cursor_left_right_moves_within_buffer`
- `prompt_input::shift_enter_inserts_newline_at_cursor`
- `prompt_input::history_up_down_navigates_records`
- `prompt_input::history_up_saves_draft_and_restores_on_overflow`
- `prompt_input::submit_in_bash_mode_dispatches_run_shell`
- `prompt_input::submit_in_note_mode_appends_system_block`
- `prompt_input::submit_clears_buffer_and_resets_mode`
- `backend::run_shell_action_dispatches_tool_and_emits_events`
- `ui::input::renders_correct_indicator_per_mode`

集成测试（在 `tests/`）：用 MockProvider 跑下列场景：

1. 输入 `!echo hi` Enter → 收到 ToolCall + ToolResult，输出包含 "hi"
2. 输入 `# my note` Enter → transcript 中出现 System 块包含 "my note"，
   provider 没有收到任何 message

### 10. 不做的事

- ❌ 命令实际执行（`/help` 等）—— Step 4
- ❌ 命令补全菜单的 UI —— Step 4
- ❌ `@文件` 自动补全
- ❌ 外部编辑器（`Ctrl+G` 调 `$EDITOR`）
- ❌ 图片粘贴 / Voice
- ❌ 历史持久化
- ❌ 历史搜索（`Ctrl+R`）
- ❌ Vim 模式

## Acceptance

1. `cargo test --workspace` 全绿
2. `cargo clippy --all-targets --all-features -- -D warnings` 无警告
3. `cargo fmt --all -- --check` 通过
4. 手工冒烟：
   - 输入 `!ls` Enter → 立刻看到 ToolCall + ToolResult，无 LLM 调用
   - 输入 `# 这是笔记` Enter → 仅 transcript 出现 System 块
   - 按 Shift+Tab 三次能循环回到 Prompt
   - 输入若干消息后按 ↑/↓ 能翻历史
   - 多行输入按 Shift+Enter 换行，按 Enter 提交多行内容
   - 按 Backspace 在空 buffer 的 Bash 模式下退回 Prompt
5. Goal 143/144 现有功能不回归

## Notes for the agent

- 光标定位用 `frame.set_cursor_position((x, y))`，需要根据 buffer 内容
  + 输入框起点 + 当前 `cursor` 字节 offset 推算
- 多行 buffer 把 `\n` 视为换行，Home/End 找的是当前**行**而非整 buffer
- 历史回溯时：当前 buffer 不为空但用户按 ↑，是否替换？fake-cc 的行为
  是"光标在第一行行首才触发"——本步可简化为"buffer 为空才触发"，避免
  误触
- 不要引入 `tui-textarea` 等大型 crate，用 ratatui 内置 spans 自己拼
- `Shift+Enter` 在大多数终端会编码成不同 sequence，crossterm 的
  `KeyEvent` 带 `modifiers`，看 `KeyModifiers::SHIFT` 即可；某些终端
  根本送不出 Shift+Enter，那就只支持 Alt+Enter 作为备选
