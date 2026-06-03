# Goal 223 — Refactor: `Error` 收窄粒度

**Roadmap**: Phase 20 — 0.7 Refactor & Hardening
**依赖**: Goal 219
**类型**: A — 架构级重构（人/Claude 主导）

## Why

`src/error.rs:11-78` 当前 13 个 variant，覆盖了主要失败模式，但有 3 个具体问题：

1. **`Error::Other(String)` 是 catch-all**（line 77）。1705 个 unwrap 之外，至少十几处 `bail!(Error::Other("..."))` 散落在 storage、http、config 路径，调试时只能看到字符串。
2. **`Error::Tool` 缺 `call_id`**（line 25-26）。下游 session 持久化要把 tool 错误写入 transcript 对应位置时，无法对应到 `Message.tool_call_id`，导致 audit log 出现 orphan。
3. **`Error::Storage` 缺分类**（line 71-73）。S3 / Redis / 本地 IO 三种 storage 路径错误混在一个 variant 里，cloud 部署的故障定位全靠日志字符串。

## Design

### 1. 收窄 `Error::Tool`

```rust
Tool {
    name: String,
    call_id: Option<String>,    // ← 新增：与 Message.tool_call_id 对齐
    message: String,
}
```

迁移：所有 `Error::Tool { name, message }` 构造点同步加 `call_id`。`tools/mod.rs` 的 dispatch 路径有 call_id 上下文，直接传入；其他点用 `None`。

### 2. 删除 `Error::Other(String)`

把它替换为具体 variant：

| 旧用法 | 新 variant |
|---|---|
| `Other("storage s3: ...")` | `Error::StorageS3 { bucket: String, message: String }` |
| `Other("storage redis: ...")` | `Error::StorageRedis { op: String, message: String }` |
| `Other("storage local: ...")` | `Error::StorageLocal { path: PathBuf, message: String }` |
| `Other("config: ...")` | 已有 `Error::Config`（line 51-53） |
| `Other("...")` 其它 | `Error::Internal { context: String, message: String }`（带 source） |

`Error::Internal` 携带 `#[source]` 字段保留原始错误链，display 输出"`<context>: <message>`"。

### 3. `Storage` 分类

按 backend 类型拆：

```rust
Storage { backend: StorageBackend, op: String, message: String }
StorageS3 { bucket: String, key: String, message: String }
StorageRedis { url: String, op: String, message: String }
StorageLocal { path: PathBuf, message: String }
```

`StorageBackend` 是个 enum：`Local` / `S3` / `Redis`。这样 telemetry 端可以按 backend 分桶统计。

### 4. 不动的东西

- `Llm`、`RateLimited`、`ProviderTruncated` 不动
- `BadToolArgs`、`UnknownTool`、`PermissionDenied`、`Mcp`、`Timeout`、`Io`、`Http`、`Json` 不动
- `is_retryable` / `is_transient` 行为不变，扩展新 variant 时同步更新 match

## 验收标准

- `grep -rn "Error::Other" src/` 返回 0
- `grep -rn "Error::Storage {" src/` 返回 0（被分类 variant 替代）
- `Error::Tool` 构造点全部带 `call_id`（`grep -A2 "Error::Tool {" src/ | grep call_id`）
- `error.rs` 包含 Display + 单元测试覆盖每个新 variant 的字符串格式
- 现有调用点的 display 输出对用户而言无破坏性变化（向后兼容——这是给运维看的，不是给 LLM 看的）
- `cargo test --workspace` 全绿
- `cargo clippy --all-targets --all-features -- -D warnings` 干净

## 风险

- Cloud 部署的 panic-on-error 路径（`?` 操作符）如果有 unwrap，会被 Goal 224 的 deny 一并清理
- `thiserror` 输出的 Display 字符串变了，外部 CLI 输出可能微调——但 `agent_error` 字段在 `.dev/observations/*.md` 里仍是字符串，loop 不会受影响
