# Goal 201 — Plan Mode Tools Opt-in (Remove from Default Registry)

**Roadmap**: Permission System V2 — Phase 3 运行时控制（质量加固）

**Design principle check**:
- 修改 `src/tools/mod.rs`：从 `default_tool_registry()` 中移除 plan mode 工具注册
- 修改 `src/runtime.rs`：`AgentRuntimeBuilder::build()` 保留 plan mode 工具注册（TUI/HTTP 渠道）
- ❌ 不修改 `agent.rs` 主循环

## Why

`enter_plan_mode` / `exit_plan_mode` 当前在 `default_tool_registry()` 中默认注册，
这意味着所有运行模式（CLI、headless、self-improve 脚本）都带有这两个工具。
在无交互渠道中（如 self-improve.sh 的 CLI 运行），LLM 调用 `exit_plan_mode` 后
会阻塞等待用户确认，永远无法推进。

正确模型：plan mode 工具属于**渠道能力（channel capability）**，
只有支持交互式 plan 审批的渠道（TUI、未来的 HTTP session）才应注入这两个工具。
CLI/headless 运行不加载它们，LLM 的工具列表里根本不存在，自然不会调用。

## Scope

### 1. `src/tools/mod.rs` — 从默认注册中移除

删除 `default_tool_registry()` 中的以下代码块（约 10 行）：

```rust
// Goal-165: plan mode 2.0 tools (NullSink / default gate placeholder).
// AgentRuntimeBuilder::build() re-registers these with the real gate and sink.
let default_gate = Arc::new(plan_mode::PlanApprovalGate::new());
registry = registry
    .register(Arc::new(plan_mode::EnterPlanModeTool::new(
        default_gate.clone(),
    )))
    .register(Arc::new(plan_mode::ExitPlanModeTool::new(
        default_gate,
        Arc::new(crate::event::NullSink),
    )));
```

`PlanApprovalGate` 的构造和 plan mode 工具注册完全交由 `AgentRuntimeBuilder` 负责。

### 2. `src/runtime.rs` — `AgentRuntimeBuilder` 保留注册（不变）

`runtime.rs` 中已经在 `build()` 里正确地重新注册了带真实 gate 和 sink 的
plan mode 工具（`src/runtime.rs:1171-1178`）。这部分**保持不动**。

只需确认：`AgentRuntimeBuilder::build()` 调用时 plan mode 工具被注册，
而直接使用 `default_tool_registry()` 的路径（self-improve CLI、单元测试）不会有这两个工具。

### 3. `src/main.rs` — 检查 build_standard_tools

检查 `build_standard_tools()` 是否直接依赖 plan mode 工具从 `default_tool_registry()`
拿到；若有，改为从 `AgentRuntimeBuilder` 路径获取（或显式不注册）。

### 4. 单元测试

- `default_registry_has_no_plan_mode_tools`：
  `default_tool_registry()` 返回的注册表中不包含 `enter_plan_mode` 和 `exit_plan_mode`
- `runtime_builder_has_plan_mode_tools`：
  通过 `AgentRuntimeBuilder::build()` 构建的注册表包含这两个工具（已有功能不回归）

## Acceptance

- `cargo test --workspace` 绿色
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- `default_tool_registry()` 不再包含 `enter_plan_mode` / `exit_plan_mode`
- `AgentRuntimeBuilder` 构建路径（TUI 使用）仍正确注册 plan mode 工具
- self-improve.sh 运行的 CLI agent 工具列表中不出现 plan mode 工具

## Notes for the agent

- 删除 `default_tool_registry()` 里的注册代码后，检查是否有其他地方
  直接调用 `default_tool_registry()` 并期望 plan mode 工具存在（grep `default_tool_registry`）。
- `runtime.rs` 中已有的 `re-register` 注释（Goal-165）说明这个设计早就意图
  让 runtime 覆盖默认值——现在只是把"覆盖"改为"唯一来源"。
- 若 `main.rs` 的某路径绕过了 `AgentRuntimeBuilder` 但又需要 plan mode，
  需要显式添加注册；但 self-improve/CLI headless 路径明确**不应该**有。
- **DO NOT call enter_plan_mode or exit_plan_mode.**
- **DO NOT modify `agent.rs::Agent::run` main loop.**
