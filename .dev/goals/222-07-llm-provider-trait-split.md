# Goal 222 — Refactor: 拆分 `LlmProvider` trait 为 `ChatProvider` + `EmbeddingProvider` + 扩展点

**Roadmap**: Phase 20 — 0.7 Refactor & Hardening
**依赖**: Goal 219（删 deprecated Agent 后，trait 表面积清晰）
**类型**: A — 架构级重构（人/Claude 主导）

## Why

`src/llm/mod.rs` 783 行定义的 `LlmProvider` trait 体积偏大（`grep` 显示 8+ 个 method），任何 provider 新增能力都会冲击所有实现：

- `llm/anthropic.rs` 1979 行
- `llm/openai.rs` 1166 行

实际今天的能力集合：
- **Chat / completion**（text 生成 + tool calls + stream）
- **Embedding**（text → vector）— Goal 194 已经在 `memory/` 单独定义了 `EmbeddingProvider`
- **Pricing / cost**（model → $/1k tokens）
- **Retry policy**（per provider 策略）

不同 provider 实现的子集不同：mock 只做 chat；openai/anthropic 做 chat + pricing；embedding 是独立子集。**把 chat 和 embedding 强行塞进一个 trait 是历史遗留**。

## Design

### 拆分后的 trait 层级

```
llm/
  mod.rs               ← ChatProvider trait（核心）+ 公共类型重导出（≤ 400 行）
  chat.rs              ← Completion、ToolSpec、ToolCall、StreamSender、TokenUsage（≤ 250 行）
  pricing.rs           ← ModelPricing、pricing_for、RetryPolicy（≤ 250 行）
  search.rs            ← 已存在（如果只是辅助函数，留原位）
  
memory/
  mod.rs               ← EmbeddingProvider trait（已在 Goal 194 定义）保留
  openai_embedding.rs  ← OpenAI embedding 实现（chat 实现里调它，但 trait 边界清晰）
  sqlite_vec.rs        ← VectorStore trait
  noop.rs              ← Noop 实现
```

### ChatProvider trait 形状（精简版）

```rust
#[async_trait]
pub trait ChatProvider: Send + Sync {
    fn name(&self) -> &str;
    fn model(&self) -> &str;
    fn pricing(&self) -> Option<&ModelPricing>;
    fn retry_policy(&self) -> RetryPolicy;
    
    async fn complete(&self, req: CompletionRequest) -> Result<Completion>;
    async fn stream(&self, req: CompletionRequest, tx: StreamSender) -> Result<Completion>;
}
```

`CompletionRequest` 合并当前散落在 `LlmProvider::complete` / `complete_with_tools` / `stream` 的参数。

### 保留 `LlmProvider` 作为 alias 期间的过渡

不保留。219 决定已经定调 breaking，222 直接换名。`grep -rn "LlmProvider" src/ tests/` 全替换为 `ChatProvider`。

### Provider 实现迁移

| 文件 | 改动 |
|---|---|
| `llm/anthropic.rs` | `impl LlmProvider` → `impl ChatProvider` |
| `llm/openai.rs` | 同上 |
| `llm/mock.rs` | 同上 |
| `tools/memory.rs` / `tools/episodic_recall.rs` | 已经走 `EmbeddingProvider`，不动 |
| `runtime.rs` / `kernel.rs` | 把 `Arc<dyn LlmProvider>` 改为 `Arc<dyn ChatProvider>` |
| `lib.rs` re-exports | `pub use llm::{ChatProvider, ...}` |

## 验收标准

- `grep -rn "LlmProvider" src/ tests/ examples/ crates/` 返回 0
- `llm/mod.rs` ≤ 400 行
- `llm/anthropic.rs` 由于 trait 表面积缩小，预期下降到 1500-1700 行（不是目标线，只是不再扩张）
- 新增 provider（哪怕是 mock）只需实现 4-5 个 method，不是当前 8+
- `cargo test --workspace` 全绿
- `cargo clippy --all-targets --all-features -- -D warnings` 干净

## Non-goals

- 不改 provider 内部 HTTP/SSE 协议
- 不改 model 名称、价格表、retry 策略
- 不动 `EmbeddingProvider`（已在 `memory/` 独立）
- 不动 `VectorStore`（Goal 194 边界）
