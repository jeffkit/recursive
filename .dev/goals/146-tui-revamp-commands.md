# Goal 146 — TUI Revamp Step 4: 斜杠命令系统 + Modal 栈

**Roadmap**: Phase 11 — TUI 大改造对齐 fake-cc 风格 (part 4/5)

**Design principle check**:
- 仅改 `crates/recursive-tui/`，不动核心库
- 命令实现都是只读 / 局部副作用（清 transcript / 触发 compactor / 显示
  modal），不改变 `AgentRuntime` 的行为契约
- Modal 是一个纯 UI 概念，不引入新事件类型

## Why

Goal 145 完成后用户输入 `/` 已经能进入 Command 模式，但实际命令既没注册
也没补全菜单。本步把命令系统真正接通：**一个注册表 + 一个补全弹层 +
10 个核心命令 + 一个通用 Modal 栈**。命令是用户掌控会话的"控制台"，缺了
它就只能用键位摸索。

## Scope (do exactly this, no more)

### 1. CommandRegistry

新建 `crates/recursive-tui/src/commands.rs`：

```rust
pub struct CommandSpec {
    pub name: &'static str,           // "help"
    pub aliases: &'static [&'static str], // ["?"]
    pub summary: &'static str,        // 一行说明
    pub usage: &'static str,          // 例 "/help [command]"
    pub handler: CommandHandler,
}

pub enum CommandHandler {
    /// 同步处理：直接修改 AppState（如清屏、显示 modal）
    Sync(fn(&mut AppState, &[String]) -> CommandOutcome),
    /// 异步处理：发送 UserAction 到 backend
    Async(fn(&mut AppState, &[String]) -> Vec<UserAction>),
}

pub enum CommandOutcome {
    Done,
    Error(String),     // 显示为 transcript 的 Error 块
    OpenModal(Modal),  // 推入 modal 栈
}

pub struct CommandRegistry {
    commands: Vec<CommandSpec>,
}

impl CommandRegistry {
    pub fn default_set() -> Self { /* 注册 10 个 */ }
    pub fn lookup(&self, name: &str) -> Option<&CommandSpec>;
    pub fn search(&self, prefix: &str) -> Vec<&CommandSpec>;
}
```

### 2. 10 个核心命令

| 命令 | 别名 | 行为 |
|---|---|---|
| `/help` | `/?` | OpenModal(`Modal::Help`)：列出所有命令 + 全局键位 |
| `/clear` | `/cls` | 清空 transcript（保留欢迎消息）+ 重置 UsageStats |
| `/compact` | — | 异步：`UserAction::Compact`（在 backend 调 `runtime.compact_now()` 或类似；如果 runtime 没暴露，则发一个空 `run("Please compact this conversation.")` 是不可接受的 —— 看 Notes） |
| `/cost` | — | OpenModal(`Modal::CostDetail`)：详细 token + cost breakdown |
| `/model` | — | OpenModal(`Modal::ModelInfo`)：显示 provider / model name / 端点 / 温度 |
| `/status` | — | 在 transcript 推一个 System 块：turn 数 / 消息数 / token 用量 / uptime |
| `/exit` | `/quit` `/q` | `should_quit = true` |
| `/tools` | — | OpenModal(`Modal::ToolList`)：列出已注册工具名 + 每个的简短说明（从 ToolRegistry 取） |
| `/plan` | — | `/plan on` / `/plan off`：通过 backend 发 `UserAction::SetPlanningMode(bool)`，runtime 端调 builder 重建或直接改 mut 字段（看下方） |
| `/journal` | — | OpenModal(`Modal::Journal`)：读 `.dev/journal/` 最近 5 个 .md 文件，只读展示，按 ↑↓ 翻 |

#### `/compact` 实现细节

`AgentRuntime` 内部已有 `compactor`，但**没有公开 `compact_now()`**。本
goal 在 backend 层用一个变通方案：

