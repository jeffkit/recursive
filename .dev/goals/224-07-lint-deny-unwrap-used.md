# Goal 224 — Lint: 上线 `#![deny(clippy::unwrap_used, clippy::expect_used)]`

**Roadmap**: Phase 20 — 0.7 Refactor & Hardening
**依赖**: **Goal 229-01 .. 229-NN 全部完成**（清完 1705 处 unwrap/expect），以及 Goal 225、226、227、228 收尾
**类型**: C — 元策略/治理（self-improve 主导）
**执行位置**: v0.7 收尾 release 的最后一个 goal

## Why

`.dev/AGENTS.md` 第 5 条 invariant 自 v0.1 写起：

> "No `unwrap()` / `expect()` in non-test code. Return `Result`. The one exception is `client build` in `openai.rs` (infallible by construction)."

但实际：`grep -rn "unwrap()\|expect(" src/ | grep -v "#\[cfg(test)\]" | grep -v test_util` 返回 **1705 处**。

invariant 在文档里写六年了，clippy 没开 deny，loop 不强制，PR review 也漏了。这是"规范没有变成代码"的典型例子。

## Design

### 1. lib.rs 顶部加 deny

```rust
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
```

### 2. test paths 用 allow 局部放开

```rust
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    // ...
}
```

任何 `#[cfg(test)]` 模块都需要顶部加 `#[allow(...)]`（Rust 2018+ 支持 inner attribute on mod）。

### 3. test_util 模块放开

`src/test_util.rs` 自身可以 unwrap（它是测试基础设施），在文件头加 `// allow(clippy::unwrap_used, clippy::expect_used) on this module only`。

### 4. 文档化"为什么没有别的 exception"

在 `lib.rs` 顶部模块文档里写一段：

```
//! Lint policy:
//! - `unwrap()` / `expect()` 是 invariant 第 5 条禁止的。
//! - 测试代码通过 `#[cfg(test)] mod tests` 内的 `#![allow(...)]` 局部放开。
//! - `test_util.rs` 文件头 allow。
//! - 任何新 exception 必须在 PR 描述里显式说明，并加 `#[allow(clippy::unwrap_used, reason = "...")]` 携带 reason。
//! - Goal 224 上线后，`clippy --all-targets --all-features -- -D warnings` 在 CI 是 blocking。
```

### 5. CI / self-improve gate 升级

`.dev/scripts/self-improve.sh` 当前跑 `cargo clippy --all-targets -- -D warnings`，新版本要确保：unwrap_used 和 expect_used 必须出现在 deny 列表里。可以写成：

```bash
cargo clippy --all-targets --all-features -- \
    -D warnings \
    -D clippy::unwrap_used \
    -D clippy::expect_used
```

## 验收标准

- `src/lib.rs` 顶部含 `#![deny(clippy::unwrap_used, clippy::expect_used)]`
- `cargo clippy --all-targets --all-features` **零** unwrap_used / expect_used 警告
- 所有 `#[cfg(test)] mod tests` 都有内部 `#![allow(...)]`
- `test_util.rs` 头部有 allow
- 0 处 `unwrap()` / `expect()` 在 production 路径
- `self-improve.sh` 包含 deny lint 参数
- 一篇 journal entry 记录"为什么这个 goal 是 v0.7 的最后一锤"——它从此刻起保护代码不漂移

## 关键

这个 goal **不能**在 229 系列完成之前执行，否则整个仓库编译失败，loop 卡死。执行顺序在 ROADMAP-v4 Phase 20 里显式画出。
