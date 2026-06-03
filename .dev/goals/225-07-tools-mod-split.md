# Goal 225 — Refactor: 拆分 `tools/mod.rs` (1377 → 4 文件)

**Roadmap**: Phase 20 — 0.7 Refactor & Hardening
**依赖**: Goal 219（删 deprecated 后再拆），以及 Goal 229 系列（先消解 1705 处 unwrap 的一部分）
**类型**: B — 机械拆分（self-improve 可执行）

## Why

`src/tools/mod.rs` 1377 行承担 4 件事：registry（`ToolRegistry`）、dispatch（`dispatch_request`）、audit（`AuditMeta`、`AUDIT_ERR_MAX_BYTES`）、policy（`PolicyToolSetProvider`）。`grep -c "^impl"` 显示所有这些都内联在 `mod.rs`。

每次新增 tool 都要修改这个文件，每次改 dispatch 逻辑也要碰它。

## Design

### 拆分目标

```
src/tools/
  mod.rs                  ← pub use 出口（≤ 100 行）
  registry.rs             ← ToolRegistry、build_standard_tools（≤ 400 行）
  dispatch.rs             ← ToolDispatch、dispatch_request、invoke（≤ 500 行）
  audit.rs                ← AuditMeta、AUDIT_ERR_MAX_BYTES、TouchedFiles（≤ 200 行）
  policy.rs               ← PolicyToolSetProvider、SandboxMode（≤ 300 行）
```

### 公开 API 保持不变

`lib.rs:124-129` 的 `pub use tools::{...}` 列表 13 个类型全部保留。`use crate::tools::X` 路径不破坏。

### 不动的东西

- `tools/apply_patch.rs`、`tools/fs.rs`、`tools/shell.rs` 等 28 个独立 tool 文件——它们已经是 `mod xxx;` 注册在 `tools/mod.rs` 里，迁出后仍然是 `mod xxx;` 在新 `tools/mod.rs` 里。

## 验收标准

- `src/tools/mod.rs` ≤ 100 行（仅 `mod` 声明 + 公共 `pub use`）
- 4 个新文件总和 ≈ 原 1377 行 ± 100 行
- `cargo test --workspace` 全绿
- `cargo clippy --all-targets --all-features -- -D warnings` 干净

## 备注

- 这是 B 类（机械拆分），可以交给 self-improve 跑
- 如果单 goal 太大（apply_patch 1320 行、facts 1321 行、a2a 1348 行也偏大），可考虑后续目标单独拆——但**本 goal 不动它们**，只拆 mod.rs