```rust
// 在 backend worker 中：
UserAction::Compact => {
    // 触发一次空 turn，runtime.run("") 会跑 compactor 检查
    // —— 不行，run 会真的去问 LLM
    // 替代：手动操作 transcript？也不行，需要 compactor.compact() 实例
    // 真正方案：本 goal 在 src/runtime.rs 暴露一个 pub async fn
    // compact_now(&mut self) -> Result<()>，封装现有 Compactor 调用
}
```

**例外允许**：本 goal 可以在 `src/runtime.rs` 增加一个公开方法：

```rust
/// Force a compaction pass right now (TUI / API surface).
pub async fn compact_now(&mut self) -> Result<()> { /* 调用现有 compactor */ }
```

修改面 < 30 行，不改变 trait、不改变现有调用方。如果发现影响别处（如
HTTP server），就把那点也补上一个 SSE 事件。

#### `/plan` 实现细节

`AgentRuntime` 的 `planning_mode` 是构造时设的（builder field），但是个
普通字段 `Option<PlanningMode>`。最小改动：在 runtime 上加 setter：

```rust
pub fn set_planning_mode(&mut self, mode: Option<PlanningMode>);
```

backend 收到 `UserAction::SetPlanningMode(true)` 时调它，并 push 一个
System 块 "Planning mode: on/off"。

### 3. Modal 栈

新建 `crates/recursive-tui/src/ui/modal.rs`：

```rust
pub enum Modal {
    Help,
    CostDetail,
    ModelInfo,
    ToolList { entries: Vec<(String, String)> },
    Journal { entries: Vec<JournalEntry>, selected: usize },
    Confirm { prompt: String, on_yes: ConfirmAction },
    // PlanReview 不在本步加，等 Step 5
}

pub enum ConfirmAction {
    Exit,
    Clear,
}
```

`AppState` 增加 `pub modals: Vec<Modal>`（栈）。渲染时栈顶 modal 居中
显示，背景半透明（用 `Block` 加 `Style::default().bg(Color::Black)`
覆盖一块）。

键位（在 modal 打开时优先于 chat 键位）：

- `Esc` / `q`：弹栈
- `Enter` / `y`（针对 Confirm）：执行动作
- `n`（针对 Confirm）：弹栈
- `↑/↓`（针对 Journal）：移动 selected

### 4. 命令补全菜单

新建 `crates/recursive-tui/src/ui/command_menu.rs`。

当 `InputMode::Command` 且 buffer 非空时，在输入框正上方浮一个最多 8 行
的菜单：

```
┌─────────────────────────────────┐
│ /help     Show commands & keys  │
│ /history  Browse history (todo) │
└─────────────────────────────────┘
```

数据来源：`CommandRegistry.search(buffer)`。键位：

- `Tab`：补全到唯一前缀（例如 buffer="he" → 补到 "help "）
- `↑/↓`：在菜单项中选择
- `Enter`：执行选中的命令（或如果没选中，执行 buffer 字面量）
- `Esc`：退出 Command 模式（同 Backspace 清空）

### 5. Help Modal 内容

`Modal::Help` 渲染：

```
 Recursive TUI — Help

 Commands:
   /help            Show this screen
   /clear           Clear conversation
   /compact         Compact the transcript
   /cost            Show token & cost detail
   /model           Show current model
   /status          Print runtime status
   /tools           List available tools
   /plan on|off     Toggle planning mode
   /journal         Show recent journal entries
   /exit            Quit

 Keys:
   Enter            Submit
   Shift+Enter      New line
   Shift+Tab        Cycle input mode (prompt → bash → note)
   ↑/↓ (empty buf)  Browse history
   PgUp / PgDn      Scroll transcript
   Ctrl+E           Toggle expand on tool result / input nav
   Ctrl+C           Interrupt (Step 5)
   Esc              Close modal / cancel
   q (in modal)     Close modal
```

