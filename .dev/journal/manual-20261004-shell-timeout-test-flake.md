# manual-20261004-shell-timeout-test-flake

## Date
2026-10-04

## Goal
`cargo test --workspace` gate red on `tools::execution::shell::tests::timeout_kills_child_process`
(高负载 flake，非本 pipeline 功能改动引入)。

## Root cause
测试用 150 ms 超时驱动 `RunShell`，再读子进程写下的 PID marker。
慢启动（`sh -c` fork+exec+echo 在高负载下 > 150 ms）时子进程可能在写 marker
**之前**就被 timeout 杀掉 —— 此时 marker 永远不会出现，上一次的「轮询等 marker」
缓解（issue #49）救不了这种情况：不是 t=0 竞争，是整段尝试作废。

## Files touched
- `src/tools/execution/shell.rs`（仅测试）：超时 150 ms → 500 ms；marker 竞争
  改为**有界重试**（最多 5 次场景重放，每次轮询 marker ≤1 s）。真回归
  （子进程根本没跑就死）永远拿不到 PID，仍在 `expect` 处响亮失败；
  泄漏回归（timeout 不杀子进程）仍被 `kill -0` 轮询抓住。断言强度不变。

## Tests added
无新增用例；修复既有用例的负载时序。

## Verification
- `cargo test -p recursive-agent --lib tools::execution::shell::tests::`：16 passed
- `cargo test --workspace`：58/58 二进制全绿，EXIT=0
  （含 `timeout_kills_child_process ... ok`）
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`：EXIT=0
- `cargo fmt --all -- --check`：clean

## Notes
- 无产品代码改动（transport 的 kill-on-timeout 语义本已正确）；无新增依赖。
