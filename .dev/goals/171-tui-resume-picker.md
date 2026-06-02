# Goal 171 — TUI: Resume picker modal（/resume + 启动时会话选择）

**Roadmap**: TUI 体验提升系列 — gap doc §7/§8 ROI #1（TUI 侧）

**Design principle check**:
- 仅改 `src/tui/`（app.rs、commands.rs、ui/modal.rs）
- 利用现有 `SessionReader::list_sessions` + `SessionMeta`（`src/session.rs`）
- 不动核心库 / runtime.rs / agent.rs
- ❌ 不在 `agent.rs::Agent::run` 主循环里加分支

## Why

gap doc §7 把 Resume picker modal 列为 🔴，ROI #1：
> Resume / 会话持久化（高价值 / 中成本）
> 落地路径：(1) session 磁盘存储；(2) TUI 启动时扫描，加 Modal::ResumePicker；(3) /resume 命令与 picker 复用

Session 持久化（Goal 151 + 152）已经完成 —— `~/.recursive/workspaces/<hash>/sessions/`
下有完整 JSONL transcript。现在只缺 TUI 入口。

## Scope

### 1. 新 `Modal::ResumePicker` 变体

在 `src/tui/ui/modal.rs` 里增加变体：

```rust
ResumePicker {
    entries: Vec<ResumeEntry>,
    selected: usize,
}
```

```rust
pub struct ResumeEntry {
    pub session_id: String,   // e.g. "abc12345"
    pub slug: String,         // first 40 chars of goal text or user's first message
    pub updated_at: String,   // human-readable "2026-06-01 14:22"
    pub turn_count: usize,
    pub cost_usd: f64,
}
```

渲染：复用 `render_journal_body` 的布局样式（modal title + ↑/↓ 选择列表 + 底部 hint）。
每行显示：`[slug…] | turns: N | $X.XX | updated: DATE`。

### 2. 数据加载函数

在 `src/tui/commands.rs` 或新文件 `src/tui/sessions.rs`（取决于代码量）
增加：

```rust
pub fn load_recent_sessions(workspace: &Path, limit: usize) -> Vec<ResumeEntry>
```

实现：
- 调 `SessionReader::list_sessions(workspace)` 拿到所有 session dir
- 每个 dir 读 `SessionMeta`（`SessionReader::load_meta`）
- 按 `meta.updated_at` 降序排，取前 `limit` 条（建议 20）
- 把 `meta.goal` 截 40 字符作为 `slug`（若 goal 为空，用 first user message）

### 3. `/resume` 命令

在 `src/tui/commands.rs` 的 `CommandRegistry::default_set()` 增加：

```rust
Command {
    name: "/resume",
    aliases: &["/r"],
    description: "Pick a previous conversation to continue",
    handler: CommandHandler::Sync(|app| {
        let entries = load_recent_sessions(&app.workspace_path, 20);
        if entries.is_empty() {
            app.push_system_block("No saved sessions found.");
        } else {
            app.modals.push(Modal::ResumePicker { entries, selected: 0 });
        }
        None
    }),
}
```

（`app.workspace_path` 需要在 `AppState` 里加一个字段，类型 `PathBuf`，
由 `Backend::new` 传入，参考 config 的 workspace 字段。）

### 4. 键盘处理（`handle_modal_key`）

在 `AppState::handle_modal_key` 里增加 `Modal::ResumePicker` 的分支：

- `↑/↓`：移动 selected
- `Enter`：
  1. 取选中的 `session_id`
  2. 把它存到 `AppState.pending_resume_session: Option<String>`
  3. pop modal
  4. 向 backend 发送 `UserAction::ResumeSession { session_id }`
- `Esc / q`：pop modal

### 5. Backend 处理 `UserAction::ResumeSession`

在 `src/tui/events.rs` 的 `UserAction` 增加：
```rust
ResumeSession { session_id: String },
```

在 `worker_loop` 增加对应分支：
1. 调 `SessionReader::load_transcript(session_dir)` 加载历史 transcript
2. 重建 `AgentRuntime`，把历史 messages 注入（复用 `AgentRuntime::from_messages`
   或类似构造方法；若不存在，则先加一个 2 行的简单方法）
3. 向 UI 推送 `UiEvent::SessionResumed { session_id, turn_count }` —— app
   清空 transcript（保留之前已有的 blocks）并显示一条 System 块
   "▶ Resumed session abc12345 (N turns)"

### 6. 测试

在 `src/tui/` 的测试里增加：
- `resume_picker_modal_renders`：构建若干 `ResumeEntry`，渲染 modal，断言
  包含 slug + date 文本
- `load_recent_sessions_sorts_by_updated_at`：写临时 session dir，
  验证结果按时间降序

## Acceptance

- `cargo test` 绿
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- `/resume` 命令弹出 picker，有历史会话列表
- 选择一条后，TUI 加载对应 transcript（System 块可见），可以继续对话
- 无 session 时，`/resume` 显示友好的"No saved sessions"提示
- ↑/↓/Enter/Esc 在 picker 里正确响应
- `ResumeEntry` 的 slug 不超过 40 字符（截断 + `...`）

## Notes for the agent

- `SessionReader::list_sessions` 返回 `Vec<PathBuf>`（session dir paths）；
  `SessionReader::load_meta(dir)` 返回 `Result<SessionMeta>`。两者在
  `src/session.rs` 里已实现。
- `SessionMeta` 字段参考 `src/session.rs:296`，有 `goal`, `updated_at`,
  `turn_count`, `cost` 等。
- `AppState.workspace_path` 可以从 `config.workspace` 复制过来；
  Backend 初始化时已经有 `config`，传一个 `Arc<Path>` 进来即可。
- `AgentRuntime` 的 messages 重建：找 `src/runtime.rs` 里的构造方法，
  看是否有接受 `Vec<Message>` 的；如没有，Goal 本身可以加一个简单的
  `with_initial_messages(msgs: Vec<Message>)` builder 方法（2-3 行）。
- 如果加载 transcript 时遇到旧格式（老版本 .json），直接跳过该 session，
  不报错（graceful degradation）。
- **DO NOT modify** `src/agent.rs` / `src/llm/` / `src/runtime.rs` 主逻辑
  （只允许加一个 builder 方法）.
