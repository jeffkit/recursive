# Goal 217 — Session ID 注入 Tracing Spans + Turn Logs

**Roadmap**: Phase 15 — Observability & Monitoring
**依赖**: Goal 115 (tracing spans per step, 已合并)

**Design principle check**:
- 修改 `src/runtime.rs` — `AgentRuntime::run()` 在每 turn 起始/结束记录 session_id + turn_index
- 修改 `src/http/handlers.rs` — `create_session` 调用 `runtime.set_session_id()`
- ❌ 不修改 `agent.rs` 主循环
- ❌ 不新增 Cargo 依赖

## Why

当前 tracing spans（Goal 115 已建立）虽然按 turn 拆分，但 span 字段里没有 `session_id`：

```rust
// 现状（缺 session_id）
INFO recursive::runtime: turn started turn_index=1
INFO recursive::runtime: turn finished steps=4 reason=stop
```

多 session 并发跑 agent 时，日志无法按 session 切片，调试和 audit 困难。Datadog/OTEL 后端也无法按 session 聚合指标。

## Scope

### 1. `src/runtime.rs` — `AgentRuntime::run()` 注入 session_id

每 turn 起始时记录：

```rust
tracing::debug!(
    session_id = self.session_id.as_deref().unwrap_or("none"),
    turn_index = self.turn_index,
    "turn started"
);
```

每 turn 结束时记录：

```rust
tracing::info!(
    session_id = self.session_id.as_deref().unwrap_or("none"),
    turn_index = self.turn_index,
    steps = outcome.steps,
    finish_reason = ?outcome.finish_reason,
    "turn finished"
);
```

如果当前 span 存在，使用 `tracing::Span::current().record("session_id", ...)` 将 session_id 注入到当前 span 的字段（避免在 await 点跨越 span guard 引发 Send 错误）。

### 2. `src/runtime.rs` — `set_session_id()`

新增方法：

```rust
pub fn set_session_id(&mut self, id: impl Into<String>) {
    self.session_id = Some(id.into());
}
```

供 HTTP handler 显式设置。

### 3. `src/http/handlers.rs` — `create_session` 调用

```rust
pub async fn create_session(...) -> Result<...> {
    let session_id = generate_session_id();
    runtime.set_session_id(&session_id);
    // ... create SessionContext ...
    Ok(CreateSessionResponse { session_id, ... })
}
```

这样所有 HTTP-created session 的 turn 自动携带 session_id，无需客户端再传。

## 验收标准

- `RUST_LOG=recursive=debug` 跑一个 HTTP session，看到日志行带 `session_id=xxx`
- `RUST_LOG=recursive[{session_id=foo}]=debug` 能只过滤特定 session 的日志
- Datadog/OTEL exporter 把 `session_id` 作为标签携带
- `cargo test --workspace` 全绿
- ❌ 没有 turn 不带 session_id 时 crash（降级为 `session_id="none"`）

## Notes

- `session_id` 是 string 字段，OTEL/Datadog label 长度有限（< 200 chars），目前 session_id 是 UUID 派生，不会超限
- 不影响 LLM 调用、tool execution 的 latency
- 与 Goal 115 (tracing spans per step) 互补：本 goal 增加 session 维度，Goal 115 提供 step 维度
