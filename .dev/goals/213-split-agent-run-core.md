# Goal 213 — 拆分 src/agent.rs：提取 RunCore 到独立模块

**Roadmap**: 代码健康 — 大文件专项整治（第二批）

**设计原则检查**:
- 纯代码组织重构，运行时行为不变
- 只新建 `src/run_core.rs`，不改其他文件逻辑
- 通过 `pub(crate) use` 保持所有调用方无需改动
- ❌ 不修改任何公开 API

## 背景

`src/agent.rs` 当前 **3521 行**，包含三类职责：

| 行范围 | 内容 | 是否应拆 |
|--------|------|---------|
| 1–989 | `RunCore<'a>`、`RunInnerOutcome`、`run_inner()` 核心循环 | ✅ 应提取 |
| 991–1436 | 已废弃的 `Agent` legacy 包装层（`#[deprecated]`） | 保留原处 |
| 1437–3521 | `AgentBuilder` + tests | 保留原处 |

`RunCore<'a>` 是真正的执行内核，约占全文件 1/3（~650 行），独立性强，
完全不依赖 `Agent`。提取后 `agent.rs` 可减至 ~2900 行。

## 目标

新建 **`src/run_core.rs`**，从 `agent.rs` 中迁移 `RunCore<'a>` 相关代码，
保持所有调用方零修改。

## 实施细节

### 1. 新建 `src/run_core.rs`

从 `src/agent.rs` 剪切以下内容（原样搬移，包含所有注释）：
- `pub(crate) struct RunInnerOutcome { ... }`
- `pub(crate) struct RunCore<'a> { ... }`
- `impl<'a> RunCore<'a> { ... }`（包含 `run_inner`、`emit`、
  `maybe_trim_transcript`、`maybe_compact`、`execute_tool_calls` 等所有方法）

文件顶部需要的 `use` 语句：从 `agent.rs` 的顶部 imports 中找出
`RunCore` 实际使用的依赖（`Arc`, `AtomicBool`, `mpsc`, `Compactor`,
`PermissionHook`, `HookRegistry`, `PlanningMode`, `OnMessageFn`,
`LlmProvider`, `Message`, `ToolRegistry`, `StepEvent`, `ToolCall`,
`FinishReason`, `TokenUsage`, `CancellationToken`, 等），
只 import 真正需要的，不要盲目复制全量 use 列表。

### 2. 在 `src/lib.rs` 声明新模块

```rust
pub(crate) mod run_core;
```

（`run_core` 是 crate 内部模块，不需要 pub。）

### 3. 在 `src/agent.rs` 顶部加重导出

```rust
pub(crate) use crate::run_core::{RunCore, RunInnerOutcome};
```

删去已迁移的类型定义，保留这一行 re-export。
这样所有已有的 `use crate::agent::RunCore` 调用方零修改。

### 4. 保留 `src/agent.rs` 中的 import 清理

迁移完成后，`agent.rs` 顶部可能有部分 `use` 仅用于已迁移代码。
运行 `cargo build` 后，根据 unused-import 警告清理这些无用 import。
（不要手动猜测，等编译器告诉你哪些可以删。）

## 验收标准

1. `cargo build --all-features` 通过，零警告
2. `cargo test --workspace` 全绿
3. `cargo clippy --all-targets --all-features -- -D warnings` 干净
4. `cargo fmt --all -- --check` 干净
5. `src/agent.rs` 行数 **≤ 2900**
6. `src/run_core.rs` 存在，包含 `RunCore` 和 `RunInnerOutcome`
7. 任何已有对 `RunCore` 的引用（如 `src/kernel.rs`、`src/runtime.rs`）
   无需修改即可编译通过

## 明确不在范围内

- ❌ 不删除废弃的 `Agent` struct（保留向后兼容）
- ❌ 不拆分 `AgentBuilder`
- ❌ 不修改任何测试的逻辑
- ❌ 不改变 `pub` 可见性

## 注意事项

- `RunCore<'a>` 是 `pub(crate)` 的生命周期参数化结构，迁移时保持 `pub(crate)` 可见性
- `run_inner()` 消费 `self`（`pub(crate) async fn run_inner(mut self)`），迁移后签名不变
- `src/kernel.rs` 和 `src/runtime.rs` 可能直接构造 `RunCore`，迁移后通过 re-export 仍然 OK
