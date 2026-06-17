# Manual edit: Phase 20 剩余目标推进

**Date**: 2026-06-17
**Goal**: 完成 Phase 20 (v0.7 Refactor & Hardening) 的全部剩余 goals
**Files touched**:
- `.dev/goals/229-02-unwrap-cleanup-transport-shell.md` (新增 goal 规格)
- `.dev/goals/229-03-unwrap-cleanup-tui-backend.md` (新增 goal 规格)
- `.dev/goals/229-04-unwrap-cleanup-misc-small.md` (新增 goal 规格)
- `src/lib.rs` (Goal 224: 加 deny lint)
- `src/llm/mock.rs`, `src/test_util.rs` (test infra allow)
- 11 个生产文件 (Goal 229-04: 加 #[allow] 注解)

**Tests added**: `tests/invariants/*.rs` (6 个不变量测试，Goal 227 by loop)

**完成情况**:

| Goal | 方式 | Verdict | Commit |
|------|------|---------|--------|
| 227 — 8 条 invariant E2E 守护测试 | self-improve (97 步) | committed | c4ce732 |
| 229-02 — transport.rs + shell.rs unwrap 清理 | self-improve (179 步) | committed | fb8aee1 |
| 229-03 — tui/backend.rs 23 处 unwrap | self-improve (46 步) | committed | bd72b5b |
| 229-04 — misc 小文件 unwrap (~30 处) | 手工 (loop stuck:Edit:8) | committed | 9af4952 |
| 224 — deny(unwrap_used, expect_used) | 手工 (与 229-04 合并) | committed | 9af4952 |

**BLOCKED**:
- Goal 226 (recursive-tui 子 crate 抽取): Cargo 循环依赖 — `recursive-tui` 依赖 `recursive-agent`，`recursive-agent` 可选依赖 `recursive-tui`，形成 package-level cycle。需要在 v0.8 重新设计（选项：提取 `recursive-core` 作为中间 crate，或放弃真正的 crate 抽取，改用更强的 feature gate）。

**Notes**:
- Goal 224 用 `#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]` 一行自动覆盖所有 `#[cfg(test)]` 模块，避免了手工给 143 个测试文件加 allow
- `lock().unwrap()` 统一保留（mutex poison 是不可恢复的），加 `#[allow(..., reason = "mutex poison is unrecoverable")]`
- reqwest client build 的 `expect` 保留（TLS 不可用是致命启动错误），加 `#[allow(..., reason = "TLS backend unavailable is fatal")]`
- Goal 229-04 的 self-improve loop 因 Edit 工具要求"先读完整文件"而 stuck，下次可以在 goal spec 里提示 agent 先 `read_file` 再 `apply_patch`
