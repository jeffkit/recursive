# Goal 229-01 — Unwrap cleanup batch 01: checkpoint.rs + mcp.rs

**Roadmap**: Phase 20 — 0.7 Refactor & Hardening
**父 goal**: Goal 229
**依赖**: Goal 219 已合并（deprecated Agent 路径已删，unwrap 场景已收窄）
**类型**: B — 机械消解（self-improve 主导）

## Why

当前生产代码（非测试、非 `unwrap_or` 变体）中，最高密度的两个文件是：
- `src/checkpoint.rs`：**51 处** `.unwrap()` / `.expect(...)`
- `src/mcp.rs`：**47 处** `.unwrap()` / `.expect(...)`

注意：`runtime.rs` 的 83 处 unwrap 大部分在测试代码（line 1300+）或是安全的 `unwrap_or`/`unwrap_or_default`——不需要修改。

这是 unwrap 系列清理的第一批，目标是真正有问题的两个文件。

## Scope（只改这两个文件，不动其他）

### 目标文件

- `src/checkpoint.rs`（51 处生产代码 unwrap）
- `src/mcp.rs`（47 处生产代码 unwrap）

### 替换策略（按优先级）

1. **`lock().unwrap()`（Mutex/RwLock）**：改为：
   ```rust
   .lock().map_err(|_| Error::Other("lock poisoned".to_string()))?
   // 或更精确：
   .lock().unwrap_or_else(|e| e.into_inner())  // 如果 poison 可恢复
   ```
   对于确实"panic 才是对的"的锁毒（极少），可用：
   ```rust
   #[allow(clippy::unwrap_used, reason = "lock poison is unrecoverable")]
   lock.unwrap()
   ```

2. **`Option::unwrap()`**：改为 `ok_or_else(|| Error::Other("msg".to_string()))?` 或更具体的 `Error` 变体。

3. **`parse().unwrap()`**：改为 `parse().map_err(|e| Error::Other(e.to_string()))?`

4. **不能立即修复的**：加 `#[allow(clippy::unwrap_used, reason = "...")]`，reason 必须写清楚为什么这里 panic 是安全的。

### 不能改的

- `#[cfg(test)] mod tests` 内部的 unwrap（由 Goal 224 处理）
- `test_util.rs` 内的 unwrap
- 任何会破坏当前语义的修改

## 验收标准

- `cargo test --workspace` 全绿
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- `cargo fmt --all` 通过
- `grep -c "\.unwrap()\|\.expect(" src/checkpoint.rs` 输出 ≤ 5（剩余必须有 `#[allow]` 注解）
- `grep -c "\.unwrap()\|\.expect(" src/mcp.rs` 输出 ≤ 10（剩余必须有 `#[allow]` 注解）

## 输出说明

完成后请在 final message 报告：
- 修改的行数
- 剩余 unwrap 总数（`grep -rn "\.unwrap()\|\.expect(" src/ | grep -v "#\[cfg(test)\]" | grep -v test_util | wc -l`）

## Notes for the agent

- 先 `grep -n "\.unwrap()\|\.expect(" src/runtime.rs | head -30` 看分布，再分批 patch
- `runtime.rs` 很长，用 `read_file` 的 `start_line`/`end_line` 参数分段读
- 避免用 `write_file` 覆盖整个文件——use `apply_patch`
- 有些 `Mutex::lock().unwrap()` 是"锁不会毒（正常流程不 panic）"的逻辑保证，但仍要加 `#[allow]` + reason
- **DO NOT modify** src/session.rs, src/runtime.rs, src/tools/, src/agent/, src/main.rs, 或任何其他文件
- 注意：`runtime.rs` 的 unwrap 大多在测试代码，不是本批目标
- 先 `grep -n "\.unwrap()\|\.expect(" src/checkpoint.rs | grep -v "cfg(test)\|mod tests" | head -20` 了解分布再下手