### 6. CostDetail Modal 内容

```
 Token usage (this session)

   Input  : 4,231  ($0.0042)
   Output : 1,082  ($0.0033)
   Total  : 5,313  ($0.0075)

   Last turn latency: 1.34 s
   Provider         : deepseek-chat (deepseek)
```

数据来自 `AppState.usage_stats`。

### 7. 输入框驱动

修改 `app.rs::on_submit`（Goal 145 已经有针对 Command 模式的占位）：

```rust
InputMode::Command => {
    let line = buffer.clone();   // 不含 / 前缀
    let mut parts = line.split_whitespace();
    let name = parts.next().unwrap_or("");
    let args: Vec<String> = parts.map(String::from).collect();
    match registry.lookup(name) {
        Some(spec) => /* invoke spec.handler */,
        None => push_error(format!("Unknown command: /{}. Try /help.", name)),
    }
}
```

### 8. 测试

- `commands::registry_finds_help_by_name_and_alias`
- `commands::registry_search_returns_prefix_matches_sorted`
- `commands::clear_resets_transcript_and_usage`
- `commands::exit_sets_should_quit`
- `commands::status_appends_system_block_with_turn_count`
- `commands::unknown_command_pushes_error_block`
- `commands::plan_on_off_toggles_state_and_pushes_system_block`
- `ui::modal::esc_pops_top_modal`
- `ui::modal::confirm_yes_executes_action_and_pops`
- `ui::command_menu::tab_completes_unique_prefix`
- `ui::command_menu::up_down_moves_selection`
- `ui::command_menu::enter_runs_selected_command`
- `runtime::compact_now_invokes_compactor`（新加在 `src/runtime.rs` 的
  测试 mod 中）
- `runtime::set_planning_mode_updates_field`

### 9. 不做的事

- ❌ Plan Mode 协议化 —— Step 5
- ❌ 双击 Esc / Ctrl+C 中断 —— Step 5
- ❌ 历史搜索 modal
- ❌ Resume（会话恢复）modal —— 需要持久化先做
- ❌ 真正切换 model（`/model` 仅显示，不切换）

## Acceptance

1. `cargo test --workspace` 全绿
2. `cargo clippy --all-targets --all-features -- -D warnings` 无警告
3. `cargo fmt --all -- --check` 通过
4. 手工冒烟：
   - 输入 `/` 看到补全菜单弹出
   - 输入 `/help` Enter 看到 Help modal，按 Esc 关闭
   - 输入 `/clear` Enter，transcript 被清空
   - 输入 `/cost` 看到 CostDetail modal
   - 输入 `/foobar` 看到错误块 "Unknown command"
   - 输入 `/exit` 退出 TUI
5. Goal 143/144/145 现有行为不回归

## Notes for the agent

- `CommandRegistry` 用静态 `Vec<CommandSpec>` 即可，不需要动态注册
- 处理 modal 优先级：`if !app.modals.is_empty() { handle_modal_key(...) }`
  否则走输入框 / chat 键位
- modal 渲染建议用 `ratatui::widgets::Clear` 先清底，再画 `Block`
- 在 `src/runtime.rs` 加 `compact_now` 时复用 `Compactor::compact`，参考
  `src/runtime.rs:168-185` 自动 compact 的代码块
- 在 runtime 上暴露 `compactor` 引用 / `set_planning_mode` 时 **不要**
  破坏现有 builder API，加 setter 即可
- Journal modal 里读 `.dev/journal/*.md`，按修改时间倒序取 5 个，每个
  显示 first 30 行 —— 注意安全：路径 hardcode 到 `.dev/journal/`，不
  接受参数防止越界
- 命令补全菜单的位置：浮在输入框上方，而不是 transcript 下方 —— 用
  `Layout` 切出 input 高度后，把 menu 画在 `(input_y - menu_height,
  input_x)` 位置（直接绘制 widget，不参与 layout）
