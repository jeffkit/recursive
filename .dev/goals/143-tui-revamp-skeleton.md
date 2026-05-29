# Goal 143 — TUI Revamp Step 1: 骨架重构 + 切到 in-process AgentRuntime

**Roadmap**: Phase 11 — TUI 大改造对齐 fake-cc 风格 (part 1/5)

**Design principle check**:
- Implemented as: 重构 `crates/recursive-tui/` 内部结构，删除 reqwest，改为
  直接持有 `recursive_agent::AgentRuntime`
- ❌ Does NOT modify the agent loop (`src/agent.rs`)
- ❌ Does NOT modify the kernel (`src/kernel.rs`) or runtime (`src/runtime.rs`)
- Orthogonal: TUI 仍是 recursive-agent 的 consumer，只是改用库 API 而不是 HTTP

## Why

当前 TUI（`crates/recursive-tui/src/main.rs`，单文件 958 行）通过 `reqwest`
同步 POST `http://127.0.0.1:3000/sessions/{id}/messages` 与 server 通信，
等到完整响应才一次性渲染。这导致：

1. 用户必须先起 HTTP server，体验割裂
2. 丢失 11 种 `AgentEvent` 中的 7 种（`src/http.rs:1378-1401` 只映射 4 种）：
   `AssistantText` / `Latency` / `Usage` / `PartialToken` / `Compacted` /
   `PlanProposed` / `PlanConfirmed` / `PlanRejected` 全部拿不到
3. Plan Mode 用 `"plan:"` 文本前缀识别（`main.rs:367-372`），脆弱且与
   server 端无契约
4. 单文件 958 行已经很难继续扩展（多模式输入、命令系统、modal 栈都需要
   独立模块）

本 goal 是 5 步 TUI 改造的第一步：**搬骨架，不动外观**。把单文件拆成模块，
通信换成 in-process runtime worker，行为保持与改造前一致；为后续 4 步
（块状渲染 / 多模式输入 / 命令系统 / Plan 协议化）打地基。

## Scope (do exactly this, no more)

### 1. Cargo.toml 调整

在 `crates/recursive-tui/Cargo.toml`：

- **删除**：`reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }`
- **保留**：`recursive-agent = { path = "../.." }`、`ratatui`、`crossterm`、
  `tokio`、`serde`、`serde_json`
- 不引入新依赖

### 2. 模块拆分

把 `crates/recursive-tui/src/main.rs` 拆成下列文件（在 `crates/recursive-tui/src/` 下）：

```
main.rs            # bin 入口：terminal init/restore + tokio main loop
                   # 目标 < 80 行
app.rs             # AppState struct：transcript/input/scroll/screen
events.rs          # UiEvent enum + UserAction enum（替代当前 main.rs 的 UiEvent）
backend.rs         # Backend：包装 AgentRuntime，跑在独立 tokio task
                   # 实现 TuiEventSink: recursive_agent::EventSink，把
                   # AgentEvent 转 UiEvent 推到 mpsc<UiEvent>
keymap.rs          # 简单的 key→action 映射（本步只迁移现有键位，不加新键）
ui/
  mod.rs           # 顶层 render 分发（splash / chat / plan_review）
  splash.rs        # render_splash（从 main.rs:388-420 迁移）
  chat.rs          # 对应当前的 ui()（main.rs:456-502）；本步保持单行格式
  plan_review.rs   # render_plan_review（从 main.rs:422-454 迁移）
```

**强制要求**：
- 现有的视觉效果（消息单行格式 `You: ...` / `Agent: ...` / `🔧 tool` /
  `✓/✗ result`、splash 屏 logo、PlanReview 屏布局、状态栏）**保持不变**
- 消息颜色、修饰（DIM、ITALIC）保持不变
- 现有键位（Enter / ↑↓ / PgUp/PgDn / Esc / Backspace / 字符输入 / `q` 退出 /
  PlanReview 的 y/n/e）**保持不变**

### 3. 通信改为 in-process

新建 `backend.rs`，包含：

```rust
// 伪代码示意
pub enum UserAction {
    SendMessage(String),
    ConfirmPlan,
    RejectPlan(String),
    Shutdown,
}

pub struct Backend {
    /// 通过 mpsc 把用户动作发给 worker
    pub action_tx: mpsc::UnboundedSender<UserAction>,
    /// worker 通过 mpsc 推 UI 事件
    pub event_rx: mpsc::UnboundedReceiver<UiEvent>,
    _worker: tokio::task::JoinHandle<()>,
}

impl Backend {
    pub fn spawn() -> Self {
        // 1. 用 AgentRuntimeBuilder 构造一个最小可用的 runtime：
        //    - MockProvider（如果环境变量 RECURSIVE_TUI_MOCK=1）
        //    - 否则从环境读 LLM provider（参考 src/main.rs CLI 怎么读的）
        //    - 注册标准工具集（read_file/write_file/apply_patch/list_dir/
        //      run_shell/search_files）
        // 2. 注入 TuiEventSink 实现的 Arc<dyn EventSink>，把 AgentEvent
        //    转成 UiEvent 推到 mpsc
        // 3. spawn tokio task：循环从 action_rx 收 UserAction，
        //    SendMessage(text) → runtime.run(text).await
        //    ConfirmPlan → runtime.confirm_plan() 后再 runtime.run("")
        //    RejectPlan(reason) → runtime.reject_plan(&reason)
        //    Shutdown → break
    }
}

struct TuiEventSink {
    tx: mpsc::UnboundedSender<UiEvent>,
}

#[async_trait::async_trait]
impl recursive_agent::event::EventSink for TuiEventSink {
    async fn emit(&self, event: recursive_agent::event::AgentEvent) {
        // 把 AgentEvent 映射到 UiEvent 推过去；本步只映射当前 TUI
        // 已经在用的 4 种（AssistantMessage / ToolCall / ToolResult /
        // Error），其余先静默丢弃。后续 step 会逐步消费更多。
    }
}
```

