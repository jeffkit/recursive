# Goal 229-02 — Unwrap cleanup batch 02: transport.rs + shell.rs

**Roadmap**: Phase 20 — 0.7 Refactor & Hardening
**父 goal**: Goal 229
**依赖**: Goal 219 已合并

## Why

当前生产代码（非测试块内的代码）中：
- `src/tools/transport.rs`：**20 处** `.unwrap()` / `.expect(...)` — MCP 传输层热路径
- `src/tools/shell.rs`：**17 处** `.unwrap()` / `.expect(...)` — 进程执行路径

这是第二批 unwrap 清理，目标是最重要的两个 tool 文件。

重要说明：
- 只处理 `#[cfg(test)] mod tests` 之前的生产代码
- 先用 `grep -n "\.unwrap()\|\.expect(" src/tools/transport.rs | head -30` 查看分布再下手
- 测试代码里的 unwrap 是合法的，**不要动**

## Scope（只改这两个文件，不动其他）

### 目标文件

- `src/tools/transport.rs`（20 处生产代码 unwrap）
- `src/tools/shell.rs`（17 处生产代码 unwrap）

### 替换策略（按优先级）

1. **`lock().unwrap()` / `lock().expect("...")`**（Mutex/RwLock）：
   ```rust
   // before
   self.inner.lock().unwrap().field
   // after (最常用)
   self.inner.lock().unwrap_or_else(|e| e.into_inner()).field
   // 或者对确实 unrecoverable 的：
   #[allow(clippy::unwrap_used, reason = "lock poison is unrecoverable")]
   self.inner.lock().unwrap().field
   ```

2. **`Option::unwrap()` / `Result::unwrap()`**：改为 `ok_or_else(|| Error::Tool { ... })?`

3. **`parse().unwrap()`**：改为 `parse().map_err(|e| Error::Tool { ... })?`

4. **不能立即替换的**：加 `#[allow(clippy::unwrap_used, reason = "...")]`，reason 必须说明为什么安全

### Error 变体参考

`src/error.rs` 中的 `Error::Tool` 现在有三个字段：
```rust
Error::Tool {
    name: String,
    call_id: Option<String>,  // 通常 None
    message: String,
}
```

## 验收标准

- `cargo test --workspace` 全绿
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- `cargo fmt --all` 通过
- 净减少 ≥ 20 处违规（两文件合计）
- 剩余的 unwrap 必须有 `#[allow]` + `reason`

## Notes for the agent

- 先读文件头了解 imports，再处理 unwrap
- `transport.rs` 是 MCP 客户端传输层，`shell.rs` 是 Bash/shell 执行工具
- 对 `child.spawn().unwrap()` → `child.spawn().map_err(|e| Error::Tool { name: "Bash".into(), call_id: None, message: format!("spawn: {e}") })?`
- **DO NOT modify** 本批目标文件之外的任何文件
