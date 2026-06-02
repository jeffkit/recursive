# Goal 210 — Hook System V2 P3-1: TUI Hook 进度展示

**Roadmap**: Hook System V2 — Phase 3 TUI 集成
**提案**: `.dev/proposals/hook-system-v2.md`
**依赖**: Goal 209（异步 Hook）

**Design principle check**:
- 修改 `src/event.rs`（或 `src/agent.rs`）— 新增 StepEvent 变体
- 修改 `src/tui/app.rs` — 处理新事件
- 修改 `src/tui/ui/chat.rs` 或 `spinner.rs` — 展示 hook 状态

## Why

当前 TUI 中 hook 运行时用户完全无感知：没有任何视觉反馈表明有 hook
在执行。fake-cc 的 hook 支持 `statusMessage` 字段，运行时在 spinner 行展示。
对于耗时 hook（如 prompt hook 调用 LLM），这个反馈至关重要。

## Scope

### 1. 新增 `StepEvent` 变体

在 `src/event.rs` 或 `src/agent.rs` 中：

```rust
/// Hook 开始执行。
StepEvent::HookStarted {
    /// 事件名（如 "PreToolCall"）。
    hook_event: String,
    /// Hook 标识（命令路径、URL 或 "prompt"）。
    hook_name: String,
    /// 来自 HookCommand::status_message 的自定义提示。
    status_message: Option<String>,
},
/// Hook 执行中（实时 stdout）。
StepEvent::HookProgress {
    hook_event: String,
    hook_name: String,
    /// 最新 stdout 行（最后一行）。
    last_line: String,
},
/// Hook 执行完毕。
StepEvent::HookFinished {
    hook_event: String,
    hook_name: String,
    /// "success" / "error" / "timeout" / "skipped"
    outcome: String,
    duration_ms: u64,
},
```

### 2. `ExternalHookRunner` 发送事件

在 `run_single_hook` 执行前后发送事件：

```rust
// 开始
if let Some(tx) = &self.event_tx {
    let _ = tx.send(StepEvent::HookStarted {
        hook_event: event_name.clone(),
        hook_name: hook_display_name.clone(),
        status_message: config.status_message.clone(),
    });
}

// stdout 流（每 500ms 采样一次）
// 完成
if let Some(tx) = &self.event_tx {
    let _ = tx.send(StepEvent::HookFinished { ... });
}
```

`ExternalHookRunner` 新增可选 `event_tx: Option<mpsc::UnboundedSender<StepEvent>>`。

### 3. TUI `App` 状态

在 `src/tui/app.rs` 中：

```rust
pub struct App {
    // 已有字段 ...

    /// 正在运行的 hook 状态（None = 无）。
    pub active_hook: Option<ActiveHookState>,
}

pub struct ActiveHookState {
    pub hook_event: String,
    pub hook_name: String,
    pub status_message: Option<String>,
    pub last_line: Option<String>,
    pub started_at: Instant,
}
```

处理 `UiEvent`：
- `HookStarted` → 设置 `app.active_hook`
- `HookProgress` → 更新 `app.active_hook.last_line`
- `HookFinished` → 清除 `app.active_hook`

### 4. TUI 渲染

在 `src/tui/ui/spinner.rs` 或 `chat.rs` 中，当 `app.active_hook` 非 None 时：

```
⟳ [hook: Checking git command safety...]  (0.3s)
```

格式：
```
{spinner_char} [hook: {status_message 或 hook_name}]  ({elapsed}s)
```

颜色：Cyan（区别于 LLM spinner 的 Yellow）。

若有 `last_line`（stdout 进度），在 hook 行下方以 DarkGray 展示：
```
  › last stdout: checking remote refs...
```

## Tests to add

1. `hook_started_event_sets_active_hook` — App 处理 HookStarted 后 active_hook 非 None
2. `hook_finished_event_clears_active_hook` — App 处理 HookFinished 后 active_hook 为 None
3. `hook_progress_updates_last_line` — HookProgress 更新 last_line
4. `spinner_shows_hook_status_message` — render_blocks 包含 statusMessage 文本

## Acceptance

- `cargo test --workspace` 绿色
- `cargo clippy` 干净
- TUI 中 hook 运行时显示 hook 名称/状态消息
- hook 结束后提示消失，恢复正常状态
- stderr/non-TUI 模式下不输出额外内容
