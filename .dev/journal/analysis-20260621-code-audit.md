# 代码审计报告 — 2026-06-21

**审计人**：Cursor AI（Sonnet 4.6）  
**范围**：`/Users/kongjie/projects/Recursive/src/`  
**审计工具**：`cargo clippy --all-targets --all-features`、`cargo test --workspace`、静态阅读

---

## 执行摘要

Recursive 项目整体代码质量**较高**，工程规范意识明显。所有测试通过（**1 574 项**），Clippy 零警告，生产代码中无未标注的 `unwrap/expect`，安全敏感路径（路径沙箱、SSRF 防护、session_id 净化）均有保护。

发现问题主要集中在**可维护性**层面：若干核心文件超过 1 500 行，个别函数承担了过多职责；另有两处已知 TODO 尚未实现。无需紧急热修复的高危问题。

| 优先级 | 数量 |
|--------|------|
| 高（需立即修复）| 0 |
| 中（建议修复）  | 4 |
| 低（可选优化）  | 4 |

---

## 发现的问题

### 高优先级（需立即修复）

无。

---

### 中优先级（建议修复）

---

**P2-001 — `src/runtime.rs` 文件过长（2 731 行）**

- **文件**：`src/runtime.rs`
- **行号**：全文件
- **描述**：`AgentRuntime` 在单个文件内承载了会话生命周期管理、目标循环（`run_goal_loop`）、消息队列（`enqueue/drain_queue`）、事件发射（`emit_turn_messages`）、跨轮压缩（`maybe_compact_cross_turn`）等约 40 个公开方法，超过 2 700 行。随着功能增加，阅读和测试特定子功能的代价会持续上升。
- **建议修复方案**：
  - 将目标循环相关逻辑（`run_goal_loop`、`run_loop`、`run_event_loop`，约 200 行）提取到 `src/runtime_loop.rs`（已有 `src/runtime_goal.rs` 先例）。
  - 将 HTTP 层专用的 checkpoint 启用逻辑移至 `src/http/` 侧。
  - 不强制拆分，但建议在下一次较大重构时跟进。

---

**P2-002 — `run_inner` 函数过长（~407 行）**

- **文件**：`src/run_core.rs`
- **行号**：484–890
- **描述**：`run_inner` 是 agent 主循环，在约 407 行内完成了：取消检测、邮箱排空、Transcript 预算检查、紧凑压缩、LLM 调用、Plan Mode 管理、工具执行、卡住检测（stuck detection）共 8 个逻辑块。每个块本身注释清晰，但函数整体难以独立测试子段。
- **建议修复方案**：
  - 将 stuck-detection 逻辑（已在 `src/run_core.rs:800` 附近有独立注释块）提取为 `check_stuck(&recent_errors, window, rate) -> Option<StuckInfo>` 私有函数。
  - 将 mailbox-drain 提取为 `drain_mailbox(&mut self) -> Vec<Message>` 以便单元测试。
  - 其他逻辑块相对独立，可按需提取。

---

**P2-003 — `src/http/handlers.rs` 文件过长（2 140 行）**

- **文件**：`src/http/handlers.rs`
- **行号**：全文件
- **描述**：该文件包含 REST 会话端点（CRUD）、AG-UI（SSE 运行端点）、事件转换（`AguiConverter`）以及多个工具函数，合计 2 140 行。不同抽象层混在一处，`run_agent` 函数（lines 62–156）包含非平凡业务逻辑。
- **建议修复方案**：
  - 将 `AguiConverter` 及 AG-UI 事件映射（约 300 行）提取到 `src/http/agui.rs`。
  - 将 session CRUD（`create_session`/`get_session`/`delete_session`/`fork_session`）提取到 `src/http/session_handlers.rs`。
  - 保留 `handlers.rs` 仅做路由分发。

---

**P2-004 — `process_sse_line` 函数圈复杂度过高（195 行、25 个分支）**

- **文件**：`src/llm/anthropic.rs`
- **行号**：427–621
- **描述**：`process_sse_line` 处理 Anthropic 的 SSE 流式协议，内含 25 个 `match/if let/if` 分支，处理 content block start/delta/stop、thinking block、tool_use 等事件类型。高圈复杂度使新增事件类型时容易漏测分支。
- **建议修复方案**：
  - 将每类 delta 类型（`text_delta`、`input_json_delta`、`thinking_delta`）各自提取为独立的 `handle_text_delta`、`handle_input_json_delta`、`handle_thinking_delta` 私有方法。
  - 为每个子函数补充单元测试（目前测试主要覆盖 `parse_completion`，对 `process_sse_line` 的路径覆盖较少）。

---

