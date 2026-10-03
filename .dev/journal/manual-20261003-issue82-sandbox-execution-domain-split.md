# Manual — 2026-10-03 — issue #82 tools 沙箱/执行域拆层（#60 拆单 3/3）

Date: 2026-10-03
Goal: #82（#60 拆单 3/3，depends-on #80）
Branch: worktree `.flowcast/runs/pipeline-82-1003083212/worktree`

## Files touched

- 新建 `src/tools/execution/{mod,shell,edit,fs,glob}.rs` — 会话执行域工具
  （自 `src/tools/` `git mv`，含各自 `#[cfg(test)]` 测试）。
- 新建 `src/tools/transport_layer/{mod,transport,container_transport,container_provider,docker_provider,docker_sandbox,e2b_provider}.rs`
  — transport trait + 容器/docker/e2b 各层 provider（feature gate 不变）。
- 新建 `src/tools/policy_domain/{mod,policy,policy_sandbox,permission_pipeline,audit,url_guard}.rs`
  — 权限管线、L1 策略沙箱、审计元数据、SSRF URL guard。
  （命名 `policy_domain` 避免与子模块 `policy` 撞名。）
- `src/tools/mod.rs` — 删去上述模块声明，改为纯 re-export（同 #80/#81 兼容策略，
  `crate::tools::shell` 等旧路径继续解析）；registry/tool_kind/dispatch 留在 tools 根。
- 迁移文件内部 `super::` 引用改 `crate::tools::*`（edit/fs/glob/shell/permission_pipeline/docker_sandbox）。
- `tests/invariants/invariant_registry.rs` — `tool_files_are_registered_in_mod_rs`
  改为递归遍历 `src/tools/` 子目录，按所在目录的 `mod.rs` 校验注册。
- `tests/invariants/test_coverage.rs` — MUST_HAVE_TESTS 路径更新为 execution/。
- `tests/issue50_microvm_shared_vm_warning.rs` — `include_str!` 路径更新。
- docs：`execution-environments.md`、`tools/{index,filesystem,shell,search}.md`、
  `review/{02-tools-system,04-security-permissions,REVIEW_STATUS,architecture-review-2026-06-15}.md`
  源码路径同步。

## Tests added

- 无新增行为测试（纯搬家）；回归依赖既有套件：
  - `cargo test --workspace` 58 个 test target 全绿（lib 2481 通过）。
  - `cargo test --test invariants` 47 passed（含改写后的 invariant #9 校验）。
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings` 干净。
  - `cargo fmt --all` 已跑。

## Notes

- `policy` 名字冲突：`src/tools/policy.rs`（doc anchor）与目标目录 `policy/`
  同名，故域目录用 `policy_domain/`；mod.rs re-export 保持
  `crate::tools::policy` / `crate::tools::policy_sandbox` 兼容。
- `permission_pipeline.rs` 内部改 `use crate::tools::audit::AuditMeta` +
  `use crate::tools::ToolRegistry`；`docker_sandbox.rs` 改 `use crate::tools::Tool`。
- invariant #9 测试现在同时覆盖顶层与嵌套（execution/transport_layer/policy_domain）的
  Tool 文件注册（checked ≥ 20 断言保持）。
- 未改动 agent loop / registry 调度；无行为 diff。
