# Goal 162 — TUI: backend.rs 拆分 + 完整工具集

**Roadmap**: Phase 11 — TUI 架构对齐

**Design principle check**:
- 改动范围：`crates/recursive-tui/src/backend.rs`（拆分）+ `src/lib.rs`（新增 `build_standard_tools`）
- ✅ 不改 AgentRuntime / Kernel 核心逻辑
- ✅ 不改任何 TUI UI 代码（app.rs / ui/ 目录）

## Why

`backend.rs` 目前混合了四件不同的事：

1. **运行时构建**（`build_runtime`、`build_default_tools`、`resolve_workspace_root`）
2. **事件桥接**（`worker_loop`、`wait_for_cancel`、`map_agent_event`）
3. **bash 直通**（`build_bash_registry`、`run_bash_command`）

更严重的问题：`build_default_tools` 只注册了 6 个工具（read_file /
write_file / apply_patch / list_dir / run_shell / search_files），
而 `src/main.rs` 的 `build_tools` 注册了 20+ 个工具（memory / facts /
episodic_recall / scratchpad / web_fetch / estimate_tokens 等）。
TUI 下运行的 agent 能力远不如 CLI，用户问 agent"你有哪些工具"时只看到 6 个。

根本原因：工具注册逻辑散落在两处且不一致，缺少一个权威的"标准工具集"定义。

## Scope (do exactly this, no more)

### 1. 在 `recursive` lib 中新增 `build_standard_tools`

在 `src/tools/mod.rs`（或 `src/lib.rs`）新增一个公开函数：

```rust
pub fn build_standard_tools(workspace: &std::path::Path) -> ToolRegistry
```

内容对齐 `src/main.rs` 的 `build_tools`（去掉 MCP 注册和 SubAgent，
那些属于渠道层）：

- read_file / write_file / apply_patch / list_dir
- run_shell（timeout 300s）
- search_files
- web_fetch
- run_background / check_background
- estimate_tokens
- remember / recall / forget（memory 版）
- remember / recall / forget / update_fact（facts 版，会覆盖 memory 版，
  这是现有行为，保持不变）
- episodic_recall
- scratchpad_set / scratchpad_get / scratchpad_delete / scratchpad_list
- load_skill / run_skill_script（如果 discover_loaded_skills 非空）

注意：`discover_loaded_skills` 和 `resolve_tool_permissions` 目前在
`main.rs` 里，`build_standard_tools` 接受一个可选的 skills 参数。
最简实现：skills 默认为空（TUI 暂不加载 skills），权限配置暂不传入。

### 2. 拆分 `backend.rs` 为三个文件

```
crates/recursive-tui/src/
├── backend.rs          ← 只保留 Backend struct + worker_loop + wait_for_cancel + map_agent_event
├── runtime_builder.rs  ← build_runtime + resolve_workspace_root，调用 recursive::build_standard_tools
└── bash.rs             ← build_bash_registry + run_bash_command
```

`backend.rs` 原有的 `build_default_tools` 删除，改为在
`runtime_builder.rs` 里调用 `recursive::build_standard_tools`。

### 3. 更新 `mod.rs`

在 `crates/recursive-tui/src/lib.rs` 或 `main.rs` 中补充两个新模块：

```rust
mod runtime_builder;
mod bash;
```

### 4. 测试

- 现有测试全部继续通过（`build_default_tools` 的测试迁移到
  `runtime_builder.rs`）
- 新增一个单元测试：`build_standard_tools` 返回的 registry 包含
  `web_fetch`、`remember`、`episodic_recall`（验证工具集不再残缺）

## Acceptance

1. `cargo test --workspace` green
2. `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean
3. `cargo build -p recursive-tui` 成功
4. `backend.rs` 不再包含任何工具注册逻辑（grep `register` 返回空）
5. `build_standard_tools` 在 `recursive` lib 中 pub 可用
6. TUI 启动后 agent 可以使用 `web_fetch`、`remember`、`episodic_recall` 等工具

## Notes for the agent

- `src/main.rs` 的 `build_tools` 是工具注册的参考实现，`build_standard_tools`
  的内容对齐它（跳过 MCP、SubAgent、ScheduleWakeup 这三项）
- `discover_loaded_skills` 在 `main.rs` 里，暂不搬到 lib；
  `build_standard_tools` 的 skills 参数类型用 `&[recursive::skills::Skill]`，
  TUI 调用时传 `&[]`
- `web_fetch` 依赖 `web_fetch` feature flag，`Cargo.toml` 里已有，
  确保 `recursive-tui` 的依赖启用了这个 feature
- `BackgroundJobManager` 需要 `Arc<tokio::sync::Mutex<...>>` 共享，
  在 `build_standard_tools` 内部创建并返回，或通过参数传入——
  选最简方式
- **不要改 UI 代码**（app.rs / ui/ 目录）
- **不要改 AgentRuntime**
- memory 和 facts 工具名冲突（`remember`/`recall`/`forget` 各有两个实现）
  是现有行为，facts 版后注册会覆盖 memory 版——保持这个行为不变，
  不需要在本 goal 里修复