### 低优先级（可选优化）

---

**P3-001 — `emit_turn_messages` 中不必要的 `Vec<Message>` 克隆**

- **文件**：`src/runtime.rs`
- **行号**：415、424
- **描述**：
  ```rust
  let new_messages = outcome.new_messages.clone();   // line 415 — 克隆整个 Vec
  // ...
  Arc::make_mut(&mut self.transcript).extend(new_messages.iter().cloned()); // line 424 — 再次逐条克隆
  ```
  `outcome` 是 `&TurnOutcome`（只读借用），所有后续操作（`rposition`、`extend`、`iter().enumerate()`）均可直接对 `&outcome.new_messages` 操作，第 415 行的 `clone()` 可消除。大型转录（数百条消息）时每轮多一次 `O(n)` 深拷贝。
- **建议修复方案**：
  ```rust
  // 改为：
  let new_messages = &outcome.new_messages;
  let mut tool_audits = outcome.tool_audits.clone();  // tool_audits 需要 mut，仍须 clone
  ```

---

**P3-002 — TODO：CLI 下 Plan Mode 的审批提示未实现**

- **文件**：`src/cli/output.rs`
- **行号**：235
- **描述**：
  ```rust
  /// TODO(plan-mode-repl): implement y/n approval prompt for PlanProposed events.
  ```
  在 CLI（非 TUI）模式下，Agent 提出的 Plan 无法交互审批，`PlanProposed` 事件被静默丢弃，用户无从感知。
- **建议修复方案**：在 `output.rs` 中捕获 `AgentEvent::PlanProposed` 并向 stdin 询问 `y/n`，分别调用 `AgentRuntime::confirm_plan` / `reject_plan`。

---

**P3-003 — TODO：AG-UI SSE 端点缺少部分事件类型映射**

- **文件**：`src/http/handlers.rs`
- **行号**：1252
- **描述**：
  ```rust
  // TODO(g141, g140): map permission_request / checkpoint_post /
  // heartbeat / file_artifact onto Custom events here.
  ```
  权限请求（`permission_request`）、检查点写入（`checkpoint_post`）、心跳（`heartbeat`）、文件产物（`file_artifact`）这四类 AG-UI 标准事件当前在 SSE 流中被直接丢弃（`_ => {}`），下游 AG-UI 客户端无法感知这些事件。
- **建议修复方案**：为这四类事件补充 `AgentEvent → ag::Event` 映射，参照现有的 `MessageAppended` 实现方式。

---

**P3-004 — `src/mcp.rs` 单文件承载过多（1 938 行）**

- **文件**：`src/mcp.rs`
- **行号**：全文件
- **描述**：MCP 协议实现（配置解析、服务进程管理、SSE/HTTP/stdio 传输、工具调用代理）均在单个文件内，测试代码占约 400 行。目前功能内聚，但继续扩展后维护难度会上升。
- **建议修复方案**：可考虑将 transport 层（stdio/SSE）抽取为子模块 `src/mcp/transport.rs`，配置解析提取为 `src/mcp/config.rs`，但不属于紧迫任务。

---

## Clippy 输出摘要

```
Checking agui-protocol v0.1.0
Checking recursive-agent v0.6.0
Checking agui-client v0.1.0
Finished `dev` profile [unoptimized + debuginfo] target(s) in 24.38s
```

**零警告，零错误。** `cargo clippy --all-targets --all-features` 完全干净。

---

## 测试覆盖情况

```
cargo test --workspace 结果：
  递归主包：  1 282 tests ok
  其他 crate：   292 tests ok
  总计：       1 574 passed / 0 failed / 2 ignored
```

覆盖面宽泛，包含：工具单元测试、LLM provider 解析测试、HTTP handler 集成测试、run_core stuck-detection 测试、路径沙箱测试等。

---

## 安全检查小结（均通过）

| 检查项 | 状态 | 位置 |
|--------|------|------|
| 路径穿越防护 (`resolve_within`) | ✓ 有测试 | `src/tools/dispatch.rs:207` |
| SSRF 防护（私有 IP 过滤）| ✓ 实现 + 测试 | `src/tools/web_fetch.rs:350` |
| Session ID 使用 UUID v7 生成 | ✓ | `src/http/handlers.rs:168` |
| Thread ID 写入文件系统前净化 | ✓ | `src/http/handlers.rs:1486` |
| 非测试代码无裸 `unwrap/expect` | ✓（有标注例外） | 全局 |

---

## 总计

| 优先级 | 数量 |
|--------|------|
| 高优先级 | 0 |
| 中优先级 | 4 |
| 低优先级 | 4 |
| **合计** | **8** |
