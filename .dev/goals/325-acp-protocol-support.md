# Goal: 为 Recursive agent 实现 ACP（Agent Client Protocol）协议支持

## ⚠️ 已完成的 sprint（不要重做）

**Sprint 0 (P0) — ACP 协议类型层：已完成并 commit。**

- `src/acp/mod.rs` 已创建（声明 `pub mod protocol`）
- `src/acp/protocol.rs` 已创建（916 行，re-export 自 `agent-client-protocol-schema` crate，64 个 round-trip test 全过）
- `Cargo.toml` 已加 `agent-client-protocol-schema = "1.4"` 依赖
- `src/lib.rs` 已加 `pub mod acp;`
- 全 workspace test 661 个全过；clippy 0 warning；fmt clean
- Commits：`a9522f9` + `4fb3ea2`

**planner 重新拆 sprint 时，sprint 1 应该从 P1（stdio JSON-RPC loop + initialize）开始**，不要再做协议类型层。

---

## 一句话需求

让 Recursive 编码 agent 支持 Zed 的 **Agent Client Protocol (ACP) v1**，作为 ACP server 通过 stdio JSON-RPC 与编辑器（Zed / JetBrains / 其他）通信，使其可被任何 ACP client 当作 coding agent 调用。

新增 `recursive acp` CLI 子命令，与现有 `recursive mcp` / `recursive http` 并列。

---

## 协议参考（必读）

- 官方 spec：https://agentclientprotocol.com/protocol/v1/
- 仓库：https://github.com/agentclientprotocol/agent-client-protocol
- 协议版本：**v1**（`protocolVersion: 1`）
- 唯一稳定 transport：**stdio**（newline-delimited JSON-RPC 2.0，UTF-8）。stdout 不许写非协议字节，stderr 留给日志。

---

## 项目硬约束（违反任意一条即视为失败）

### Recursive 8 大不变式（必读 `.dev/AGENTS.md`）

1. **Agent loop stays small**：ACP 分发逻辑**绝对不能**作为分支加进 `src/run_core.rs::RunCore::run_inner`。新能力永远是 tool / provider / adapter。
2. **Orthogonality**：ACP adapter 不能依赖 LLM internals；provider 不能依赖 ACP。
3. **Sandbox**：所有 fs 操作必须过 `tools::resolve_within`。**ACP `session/new` 的 `cwd` 就是沙箱根**——天作之合，直接接现有 sandbox。
4. **Tests are non-negotiable**：每个新的 public 函数 / tool / 模块必须有 `#[cfg(test)] mod tests` 在同一文件里。
5. **No `unwrap()` / `expect()` in non-test code**。`clippy::unwrap_used` 是 deny。
6. **No new dependencies without justification**。本 goal 已授权的依赖：`agent-client-protocol-schema`（官方 Rust 类型 crate）。其他新依赖必须在 sprint contract 里写明理由。
7. **Finish reasons are data, not errors**：`session/cancel` 必须返回 `stopReason: "cancelled"`，不能是 error。provider abort 返回 `Err(Error::Cancelled)`，run_inner 翻成 `FinishReason::Cancelled`，ACP 桥再翻成 `stopReason: "cancelled"`。
8. **Tool-call ↔ tool-result pairing**：取消 / abort 后 transcript 里仍必须保持配对，host 拒绝编辑必须写一条 error tool result，不能丢。

### 其他强制项

- **质量门**：`cargo test --workspace` / `cargo clippy --all-targets --all-features -- -D warnings` / `cargo fmt --all` 必须全绿。
- **`cargo` 不在 PATH**：所有 cargo 命令前缀 `PATH="$HOME/.cargo/bin:$PATH"`。
- **worktree 铁律**：所有代码改动在当前 worktree `feature/acp-protocol-support`（路径 `/Users/kongjie/projects/Recursive-feature/acp-protocol-support`）。**绝对不在项目根的 main 上改。**
- **不动 `Cargo.toml` 加依赖**除非 spec.md 写明。

### generator 行为铁律（sprint-1 教训沉淀）

- **🚫 禁止用 `cargo run` / `cargo build` 创建临时探查二进制**（比如 `acp_derive_check` / `acp_explore3` / `acp_json_explore`）。
  - 探查 crate 类型用 `cargo expand` 或写到 `#[cfg(test)] mod tests` 里。
  - worktree 提交时 `git status` 必须干净，不允许遗留可执行文件。
