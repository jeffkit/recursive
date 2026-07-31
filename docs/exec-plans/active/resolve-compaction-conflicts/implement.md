# 恢复 Compaction 冲突 — Implement

> 基于：[plan.md](./plan.md)

## M1：合并 compaction 三方版本

### Decisions

- 保留工作目录中已验证的直接 `Compactor` 实现；它保留 token 阈值、失败熔断与 `PreCompact`/`PostCompact` 配对。
- 解析为删除 `src/compact/runner.rs`，避免同时维护旧 `CompactionRunner` 与直接实现两套编排路径。

### Problems & Solutions

- 三次实现角色会话均在开始前因平台连接中断。为避免继续阻塞，编排者只执行了不改写源码的 index 恢复：对现有工作目录版本执行 `git add`，并以 `git rm` 标记已删除的 runner。

### Outcome

- 冲突路径已清零；仅 `src/run_core.rs` 与 `src/runtime.rs` 被暂存为修改，`runner.rs` 按当前 HEAD 的删除状态解决。
- 验证通过：`cargo fmt --all -- --check`、`cargo test`（2081 passed，2 ignored）、`cargo clippy --all-targets --all-features -- -D warnings`、`cargo build --all-features`。
- 未提交：恢复发生在用户已有的 `main` 工作区，且未授权提交。

## M1：修复对抗审查 f1

### Decisions

- 在 emergency compaction 路径派发 `PreCompact` 前调用 `Compactor::would_compact`；不可压缩的 transcript 直接返回 `Ok(false)`，保持 hook 生命周期成对。
- 回归测试使用 `keep_recent_n=8` 的九条消息（older slice 仅含 system message），同时记录 hook 事件和 `MockProvider` 调用，覆盖审查指出的 no-op 边界。

### Problems & Solutions

- Problem: `apply_to_transcript` 可在 `PreCompact` 已派发后返回 `Ok(None)`，使 hook 留下未配对的开始状态。→ Solution: 在派发任何 hook 或调用 provider 前以 `would_compact` 拒绝该 transcript。

### Outcome

- 新增 `compact_on_overflow_rejects_degenerate_transcript_without_hook_events`：验证返回 `false`、无 `PreCompact`/`PostCompact` 且 provider 未调用。
- 验证通过：`cargo fmt --all -- --check`、focused runtime test、`cargo test`（2082 passed，2 ignored）、`cargo clippy --all-targets --all-features -- -D warnings`、`cargo build --all-features`。
- 未提交：按授权保留当前分支和用户工作区状态。
