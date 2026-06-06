# Review: 接口层 (CLI / TUI / HTTP / MCP / WeChat)

**Date**: 2026-06-06
**Reviewer**: Architecture Critic (AI)
**Scope**: src/cli/, src/tui/, src/http/, src/mcp.rs, src/mcp_server.rs, src/weixin/

---

## Executive Summary

接口层整体工程质量较高：错误处理规范（anyhow 贯穿 CLI 层，无裸 unwrap），TUI 的 raw-mode RAII guard 可靠，HTTP 认证设计合理（常量时间比较）。主要风险集中在以下几点：1) HTTP 层无请求体大小限制，存在 OOM 向量；2) SSE 端点无超时，客户端断连后服务器端连接无法感知；3) MCP 协议在 `main.rs` 和 `mcp_server.rs` 中有两套几乎相同的实现（代码重复 + 行为分叉风险）；4) WeChat 命令功能已声明但尚未实现，存在悄无声息的功能缺失；5) 日志系统缺乏结构化字段和 trace_id，3am 排障困难。

---

## 严重问题 (Critical)

### C1 — HTTP 层无请求体大小限制（OOM 向量）

**位置**: `src/http/mod.rs:383–420`（`build_router_with_auth_and_rate_limit`）

Axum 默认请求体限制为 2MB，但此路由器**没有显式设置** `DefaultBodyLimit`。`POST /run` 接受 `goal: String`，`POST /sessions/:id/messages` 接受 `content: String`，两者都无大小限制。攻击者或失控客户端发送 100MB 的 goal 字符串会在 JSON 反序列化阶段把整个字符串 load 进内存，且 rate limiter 在 body 读取完成之前根本没有机会限流（rate limiter 是 middleware，在 body 被 extract 之前先于 handler 运行，但 body 的实际 IO 是在 axum extractor 里完成的，tokio 会先缓冲整个 body）。

**建议修复**:
```rust
use axum::extract::DefaultBodyLimit;
Router::new()
    // ... routes ...
    .layer(DefaultBodyLimit::max(1 * 1024 * 1024)) // 1 MB
```

---

### C2 — SSE 端点无超时，连接永久保活

**位置**: `src/http/handlers.rs:860–903`（`session_events`）

`GET /sessions/:id/events` 返回一个 `BroadcastStream`，客户端断开连接后服务器端 stream 不会立即感知（TCP RST 只有在下一次写操作时才触发错误）。`broadcast::Sender` 只要还有 `Receiver` 存在就不会被 drop。每个 `GET /sessions/:id/events` 请求都调用 `tx.subscribe()`，增加引用计数。如果客户端消费缓慢或断线不清理，大量 `Receiver` 会堆积，`broadcast::channel(64)` 的发送端在接收方满载时会丢弃旧消息——更糟糕的是这里实际上不会限制连接数。

此外，代理层（nginx/cloudflare）默认会在 60–90s 空闲后切断 SSE，但服务端没有发送 heartbeat，客户端无法区分静默（agent 在思考）和断线。

**建议修复**: 1) 添加周期性 heartbeat 事件（每 30s 发送 comment 行）；2) 设置 SSE keep-alive 或连接超时；3) 用 `AbortHandle` 跟踪 SSE 任务，在 session 删除时强制关闭。

---

## 中等问题 (Major)

### M1 — MCP dispatcher 在 main.rs 和 mcp_server.rs 中重复实现

**位置**: `src/main.rs:1922–2010`（`dispatch_request_via_registry`）vs `src/mcp_server.rs:110–197`（`dispatch_request`）

两套几乎相同的 JSON-RPC dispatcher：同样的 `initialize` response（但 capabilities 格式有细微差异：main.rs 用 `"tools": true`，mcp_server.rs 用 `"tools": {}`），同样的 `tools/list`、`tools/call` 逻辑。更危险的是 `method_not_found` 的签名不同：`mcp.rs` 的版本是 `method_not_found(id: Option<Value>, method: &str)`，`mcp_server.rs` 的是 `method_not_found(id: Value)`（无 method 参数）。今后修改 MCP 协议处理逻辑时，很可能只改了一处，另一处出现静默分叉。

