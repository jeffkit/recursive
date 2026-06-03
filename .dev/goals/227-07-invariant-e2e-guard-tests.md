# Goal 227 — Tests: 8 条 AGENTS.md invariant 各配 e2e 守护测试

**Roadmap**: Phase 20 — 0.7 Refactor & Hardening
**依赖**: Goal 219、220、221、222 完成（结构稳定后加测试才有意义）
**类型**: C — 元策略/治理（self-improve 主导）
**执行**: 一组并行 goal（227-01 到 227-08），每个对应一条 invariant

## Why

`.dev/AGENTS.md` 第 50-95 行的 8 条 invariant 写在文档里六年了。每条都对应一个"如果被违反，loop 应该自动回滚"的检查。但实际只有 invariant #8 有专门的回归测试（`compaction_keeps_tool_calls_paired_with_results`）。

剩下 7 条 invariant 靠人工 review 守护——这就是 v0.4→v0.6 三轮 refactor 累积出 god module 的根因之一。

## Design

### 8 条 invariant 与对应测试

| # | Invariant | 测试形态 | 文件 |
|---|---|---|---|
| 1 | Agent loop stays small | shell 脚本 + cargo test：`assert!(std::fs::metadata("src/agent.rs")?.len() < 200_000 /* ~600 行 */)` | `tests/invariants/loop_size.rs` |
| 2 | Orthogonality | 编译时检查：`tools/` 模块不能 import `crate::llm::*`（除 traits） | `tests/invariants/orthogonality.rs` |
| 3 | Sandbox | e2e：所有 fs/shell 工具拒绝 `../` path traversal；`tools::resolve_within` 拒绝 symlink escape | `tests/invariants/sandbox.rs` |
| 4 | Tests are non-negotiable | 每个新 `pub fn` 必须出现在同一文件的 `#[cfg(test)] mod tests` 里——通过 coverage 工具 + 自定义 lint 检查 | `tests/invariants/test_coverage.rs` |
| 5 | No unwrap/expect in non-test | 已被 Goal 224 替代为 clippy deny | (224 覆盖) |
| 6 | No new deps without justification | CI 检查：Cargo.toml 改动必须有 `.dev/journal/` 同 PR 内的 journal entry 解释 | `scripts/check-new-deps.sh` |
| 7 | Finish reasons are data, not errors | 单元测试：所有 `FinishReason` 变体都映射到 `Ok(outcome)`，没有 `Err` 路径返回 `FinishReason::*` | `tests/invariants/finish_reason_data.rs` |
| 8 | Tool-call ↔ tool-result pairing | 已有；扩展为：对每种 transcript mutation（compaction, trim, replay, resume）跑一遍 | `tests/invariants/tool_call_pairing.rs` |

### 子 goal 拆分

- **227-01**: invariant #1（loop size）+ #2（orthogonality）编译期检查
- **227-02**: invariant #3（sandbox）e2e
- **227-03**: invariant #4（test coverage）cargo-llvm-cov 集成
- **227-04**: invariant #6（dep justification）CI 脚本
- **227-05**: invariant #7（finish reason data）单元测试
- **227-06**: invariant #8（tool-call pairing）扩展 e2e

（invariant #5 已被 Goal 224 覆盖，跳过。）

## 验收标准

- 8 条 invariant 中，6 条有自动化检查（#1、#2、#3、#4、#6、#7、#8——其实是 7 条，但 #5 由 224 兜底）
- `cargo test --workspace` 全绿，新 invariant 测试 count 至少 +6
- `self-improve.sh` 失败模式：当 invariant 测试 fail 时，loop 自动 rollback（与 clippy 失败同等地位）
- 每个 invariant 测试文件头有文档化：`# Why this test exists:` 段落引用 `.dev/AGENTS.md` 对应行号
- `.dev/AGENTS.md` 第 50-95 行不变（这些 invariant 本身不动），但每条 invariant 后面追加一行："Automated test: `tests/invariants/X.rs`"

## Non-goals

- 不动 `.dev/AGENTS.md` 的 invariant 文字
- 不改测试覆盖率的"100% line coverage"目标（cargo-llvm-cov 是工具，不是 KPI）
- 不引入新的 testing framework（继续用 std + tempfile + mockito）