**注意**：
- `AgentRuntime::run` 是 `&mut self`，所以 runtime 实例只能存在 worker
  task 内部，UI 线程不直接持有
- 用 `tokio::sync::Mutex<AgentRuntime>` 不必要 —— 单 worker task 串行处理
  UserAction 即可
- 若 LLM provider 配置失败（缺 API key 等），worker 应推一个
  `UiEvent::Error { message: "..." }` 而不是 panic；UI 仍可启动只是无法
  实际对话（保持当前 "Not connected — running offline" 的体验）

### 4. main.rs 瘦身

把 `crates/recursive-tui/src/main.rs` 改造成：

- 调用 `Backend::spawn()`
- 初始化 terminal
- 主循环：`tokio::select!` 在 crossterm key event 与 `backend.event_rx`
  之间，把 key 转成 `UserAction` 通过 `action_tx` 发出，把 `UiEvent`
  应用到 `AppState`，然后 `terminal.draw(...)` 调用 `ui::render(...)`
- 退出时 `action_tx.send(Shutdown)` 然后 await worker
- 目标行数 < 80 行

### 5. 测试迁移

把现有 30 个测试（`main.rs:558-958` `mod tests`）迁移到对应新模块下：

- `app_new_*` / `styled_message_*` / `handle_ui_event_*` / `*scroll*` /
  `splash_*` / `enter_*` / `esc_*` / `char_*` / `backspace_*` /
  `*assistant_message*` / `*plan*` 等
- 每个新模块用 `#[cfg(test)] mod tests` 自带单测
- 保留 100% 测试通过

新增 1 个集成测试（在 `crates/recursive-tui/tests/backend_smoke.rs`）：

```rust
// 用 MockProvider 跑一轮：
// 1. spawn backend
// 2. 发 SendMessage("hello")
// 3. 期望从 event_rx 收到 UiEvent::AssistantMessage / ToolCall / ToolResult
//    （取决于 mock 脚本）
// 4. 发 Shutdown，await worker
```

如果引入 MockProvider 测试需要 feature gate（如 `recursive-agent` 的
`test-utils`），在 `[dev-dependencies]` 里启用。

### 6. 不做的事

- ❌ 不改任何视觉效果（保留单行 `You:` / `Agent:` 格式）
- ❌ 不加新键位
- ❌ 不实现新功能（多模式输入 / 斜杠命令 / Status Bar 升级 / Diff 视图都
  在后续 goal）
- ❌ 不改 `src/` 下任何文件
- ❌ 不删除 HTTP server（`src/http.rs`），它还服务于 Python SDK 等

## Acceptance

1. `cargo build -p recursive-tui` 通过
2. `cargo test --workspace` 全绿，包括迁移后的 30 个原测试 + 1 个新集成测试
3. `cargo clippy --all-targets --all-features -- -D warnings` 无警告
4. `cargo fmt --all -- --check` 通过
5. 手工冒烟（在 goal 描述里只要求第一项，剩下由验收人确认）：
   - `cargo run -p recursive-tui` 启动 TUI
   - 看到 splash 屏 → 2 秒后自动进入 chat 屏
   - 输入 `hello` 按 Enter，能看到 `You: hello` 出现
   - 按 `q` 或 Esc 退出，终端正常恢复
6. 删除了 reqwest 依赖（`crates/recursive-tui/Cargo.toml` 中没有 reqwest 行）

## Notes for the agent

- **先读** `crates/recursive-tui/src/main.rs` 全部 958 行，掌握所有现有
  状态机、键位逻辑、测试覆盖
- **再读** `src/runtime.rs` 中 `AgentRuntime`、`AgentRuntimeBuilder`、
  `set_event_sink`、`confirm_plan`、`reject_plan`
- **再读** `src/event.rs` 中 `EventSink` trait 与 `AgentEvent` 11 种变体
- 推荐顺序：events.rs → backend.rs → ui/* → app.rs → keymap.rs → main.rs →
  跑测试
- 用 `apply_patch` 创建新文件用 `write_file`；这是个新建多个文件 + 删除一个
  巨型文件的工作，`write_file` 用得多很正常
- `recursive-agent` crate 在工作区根，是默认 binary 同名 lib，include 时
  `use recursive_agent::runtime::AgentRuntime;` 即可
- 如果 `EventSink` trait 用法不清，参考 `src/http.rs` 中的 SSE event sink
  实现
- 单文件 main.rs 拆完后总行数应在 800-1100 之间（拆分 + worker task 代码
  补回一些）
- 如果 `AgentRuntime` 构造失败（无 API key 环境），让 worker 进入"离线
  模式"：收到 SendMessage 时回推 `UiEvent::Error { message: "no LLM
  provider configured (set OPENAI_API_KEY or use mock mode)" }`，TUI 不
  panic
- 不要给 main.rs 加新功能，**这一步只是搬家**。任何忍不住加的功能写到
  下一个 goal 的 scope 里

## 后续

完成后，下一个 goal 是 144（Transcript 块状渲染 + Status Bar + Diff 视图）。
完整的 5 步路线图见 `~/.codebuddy/plans/toasty-pulse-babbage.md`。