- **🚫 禁止写孤立模块文件**。新建 `src/<mod>/` 必须同时：
  - 加 `src/<mod>/mod.rs`（或在 `src/lib.rs` 里 `pub mod <mod>`）
  - 否则 cargo 根本不编译，等于死代码、测试也跑不到
- **📦 Cargo.toml 新依赖必须 `optional = true` 加 feature gate**（参考 `agui-protocol` / `redis` 等）。仅在协议类型层（纯数据，被所有后续模块依赖）时可以非 optional，但要在 spec.md 显式说明。
- **🔬 e2e/plugins 已经能 build**（worktree 已 symlink `Recursive-feature/infra4agent/argusai` → 真 argusai 仓库）。质量门 `e2e` 必须真过，不要跳过。
- **🧹 每个 sprint 完成前 `git status --short` 自检**，确认没遗留：
  - 调试用二进制 / 临时文件
  - 孤立模块（写了文件但没注册）
  - 未追踪的 `*.rs.bk` / `*.swp` 等

---

## 已拍板的 8 个设计决策（不要重新讨论）

| # | 决策 |
|---|------|
| 1 | 支持 editor→agent 的反向 fs（`fs/read_text_file` / `fs/write_text_file`）：client 声明 `fs.readTextFile=true` 时 agent 优先用 client buffer 拿未保存内容，否则降级到本地 Read。**沙箱校验永远跑。** |
| 2 | `session/load` 历史回放的 `messageId`：**生成 stable id**（内容 hash），不改 transcript schema。 |
| 3 | 协议类型用官方 **`agent-client-protocol-schema`** crate（纯类型，零 runtime）。 |
| 4a | 在途的 agent→client fs/* RPC：绑 `CancellationToken` + **30s 超时**双保险。 |
| **4b** | **LLM 流必须支持立刻 abort**：`ChatProvider::stream` 在 SSE 循环里 `tokio::select!` 上 cancel token，reqwest::Response drop 关连接，返回 `Err(Error::Cancelled)`，run_inner 已有逻辑翻成 `FinishReason::Cancelled`（不破 Invariant #7）。 |
| 4c | `src/acp/server.rs` 顶部必须写一节文档「协作式 cancel」说明语义。 |
| 5.1 | MCP transport **全支持**（stdio + http + sse）。stdio 在本地起子进程，http/sse 不起进程只连 URL。声明 `mcpCapabilities: { http: true, sse: true }`。 |
| 5.2 | MCP server 共存：session-scoped 注册表，session/close 时 kill 子进程；与全局 config 合并，命名冲突 session 优先。 |
| 5.3 | session/close + load/resume **必须 kill** 上次的 stdio MCP 子进程，不泄露。 |
| 5.4 | `session/load` / `session/resume` 会带 `mcpServers` 配置，必须 kill 老 server、启新 server、再 return result。 |

---

## 阶段拆分（建议按这个分 sprint，8 个）

每个 sprint 必须自带验收点（contract 时写清「怎么验证」）。**不要重新排序**——P0/P1 是地基，P2 之后才能动 LLM/provider。

### P0 — ACP 协议类型层

- 在 `src/acp/protocol.rs`（或新 crate module）落地 ACP v1 wire types
- **决策 3**：用官方 `agent-client-protocol-schema` crate
- **验证**：serde round-trip 单测覆盖每个 enum/struct；编译过；clippy 干净

### P1 — stdio JSON-RPC loop

- `src/acp/server.rs`：`AcpServerRunner::run()`，复用 `src/mcp_server.rs::McpServerRunner::run()` 的 stdin/stdout 套路
- 先只支持 `initialize` 一个方法，返回 `protocolVersion=1` + `agentInfo=recursive` + 完整 `agentCapabilities`
- **验证**：喂一个 `initialize` 请求，能写出正确的 response

### P2 — `session/new` + `session/prompt`（text-only）

- ACP session 接到 `AgentRuntime`
- `session/new` 用 `cwd` 作沙箱根（接 `resolve_within`），生成 stable sessionId
- `session/prompt` 把 `ContentBlock[]` 里的 text 拼成 `Message` 喂给 `AgentRuntime::run()`
- `EventSink` 翻译成 `session/update` notification（`agent_message_chunk` + 最后 `stopReason`）
- **决策 2**：`messageId` 用内容 hash
- 暂不做 tool_call 通知、permission、fs
- **验证**：端到端跑一个 text prompt，收到 `agent_message_chunk` + `end_turn`

### P3 — tool_call 通知 + kind/status

- `Tool` trait 加默认方法 `kind() -> ToolKind`（默认 `Other`），具体 tool 覆盖（Read→read, Edit→edit, Bash→execute, Glob→search, …）
- 把 `ToolCall` / `ToolResult` 事件翻译成 `session/update` 的 `tool_call`（首次）和 `tool_call_update`（后续）notification
- `locations` 字段从工具的 path 参数提取
- **验证**：跑一个 Bash 调用，client 看到 pending→in_progress→completed 三段

### P4 — `session/cancel` + LLM 流 abort + permission 桥

**这是最复杂的一个 sprint，可能要拆成 P4a/P4b/P4c**

- **4b 改 provider**：`ChatProvider::stream` 在 SSE 循环 `tokio::select!` 上 cancel token，reqwest::Response drop 关连接，返回 `Err(Error::Cancelled)`。run_inner 已有逻辑翻成 `FinishReason::Cancelled`。改 `src/llm/openai.rs::parse_sse_stream` 和 `src/llm/anthropic.rs::parse_sse_stream` 两处。
- `session/cancel` notification 接 `AgentRuntime::set_interrupt_token`
- `PermissionHook` 桥：发 `session/request_permission` 给 client，等 `PermissionOutcome`，转 `PermissionDecision`
- **决策 4a**：fs/* 的在途 RPC 绑 cancel token + 30s 超时双保险
- **决策 4c**：`src/acp/server.rs` 顶部文档「协作式 cancel」一节
- **验证**：(1) cancel 时 LLM 流立刻断（不等下一个 chunk）；(2) `session/prompt` 响应是 `cancelled`，不是 error；(3) permission 弹窗能交互；(4) Invariant #8 不破（取消后 transcript 配对仍成立）

### P5 — `session/load` 历史回放 + `session/resume`

- `session/load`：从 SessionStore 拉历史 transcript，逐条用 `session/update` 回放（`user_message_chunk` / `agent_message_chunk` / `tool_call`），回放完 `return result=null`
- `session/resume`：不回放，恢复 context 后 return
- `SessionCapabilities` 声明 `resume: {}` + `loadSession: true`
- **决策 2**：messageId 用内容 hash
- **决策 5.4**：load/resume 重连 MCP（kill 老 stdio server、启新 server）
- **验证**：关掉重连，load 能看到完整历史；resume 不回放

### P6 — editor fs（agent→client）+ MCP 多 transport + session/close 清理

- **决策 1**：`ClientReadFile` / `ClientWriteFile` 工具，client 声明 `fs.readTextFile=true` 时优先用，否则降级本地 Read，**沙箱校验永远跑**
- **决策 5.1**：MCP bridge 支持 stdio（起子进程）/ http / sse（远程连接），配置转换在 `src/acp/mcp_bridge.rs`
- **决策 5.2/5.3/5.4**：session/close + load/resume 必 kill 老的 stdio 子进程；命名冲突 session 优先于全局 config
- **验证**：(1) 能拿 editor 未保存 buffer；(2) stdio/http/sse 三种 MCP server 都能连；(3) session 关闭后 `ps` 无遗留子进程

### P7 — CLI `recursive acp` 子命令 + E2E + invariants test

- 在 `crates/recursive-cli` 加 `Acp` 子命令，与 mcp/http 并列
- **E2E**（按 `CLAUDE.md` e2e 规则）：scripted ACP client 跑一遍 prompt→tool→cancel→load 流程，断言 notification 序列
- **invariants test**：
  1. ACP 代码不往 `run_inner` 加分支（用 `tests/invariants/loop_size_orthogonality.rs` 同款 AST 检查）
  2. ACP host fs 操作仍走 `resolve_within`（沙箱逃逸检测）
  3. cancel 时 tool-call ↔ tool-result pairing 仍成立（Invariant #8）
- **验证**：所有 invariants test 过；E2E 通过；Zed 客户端能连上

---

## 关键文件指引（不要乱猜路径，按这个来）

| 现有抽象 | 路径 | 复用方式 |
|---------|------|---------|
| MCP server stdio loop | `src/mcp_server.rs::McpServerRunner::run` | 抄成 `AcpServerRunner::run` |
| Agent 运行时 | `src/runtime.rs::AgentRuntime::run` + `event_sink()` | 直接用 |
| ReAct step loop（**禁动**） | `src/run_core.rs::RunCore::run_inner` | 不要改，加分支即违反 Invariant #1 |
| 事件流 | `src/event.rs::Event` | 加 `event_to_acp_update()` 映射 |
| Session 生命周期 | `src/session/` + http/mod.rs `SessionState` | ACP session id 用现有 session id |
| Tool trait | `src/tools/mod.rs::Tool` + `registry.rs` | 加默认方法 `kind() -> ToolKind` |
| 沙箱 | `src/tools::resolve_within` | ACP cwd → 沙箱根；host fs 也要走 |
| 权限钩子 | `src/permissions/` + `PermissionHook` | 桥到 ACP `session/request_permission` |
| Provider streaming | `src/llm/openai.rs::parse_sse_stream` + `src/llm/anthropic.rs::parse_sse_stream` | **决策 4b 要改**：加 cancel token select |
| MCP 客户端 | `src/mcp.rs` | 决策 5.1：扩 http/sse transport |
| CLI 入口 | `crates/recursive-cli/src/main.rs`（`Cli` 枚举） | 加 `Acp` 变体 |

---

## evaluator 验收重点（skeptical QA 必须查的）

每个 sprint 的 evaluator 验收时，**除了 contract 验收点**，还必须检查：

1. **`cargo test --workspace` 全绿**（不是某个 test，是全 workspace）
2. **`cargo clippy --all-targets --all-features -- -D warnings` 全绿**（一个 warning 都不能有）
3. **`cargo fmt --all -- --check` 全绿**
4. **没新 `unwrap()` / `expect()` 在非 test 代码**（grep 验证）
5. **ACP 代码不在 `src/run_core.rs::run_inner` 里**（AST 检查）
6. **ACP host fs 操作过 `resolve_within`**（grep 验证）
7. **`session/cancel` 响应是 `stopReason: "cancelled"`**（不是 error）
8. **取消 / abort 后 transcript tool-call↔tool-result 配对仍成立**（grep `Role::Tool` 序列）
9. **决策 4b 改的 SSE 循环**：reqwest::Response 在 cancel 路径被 drop（grep `select!` + `cancelled()`）
10. **ACP 子命令注册到 CLI**（`recursive --help` 能看到 `acp`）

**evaluator 不许放水**：上述任何一条 fail 即整个 sprint fail，进 repair loop。

---

## 范围外（不要做）

- 不动 Recursive 的 ReAct kernel（`src/kernel.rs` / `src/run_core.rs::run_inner`）
- 不实现 ACP v2（spec 还在 draft）
- 不实现 streamable HTTP transport（draft）
- 不实现 ACP `terminal/*`（除非 sprint contract 明确要求；本 goal 不要求）
- 不实现 Slash Commands、Session Modes、Session Config Options（这些是 optional ACP features，第一版不做）
- 不重写 `src/mcp_server.rs`（只是参考其结构）
- 不删任何现有 CLI 子命令

---

## 参考实现

- Rust 官方高层 SDK：https://github.com/agentclientprotocol/agent-client-protocol（`agent-client-protocol` runtime crate + `agent-client-protocol-schema` 类型 crate）
- 官方 spec 文档：https://agentclientprotocol.com/protocol/v1/
- 现有 Recursive MCP server（结构参考）：`src/mcp_server.rs`
- 现有 Recursive HTTP/SSE API（流式参考）：`src/http/`

---

## 期望产物

- `src/acp/` 新模块（按 P0~P6 阶段组织）
- `crates/recursive-cli` 加 `acp` 子命令
- 完整的 unit test + integration test + e2e test
- `.dev/journal/manual-<date>-acp-protocol.md` 记录关键决策和非显然细节
- 8 大不变式全过
- Zed 编辑器能用 `recursive acp` 当 coding agent（最终手动验收）