`Cmd::Serve` 实际使用 `main.rs` 里的 `run_mcp_server_stdio` + `dispatch_request_via_registry`，而 `McpServerRunner` 使用 `mcp_server.rs::dispatch_request`。两路代码路径并存，没有任何注释说明为何要有两套。

**建议修复**: 删除 `main.rs` 中的 `dispatch_request_via_registry`，`Cmd::Serve` 改为直接使用 `McpServerRunner::new(tools).run().await`，统一到单一实现。

### M2 — WeChat 多条命令功能已声明但未实现（悄无声息的功能缺失）

**位置**: `src/weixin/daemon.rs:230–244`（`handle_command`）

`/l`（列出历史）、`/c N`（切换会话）、`/r`（重置会话）三个命令均返回"功能即将到来"的中文字符串。但这些命令已经通过 `/help` 文档对外声明（`src/weixin/commands.rs:52–60`），用户实际使用时会得到误导性的回应。更严重的是：`/r` 的语义（"重置当前会话"）如果未来实现会直接丢弃上下文，但目前的 handler 中没有任何锁或状态保护——届时需要仔细处理与 `runtime.enqueue` 并发调用的竞态。

`WeixinCommand::Change` 和 `WeixinCommand::List` 的 `_req_tx` 参数被有意忽略（前缀 `_`），说明设计者清楚这些命令最终需要通过 backend channel 实现，但当前实现绕过了这个机制。

**建议**: 要么从 `/help` 输出和 `parse_command` 中移除这些命令（直到实现），要么在 `handle_command` 里通过 `req_tx` 发送特殊控制消息到 backend。目前是把承诺和实现脱钩了。

### M3 — HTTP 会话 TTL 有配置但无 GC 任务被实际启动

**位置**: `src/http/mod.rs:275`（`AppState.session_ttl_secs`），`src/main.rs:682–699`（Http 命令构建 AppState）

`AppState` 携带 `session_ttl_secs` 字段，`src/http/mod.rs:813–830` 中有一个 `session_reaper` 函数描述（grep 结果确认），但在 `Cmd::Http` 的启动路径中（`main.rs:604–719`）没有 `tokio::spawn` 启动这个 reaper 任务。会话只会增不会减，长时间运行的 HTTP server 内存会持续增长，每个 session 都持有一个 `Arc<tokio::sync::Mutex<AgentRuntime>>`，运行时状态不会被释放。

**建议**: 在 `axum::serve` 之前 `tokio::spawn(session_reaper(...))` 并确保优雅关闭时 join 它。

### M4 — TUI 键盘事件在 50ms tick 内批量处理，有事件丢失窗口

**位置**: `src/tui/mod.rs:389–409`（主事件循环）

```rust
tokio::select! {
    _ = tokio::time::sleep(Duration::from_millis(50)) => {
        while event::poll(Duration::ZERO)? { ... }
    }
    Some(ui_event) = backend.event_rx.recv() => { ... }
    ...
}
```

`select!` 是 **非公平** 的（Tokio 的实现会随机选择就绪分支）。当 `backend.event_rx` 有事件时，timer 分支可能被饿死，导致键盘事件最多延迟到下一个 50ms tick 才被处理。更严重的是：如果 `backend.event_rx` 连续高频产生事件（agent 长时间运行时的 SSE 流），timer 分支永远得不到机会，crossterm 的键盘缓冲区会积压，最终**在缓冲区满时丢弃按键事件**。crossterm 使用的 OS 键盘缓冲区（`/dev/tty` 上的 `read` buffer）只有几 KB，快速连续按键（如 Ctrl+C）可能丢失。

**建议**: 拆分 timer 和 event 的处理，或者在 `backend.event_rx.recv()` 分支处理完后也检查键盘事件：
```rust
// After handling ui_event, drain keyboard too
while event::poll(Duration::ZERO)? { ... }
```

### M5 — Rate limiter 以 API key 明文作为 bucket key

**位置**: `src/http/rate_limit.rs:101–111`（`extract_client_key`）

