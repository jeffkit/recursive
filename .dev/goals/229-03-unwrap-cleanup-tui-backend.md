# Goal 229-03 — Unwrap cleanup batch 03: tui/backend.rs

**Roadmap**: Phase 20 — 0.7 Refactor & Hardening
**父 goal**: Goal 229
**依赖**: Goal 229-02 已合并

## Why

`src/tui/backend.rs`（1022 行）是 TUI 的核心协调器，包含 **23 处**生产代码 `unwrap()`/`expect()`，是整个代码库中最集中的违规点。

## Scope（只改这一个文件）

- `src/tui/backend.rs`（23 处生产代码 unwrap）

## 主要 unwrap 类型和建议修复

### 1. `rt_opt.as_ref().unwrap()` / `rt_opt.as_mut().unwrap()` / `rt_opt.take().unwrap()`

这是最常见的 Option unwrap：

```rust
// before
let rt = rt_opt.as_mut().unwrap();
// after
let Some(rt) = rt_opt.as_mut() else {
    tracing::warn!("backend: runtime not initialized");
    return Err(crate::Error::Internal {
        context: "tui::backend".into(),
        message: "runtime not initialized".into(),
    });
};
```

或者，如果函数返回 `()` 而非 `Result`：
```rust
let Some(rt) = rt_opt.as_mut() else {
    tracing::warn!("backend: runtime not available in handler");
    return;
};
```

### 2. `.expect("single owner after weixin task")` 等

改为更明确的 `ok_or_else`：
```rust
Arc::try_unwrap(arc_value)
    .map_err(|_| crate::Error::Internal { 
        context: "tui::backend".into(), 
        message: "arc has multiple owners".into() 
    })?
```

### 3. `lock().unwrap()` for Mutex

```rust
// before
self.state.lock().unwrap().field
// after (lock poison is usually unrecoverable, allow with reason)
#[allow(clippy::unwrap_used, reason = "mutex poison is unrecoverable")]
self.state.lock().unwrap().field
```

## 验收标准

- `cargo test --workspace` 全绿
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- `cargo fmt --all` 通过
- 净减少 ≥ 15 处违规（理想全部 23 处，实在不能改的加 `#[allow]` + reason）
- **只改 `src/tui/backend.rs`，不动其他文件**

## Strategy notes

- 先 `grep -n "\.unwrap()\|\.expect(" src/tui/backend.rs` 定位全部位置
- 分批次读取上下文，了解 Option 的生命周期
- 注意该文件是 async 代码（tokio），可以 `?` 传播错误
- `crate::Error::Internal` 有两个字段：`context: String` 和 `message: String`
