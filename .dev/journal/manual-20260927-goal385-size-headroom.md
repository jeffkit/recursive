# Journal — Goal 385 (kernel/runtime 部分) — invariant size headroom via test extraction

- **Date**: 2026-09-27
- **Goal**: `.dev/goals/385-invariant-size-headroom.md`（issue #26 / Goal 399 的硬前置）
- **Branch**: `feat/goal-399-wall-timeout`（commit 1）
- **Files touched**:
  - `src/kernel.rs` → 单元测试块整体移至 `src/kernel/tests.rs`（纯机械搬移，`use super::*` 语义不变）
  - `src/runtime.rs` → 同上，移至 `src/runtime/tests.rs`
- **Tests added**: 无新测试（纯搬移）；全量回归绿。

## 四个 guard 的行数变化

| Guard | 限制 | 之前 | 之后 | 备注 |
|-------|------|------|------|------|
| `src/kernel.rs` 总行数 | 1000 | 998（余 2） | **589**（余 411） | 目标 ≤920 达成 |
| `src/runtime.rs` 总行数 | 3700 | 3697（余 3） | **1476**（余 2224） | 目标 ≤3550 达成 |
| `RunCore::run_inner` body | 150 | 147（余 3） | 147（未动） | **385 此项目标未完成** |
| `src/run_core.rs` production | 1500 | ~1467（余 33） | ~1467（未动） | **385 此项目标未完成** |

## 说明

- 385 的 design principle check 明确允许「extract already-isolated helpers / **test modules**
  into sibling files」；本次只做 test-module 提取，零行为变化。两个文件的总行数 guard
  把 tests 计入，因此搬移即达成 kernel/runtime 两项的目标余量。
- `run_inner`（147→≤120）与 run_core production（~1467→≤1400）两项需要从 run_core.rs
  提取生产代码，超出本次（issue #26 解阻塞所需的）最小范围，**保持 385 在这两项上未完成**，
  留待后续 goal。Goal 399 不触碰 `src/run_core.rs`，不受这两项阻塞。
- 验证：`cargo test --workspace` 全绿（lib 818 + invariants 39）；clippy `-D warnings` 干净；fmt 干净。