```rust
if let Some(api_key) = req.headers().get("x-api-key") {
    return format!("apikey:{}", key);
}
```

API key 明文写入 rate limiter 的 `HashMap<String, TokenBucket>` 中。这把一个用于限速的辅助数据结构变成了一个隐式的 API key 注册表——任何拿到进程内存 dump 或者能读到 tracing span 输出的人都能看到所有曾发过请求的 API key 集合。

**建议**: 对 key 做 HMAC 或 SHA-256 哈希后再用作 bucket key：`format!("apikey:{}", sha256_hex(key))`。

---

## 轻微问题 (Minor)

### N1 — `shutdown_signal` 中有两处裸 panic/expect

**位置**: `src/main.rs:1419–1431`

```rust
.expect("failed to register SIGTERM handler")  // line 1421
ctrl_c.await.unwrap();  // line 1428 (非 Unix 路径)
```

`expect` 在注册 SIGTERM handler 失败时直接 panic（虽然在非容器环境极罕见，但违反 CLAUDE.md Invariant #5）。非 Unix 路径的 `ctrl_c.await.unwrap()` 同理。这发生在 `tokio::spawn` 内部，panic 会导致该 task 静默死亡，关闭信号永远不会触发——运行中的 HTTP server 无法正常关闭。

**建议**: 改为 `map_err(|e| tracing::error!(...))` 并在失败时 `t.cancel()`（最差情况：立即关闭，好过永远不关闭）。

### N2 — TUI Banner 中使用了 `unwrap_or_default` 掩盖配置错误

**位置**: `src/tui/mod.rs:275–277`，`src/tui/app/state.rs:15–18`

```rust
let workspace = crate::config::Config::from_env()
    .map(|c| c.workspace)
    .unwrap_or_else(|_| std::path::PathBuf::from("."));
```

`App::new()` 和 `run_with_backend` 各自独立调用 `Config::from_env()`，不共享结果。如果配置有问题（比如 TOML parse 错误），错误被静默吞掉，workspace 退化为 `"."`，session 列表会显示错误目录下的文件。这也意味着 TUI 启动时会有两次配置文件读取 IO。

**建议**: workspace 路径应从调用者（`main.rs`）传入，而不是在 TUI 内部重新解析。

### N3 — `mask_key` 对短 key（≤8字符）的掩码过于简单

**位置**: `src/main.rs:1434–1440`

长度 ≤ 8 的 API key 显示为 `****`，但这泄露了 key 的长度范围信息（用户知道 key 很短）。更重要的是，这个函数用于 `config show` 命令，如果 key 恰好是 8 字符（某些服务的 short token），`&&k[..4]...&&k[k.len()-4..]` 会暴露整个 key（前后各 4 字符覆盖了全部 8 字符）。

**建议**: `Some(k) if k.len() <= 8 => "****"` 应改为 `Some(k) if k.len() < 12 => "****"`，且对于 8 字符 key `k[..4]` 和 `k[4..]` 的组合暴露了完整 key。

### N4 — HTTP 认证 401 响应不符合 RFC 7235

**位置**: `src/http/auth.rs:218–220`

```rust
let mut resp = axum::response::Response::new(axum::body::Body::from("unauthorized"));
*resp.status_mut() = StatusCode::UNAUTHORIZED;
resp
```

RFC 7235 要求 401 响应必须包含 `WWW-Authenticate` header，否则客户端（尤其是 curl、浏览器）无法知道支持哪种认证机制。错误体是纯文本而非 JSON，与其他所有错误响应（`ErrorResponse { status, error }`）格式不一致。

**建议**: 统一返回 `Json(ErrorResponse { ... })`，并添加 `WWW-Authenticate: Bearer realm="recursive"` header。

### N5 — 日志无结构化字段、无 trace_id

**位置**: `src/logging.rs`（全局），`src/http/handlers.rs`（各 handler）

整个接口层的日志（包括 HTTP handler 错误）均通过 `eprintln!` 或 `tracing::error!` 输出，没有 `session_id`、`request_id`、`user_id` 等结构化字段。`tracing` 已经引入，但用法停留在格式化字符串级别，没有使用 `tracing::error!(session_id = %id, error = %e, "...")` 的结构化形式。

