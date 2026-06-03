# Goal 194 — 内存向量化：EmbeddingProvider + VectorStore + sqlite-vec

**Roadmap**: Phase 22.4 — 向量记忆层

**依赖**: Goal 182（LocalStorageBackend）, Goal 183（ToolSetProvider trait）

## Why

现有的 `remember` / `recall` 工具基于文件全文匹配，无法进行语义相似度检索。
引入向量检索后，agent 可以在大量记忆片段中高效找到语义相关的内容，而无需
独立部署向量数据库（sqlite-vec 是嵌入在进程内的 SQLite 扩展）。

## Design

### 两个新 trait

```
EmbeddingProvider  — 将文本转为嵌入向量（f32 数组）
VectorStore        — 存储 / 检索向量记忆片段
```

### 实现层级

| 层级 | EmbeddingProvider | VectorStore |
|------|-------------------|-------------|
| 本地 | `OpenAiEmbedding`（调 OpenAI embedding API） | `SqliteVecStore`（sqlite-vec 嵌入） |
| 无 embedding | `NoopEmbedding`（返回空向量，所有记录按时序召回） | `NoopVectorStore`（内存列表，线性扫描） |

### 不修改内核

`EmbeddingProvider` 和 `VectorStore` 通过现有的 `remember` / `recall` 工具
注入，不往 `AgentKernel` / `AgentRuntime` 添加字段。
工具层通过 `Arc<dyn VectorStore>` 共享状态。

## Files

- `src/memory/mod.rs` — trait 定义（`EmbeddingProvider`, `VectorStore`, `MemoryEntry`）
- `src/memory/noop.rs` — `NoopEmbedding`, `NoopVectorStore`（线性扫描 fallback）
- `src/memory/openai_embedding.rs` — `OpenAiEmbedding`（调 OpenAI text-embedding-3-small）
- `src/memory/sqlite_vec.rs` — `SqliteVecStore`（feature = "sqlite-vec"）
- `src/tools/remember.rs` — 更新：若 `VectorStore` 注入则向量化存储
- `src/tools/recall.rs` — 更新：若 `VectorStore` 注入则向量检索
- `Cargo.toml` — 新增 `sqlite-vec` 可选依赖 + `vector-memory` feature flag

## Scope limiter

- `SqliteVecStore` 仅在 `--features vector-memory` 下编译
- 默认 (`NoopVectorStore`) 不引入新依赖，不改变现有行为
- `EmbeddingProvider::embed()` 调用失败时 fallback 到 `NoopVectorStore` 行为（警告日志，不中断）

## 验收标准

- `cargo test --workspace` 全绿
- `cargo clippy -- -D warnings` 无警告
- `NoopVectorStore` 在默认 feature 下可用
- `SqliteVecStore` 在 `--features vector-memory` 下编译通过（可能无法在 CI 无 C 头的环境下跑全测试，用 `#[ignore]` 标记）
