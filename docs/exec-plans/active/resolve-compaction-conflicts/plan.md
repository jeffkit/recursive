# 恢复 Compaction 冲突 — Plan

> 基于：[prompt.md](./prompt.md)

## Scope Record

- Mode: single-repo (`/Users/kongjie/projects/Recursive`)
- E2E capable: true（存在 Dockerfile 与 ArgusAI `e2e/e2e.yaml`）
- Exec-plan dir: `docs/exec-plans/active/resolve-compaction-conflicts/`
- Feature name: `resolve-compaction-conflicts`

## 架构决策

保留当前工作目录的单一 `Compactor` 路径：`RunCore` 与 `AgentRuntime` 各自持有可选 compactor 和失败计数器，避免再引入已删除的 `CompactionRunner` 作为第二套编排层。该版本已包含钩子配对、阈值、circuit breaker 与事件语义测试。

## 里程碑拆解

### M1：合并 compaction 三方版本

**目标：** 将三个未合并索引条目收敛为工作目录已验证的 `Compactor` 实现，并删除旧 runner 文件。

**IFU 边界：** 输入为 stage 1/2/3 版本和当前工作目录；输出为已解决索引且仅涉及这三个冲突路径。

任务：

- [ ] 复核三方差异与工作目录实现。
- [ ] 以 `Compactor` 直接路径解决 `run_core.rs` 与 `runtime.rs`。
- [ ] 标记 `runner.rs` 删除并验证不再有 `CompactionRunner` 引用。

**验收标准：**

```bash
git diff --name-only --diff-filter=U
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo build --all-features
```

**对抗审查点：** compaction hook 配对、紧急压缩语义、circuit breaker、旧类型残留与冲突范围。

**E2E 覆盖：** not-needed。
**E2E 判定依据：** E2E Protocol Step B 的“内部重构，接口不变 → NO”；本里程碑不修改 API 或 UI 外部行为。
**E2E 场景：** n/a；由单元、集成和全特性编译验证覆盖。
