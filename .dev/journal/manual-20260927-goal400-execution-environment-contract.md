# Journal — Goal 400: 执行环境契约（capabilities + 失败分类）

- **Date**: 2026-09-27
- **Goal**: `.dev/goals/400-execution-environment-contract.md`（issue #27）
- **Run type**: manual（agent run，由 issue #29 的处理 run 顺带落地 —— #29 的 scope
  直接引用 `capabilities().toolchain` 与 `TransportFailure`，且当时 #27 无任何
  实现/认领，先落契约再接电是 goal 文档明确要求的顺序）
- **Branch**: `feat/goal-402-transport-walk`（commit 1/2）

## Files touched

- `src/tools/transport.rs`（唯一产品文件，符合 goal 的 `git diff --stat` 要求）
  - `EnvironmentCapabilities`（network / persistent / path_root / user / toolchain /
    snapshot）+ `EnvironmentCapabilities::local()`
  - `ToolTransport::capabilities()` 默认实现（返回 local 语义，既有实现与测试替身零改动）
  - `TransportFailure { Retryable, Environment, Tool }`
  - `ExecResult.failure: Option<TransportFailure>`（derive `Default`；三处既有构造点
    全部在本文件内，均补 `failure: None`，无外部构造点 —— `rg "ExecResult"` 复核）
  - 模块级**语义契约**成文：写后立即可读（write-then-read）、路径语义
    （trait 收环境内绝对路径；LocalTransport 的 `path_root` 为空 = 调用方解析的宿主路径）、
    `persistent: false` 的含义
- `src/tools/mod.rs`：re-export `EnvironmentCapabilities` / `TransportFailure`

## Tests added（`cargo test --lib transport` 可跑）

- `local_transport_capabilities_match_local_semantics`
- `capabilities_default_is_local_semantics`（BareTransport 替身不 opt-in 仍可用）
- `exec_result_default_has_no_failure`
- `transport_failure_variants_are_distinguishable`
- `mock_transport_surfaces_failure_classification`（FlakyTransport 经 trait 传递分类值）

## Acceptance 核实

- `cargo test --workspace` / `cargo clippy --workspace --all-targets --all-features
  -- -D warnings` / `cargo fmt --all` 全绿（在本 commit 状态单独验证过）
- `rg -c "EnvironmentCapabilities|TransportFailure" src/tools/transport.rs` = 14（≥ 8）
- `rg -c "写后立即可读|write-then-read|path_root"` = 8（契约成文）
- `git diff --stat`：本 commit 只含 `src/tools/transport.rs` + `src/tools/mod.rs`
  （mod.rs 是 re-export，无行为变更）+ 本 journal

## Notes

- `SshTransport` 未覆写 `capabilities()`（沿用默认 local 语义）。这是有意的最小改动：
  goal 明确「使所有既有实现与测试替身不需要立刻改」；SSH 档的精确能力（user、
  network 等）留给容器/远端档落地时一并补。
- GitNexus MCP 工具在本 run 环境不可用；影响面用 rg 手工复核（`ExecResult` 全仓构造点
  仅 transport.rs 内 3 处；消费方 shell.rs / run_background.rs / docker_sandbox.rs 均
  只读字段，新增字段无破坏）。
