# Recursive 文档站 Review V2

**日期**：2026-06-03（第一轮反馈优化后）  
**审阅者**：小美

---

## 总体改进情况

第一轮 Review 的 P0/P1 问题解决率很高：
- ✅ 自改进循环页面：内容质量很好，步骤清晰
- ✅ Examples & Recipes 页面：有 4 个实用场景
- ✅ 首页 Feature Cards 完全重写，行动导向
- ✅ 版本号（v0.6.0）加到了首页
- ✅ SDK 包名修正，加了"如未发布则从源码安装"的提示
- ✅ multi-agent/ 空壳页面已删除
- ✅ Changelog 出现在侧边栏了

---

## 🚨 严重问题：文档 API 与实际源码不一致

这是本次 Review 最重要的发现，会直接导致用户跟着文档写代码时**编译失败**。

### 问题：`Agent::builder()` vs `AgentRuntime::builder()`

当前文档（quickstart.md、examples.md、library/agent.md）使用的 API：
```rust
use recursive::{Agent, ToolRegistry, ...};
let mut agent = Agent::builder()
    .llm(llm)
    .tools(tools)
    .max_steps(20)
    .build()?;
let outcome = agent.run("...").await?;
println!("{}", outcome.final_message.unwrap_or_default());
```

**实际源码（src/agent/mod.rs 注释明确说明）**：
> The legacy `Agent` / `AgentBuilder` / `AgentOutcome` / `OnMessageFn` / `StepEvent` types have been removed in Goal 219. Use `AgentRuntime` and `AgentEvent` instead.

正确的 API 应该是：
```rust
use recursive::{runtime::AgentRuntime, ...};
let mut runtime = AgentRuntime::builder()
    .llm(provider)
    .system_prompt("...")
    .max_steps(20)
    .build()?;
let outcome = runtime.run("...").await?;
println!("{}", outcome.final_text.unwrap_or_default());  // final_text, not final_message
```

**受影响页面**：
- `en/guide/quickstart.md`（整个 Rust 示例段）
- `en/guide/examples.md`（全部 4 个 Recipe）
- `en/library/index.md`（Minimal example）
- `en/library/agent.md`（全部代码）
- `en/guide/self-improve.md`（最后的 on_event 示例）
- 所有对应的 zh 页面

### 问题：`FinishReason` 枚举变体已变化

文档写的是：
```rust
match outcome.finish_reason {
    FinishReason::Done => { ... }          // ❌ Done 不存在
    FinishReason::Error(e) => { ... }     // ❌ Error 不存在
    _ => {}
}
```

实际的变体（来自 `src/agent/types.rs`）：
```rust
// 正确的变体
FinishReason::NoMoreToolCalls    // 原来的 Done
FinishReason::BudgetExceeded     // 还在
FinishReason::Stuck { repeated_call, repeats }  // 现在是结构体变体
FinishReason::ProviderStop(String)
FinishReason::TranscriptLimit { chars, limit }
FinishReason::PlanPending
FinishReason::Cancelled
```

### 问题：`StepEvent` → `AgentEvent`

`guide/self-improve.md` 里用了 `StepEvent` 的示例代码，但 StepEvent 已被移除，现在是 `AgentEvent`。

---

## 其他待改进点

### examples.md 链接到不存在的示例文件

文档说：
> All these recipes are available in the `examples/` directory — `cargo run --example code-review`

但实际的 `examples/` 目录里是：`basic.rs`、`with_hooks.rs`、`with_mcp.rs`、`with_skills.rs`、`with_tools.rs`

**没有 `code-review` example！** 需要：
- 要么删掉"可在 examples/ 目录找到"这句话
- 要么真的往 examples/ 里加这些 recipe

### 路线图状态与代码不符

`.dev/ROADMAP-v4.md` 里 Goal 219（删除 Legacy Agent）标记为 🔴（未开始），但实际源码已经执行了。路线图文档和代码状态脱节了，不是文档站的问题但值得注意。

---

## 不影响用户但值得跟进的

1. **首页仍无截图/GIF**：自改进 loop 跑起来的样子对潜在用户非常有吸引力，一张 asciinema 录像就能解决
2. **agui-* crates**：依然是个谜，需要等你决定要不要公开展示它
3. **zh/library/multi-agent.md 截断**：第一轮 Review 提到了，但不确定这次提交是否修复了，需要核实

---

## 修复优先级

| 优先级 | 问题 | 说明 |
|--------|------|------|
| P0 | `Agent` → `AgentRuntime`，`final_message` → `final_text` | 所有 Rust 代码示例需更新 |
| P0 | `FinishReason::Done`/`Error` → 实际变体名 | 编译不通过 |
| P0 | `StepEvent` → `AgentEvent` | self-improve.md 示例 |
| P1 | examples.md 中"可在 examples/ 找到"的说法 | 要么加文件要么删说法 |

---

*P0 项可以用 `cargo build --example ...` 或 `cargo test` 快速验证——所有文档中的代码都应该能跑通才算合格。*
