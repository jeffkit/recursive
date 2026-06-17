# Goal 229-04 — Unwrap cleanup batch 04: memory/*.rs + http + cli + tools 小文件

**Roadmap**: Phase 20 — 0.7 Refactor & Hardening
**父 goal**: Goal 229
**依赖**: Goal 229-03 已合并

## Why

清理 4 组小文件（各 1-4 处），一批搞定。

## Scope

| 文件 | 生产 unwrap 数 |
|------|--------------|
| `src/memory/sqlite_vec.rs` | 4 |
| `src/memory/noop.rs` | 4 |
| `src/http/mod.rs` | 4 |
| `src/cli/resume.rs` | 4 |
| `src/tools/facts.rs` | 3 |
| `src/tools/web_search.rs` | 2 |
| `src/skills.rs` | 2 |
| `src/permissions/mod.rs` | 2 |
| `src/main.rs` | 2 |
| `src/tools/web_fetch.rs` | 1 |
| `src/tools/e2b_provider.rs` | 1 |
| `src/providers.rs` | 1 |

**合计 ~30 处**

## 替换策略

### `lock().unwrap()` (Mutex/RwLock)

通常 lock poison 是不可恢复的，改为：
```rust
#[allow(clippy::unwrap_used, reason = "mutex poison is unrecoverable")]
self.db.lock().unwrap()
```

### `Option::unwrap()` / `Result::unwrap()`

改为 `?` 传播或 `ok_or_else(|| Error::...)?`

### `parse().unwrap()` / 常量式 unwrap

如果值是编译期确定安全的：
```rust
#[allow(clippy::unwrap_used, reason = "static value always valid")]
"127.0.0.1:0".parse().unwrap()
```

### main.rs 特殊处理

`main.rs` 里的 unwrap 通常是顶层 fatal，可以改为 `expect()` 加描述：
```rust
// 或者返回 anyhow::Result<()> 然后用 ?
```

## 验收标准

- `cargo test --workspace` 全绿
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- `cargo fmt --all` 通过
- 净减少 ≥ 20 处违规
- **只改以上列出的文件**

## Notes for the agent

- 先全部 grep 确认位置，然后按文件分批 read + apply_patch
- `src/llm/mock.rs`（7 处）和 `src/test_util.rs`（2 处）是测试基础设施，**本批跳过**
- lock poison 模式推荐统一用 `#[allow]` + reason 而非 `unwrap_or_else(|e| e.into_inner())` — 后者会掩盖数据竞争