3am 排障时，面对一堆 `agent run failed: ...` 日志，无法快速关联到是哪个 session、哪个请求、哪个用户触发的。

**建议**: 在 HTTP middleware 层注入 `request_id`（UUID）作为 tracing span 字段，在 session 相关 handler 里用 `tracing::Span::current().record("session_id", &id)`。

### N6 — `resolve_session_path` 的双重目录扫描存在 TOCTOU

**位置**: `src/cli/session.rs:85–121`

函数先扫描旧格式 `.json` 文件，再扫描新格式 JSONL 目录，两次 `read_dir` 不在同一事务内。如果用户在两次扫描之间删除或重命名了 session，可能得到"模糊匹配"错误而不是"未找到"。此外，模糊匹配的 `contains` 比较是基于 session ID 的子串，如果工作区有大量 session，这是 O(n) 的线性扫描，没有任何缓存或索引。

### N7 — `list_sessions` 在 HTTP handler 中不保证稳定排序

**位置**: `src/http/handlers.rs:265–293`

`sessions.values()` 遍历 `HashMap`，顺序随机。加上 offset/limit 分页，两次请求之间排序可能变化，导致客户端分页时出现重复或跳过条目。

**建议**: 在返回前按 `created_at` 排序。

---

## 正面评价

1. **TUI raw-mode 恢复可靠**：`RawModeGuard` (`src/tui/mod.rs:225–231`) 通过 RAII 确保 `disable_raw_mode()` 在任何退出路径（包括 panic）都被调用，且使用了 `Viewport::Inline` 而不是 alternate screen，所以 panic 后终端状态天然可恢复。`TuiQuietGuard` 同样设计优雅。

2. **HTTP 认证常量时间比较**：`auth.rs:62–82` 中 `is_valid` 对所有 key 都遍历完整，不走捷径，正确防止了 timing side-channel 攻击。这是少见的认真实现。

3. **会话恢复的孤儿检测**：`src/cli/resume.rs:188–272` 的 OrphanPolicy 机制（detect + ask/skip/redo/abort）设计细致，特别是对 `ToolSideEffect::External` 类工具发出二次警告，显示了对操作安全性的认真考量。

4. **`--session-out` 废弃警告**：`src/main.rs:442–449` 明确打印废弃警告并给出迁移路径，向后兼容做得规范。

5. **HTTP provider 构建代码重复但有规律**：`run_once`、`repl`、`run_loop`、`Cmd::Http` 中的 provider 构建代码重复了 4 次（`match config.provider_type.as_str() { "anthropic" => ... }`），虽然是重复，但模式完全一致，clippy 不会报错，且每处都有相同的 retry policy 逻辑——这比"聪明"的抽象更容易 review。但长期维护时应提取为 `fn build_provider(config: &Config) -> Arc<dyn LlmProvider>`。

---

## 建议优先级

| 优先级 | 问题 | 工作量估计 |
|--------|------|-----------|
| 立即 | C1 — HTTP 请求体无大小限制 | 1行 |
| 立即 | C2 — SSE 无超时/heartbeat | 中 |
| 高   | M1 — MCP dispatcher 双实现 | 小 |
| 高   | M3 — session TTL reaper 未启动 | 小 |
| 中   | M2 — WeChat 命令存根未实现 | 中 |
| 中   | M4 — TUI 键盘事件丢失窗口 | 小 |
| 中   | M5 — rate limiter 明文 key | 小 |
| 低   | N5 — 无结构化日志/trace_id | 大 |

---

如果我只能改一件事，那就是 **C1**：在 `build_router_with_auth_and_rate_limit` 中加上 `DefaultBodyLimit::max(1 * 1024 * 1024)`。这是一行改动，但消除了一个任何知道接口存在的人都可以利用的 OOM 向量——而且 HTTP 接口在设计上是对外暴露的（有 auth 只是可选的）。其他问题要么有运维层面的缓解手段（nginx 前置限流），要么是功能缺陷而非安全漏洞。请求体无限制在没有反向代理的部署场景下是无条件的崩溃风险。
