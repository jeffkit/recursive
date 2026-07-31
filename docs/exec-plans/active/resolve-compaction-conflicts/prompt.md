# 恢复 Compaction 冲突 — Prompt

> 创建日期：2026-07-30 | 状态：active | 模式：单仓

## 目标

恢复当前工作区中 compaction 相关的未合并文件，使运行时只保留一套已验证的 compaction 实现，并恢复可提交状态。

## 完成标准

- [ ] `src/run_core.rs`、`src/runtime.rs` 与 `src/compact/runner.rs` 不再处于未合并状态。
- [ ] 不回退现有的 circuit-breaker、token-threshold 和 hook-balance 行为。
- [ ] `cargo fmt --all -- --check`、`cargo clippy --all-targets --all-features -- -D warnings`、`cargo test` 与 `cargo build` 通过。

## 非目标

- 不修改用户在 TUI、`AGENTS.md`、`CLAUDE.md` 或 journal 中的现有未提交工作。
- 不在本次恢复中进行新的架构重构。

## 已确认与假设

| 已确认 | 假设（低影响） |
| --- | --- |
| 用户要求先解决冲突。 | 采用当前工作目录中已通过全特性检查的 `Compactor` 直接实现，删除冲突的旧 `CompactionRunner`。 |

## 例外记录

此恢复发生在主分支已有的未合并索引上。为保留用户现有冲突上下文，不能切换至新的 worktree；本次只处理用户指定的三个冲突路径。
