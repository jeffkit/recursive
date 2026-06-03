# Goal 229 — Refactor: `unwrap()/expect()` 批量消解（多 batch 模板）

**Roadmap**: Phase 20 — 0.7 Refactor & Hardening
**依赖**: Goal 219（先删 deprecated，再消解 unwrap——避免双修旧路径）
**类型**: B — 机械消解（self-improve 主导，单 batch 一次循环）
**执行模式**: 拆分为 N 个子 goal（229-01, 229-02, ...，N 估为 8-15），每个 batch 处理 ~50-100 处违规

## Why

`grep -rn "unwrap()\|expect(" src/ | grep -v "#\[cfg(test)\]" | grep -v test_util` → **1705 处**。

self-improve loop 单 budget 200 step × 2 = 400 step。即使每 step 修 1 处 unwrap，也需要 4-5 个完整 batch。考虑 agent 的 apply_patch 失败率（"Stuck on three identical failing tool calls"），单 batch 上限应设为 **50-100 处**。

## Why multi-batch 模板

旧版 `110b-memory-layer0-complete.md` 已经用过字母后缀（`b` 表示"补全"）。本 goal 用数字后缀（`01`、`02`、...）是因为：

- 字母后缀暗示"补全/重做"语义
- 数字后缀暗示"批次序列"语义，更准确
- 与 ROADMAP-v4 的"Batch N" 风格对齐

## Design: 单 batch goal 模板（229-01 为例）

```markdown
# Goal 229-01 — Unwrap cleanup batch 01

**Roadmap**: Phase 20 — 0.7 Refactor & Hardening
**父 goal**: Goal 229
**依赖**: Goal 219 已合并

## Scope

- 处理 src/agent.rs、src/runtime.rs、src/kernel.rs 中的 N 处 unwrap/expect
- 用 `?` / `ok_or_else(|| Error::Xxx { ... })?` 替换
- 不能立即替换的加 `#[allow(clippy::unwrap_used, reason = "...")]` 携带 reason

## Acceptance

- `cargo test --workspace` 全绿
- `cargo clippy --all-targets -- -D warnings` 干净（当前尚未启用 unwrap_used deny；用 `cargo clippy --all-targets -- -W clippy::unwrap_used` 走 warn 路径验证）
- 本 batch 净减少 ≥ 50 处违规

## 输出

- 修改的文件列表
- 剩余违规总数（应在 1705 - 50 = 1655 左右）
```

## 实际 11 类违规分布（基于 grep 计数）

| 文件 | 违规数 | 备注 |
|---|---|---|
| `src/agent.rs` | 91 | Goal 219 删掉 50% 以上 |
| `src/session.rs` | 95 | Goal 221 拆分后分散 |
| `src/runtime.rs` | 68 | 含 goal/sandbox init 路径 |
| `src/mcp.rs` | 47 | protocol 反序列化热路径 |
| `src/tui/app.rs` | 12 | 220 拆分后大减 |
| `src/llm/openai.rs` | ~30 | 222 拆分后减少 |
| `src/llm/anthropic.rs` | ~30 | 同上 |
| `src/tools/*.rs` | ~600 | 散落在 28 个 tool 文件中 |
| 其它 | ~700 | 配置、路径、compat 路径 |

## batch 切分建议（初版）

- **229-01**: `agent.rs` + `runtime.rs` + `kernel.rs`（~150 处）
- **229-02**: `session.rs`（~95 处）
- **229-03**: `mcp.rs`（~47 处）
- **229-04**: `tui/app.rs`（~12 处）
- **229-05**: `llm/openai.rs`（~30 处）
- **229-06**: `llm/anthropic.rs`（~30 处）
- **229-07-12**: `tools/*.rs` 按 alphabet 切（每批 ~50 处）
- **229-13-15**: 杂项（config / paths / 其它）

15 个 batch 是粗略估计。loop 实际跑时按违规密度动态切。

## inline allow 策略

不能立即替换的（例如：parser 内部 fallback、明确的 invariant 检查点）使用：

```rust
#[allow(clippy::unwrap_used, reason = "invariant: tool spec is non-empty by construction")]
let name = spec.name.as_str();
```

`reason` 必填——这是给未来的 reviewer 看的"为什么这条 escape 是合理的"。

## Goal 224 上线条件

- 229-01 到 229-NN 全部合并到 main
- 主仓 `grep -rn "unwrap()\|expect(" src/ | grep -v "#\[cfg(test)\]" | grep -v test_util` ≤ 50（剩余都是带 reason 的 allow）
- 224 把 deny 上线，把 `#[allow(reason = "...")]` 保留——Goal 224 不清残留 allow，只确保没有"裸" unwrap

## Non-goals

- 不改 `test_util.rs`（它本就该 unwrap）
- 不动 `#[cfg(test)] mod tests` 内部的 unwrap（这些由 Goal 224 的 `#[allow]` 处理）
- 不强求所有 unwrap 改 `?`——`expect("invariant: ...")` 也是合法形式
