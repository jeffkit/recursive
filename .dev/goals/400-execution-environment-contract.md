# Goal 400 — 执行环境契约：在既有 `ToolTransport` 上补齐 capabilities 与错误分类

**Roadmap**: milestone 批次 3（执行面）第一步。为「同一份 agent 逻辑跑在不同隔离档」提供
唯一接缝——**不新增第三套抽象**。

**依赖**: 无（纯新增 + 测试，不改任何工具的调用路径）。

**Design principle check**:
- Implemented as: 扩展 `src/tools/transport.rs` 的 `ToolTransport` trait（capabilities +
  失败分类 + 语义契约文档），并保留 `LocalTransport` 为默认实现。
- ❌ Does NOT 新建平行的 `ExecutionEnvironment` trait（该接缝已存在：
  `ToolTransport`，且 `ToolRegistry` 已持有 `Arc<dyn ToolTransport>`，
  `src/tools/registry.rs:111`）。
- ❌ Does NOT 改变任何工具的 I/O 路径（读/写仍走 `tokio::fs`，那是 Goal 401/402 的事）。
- ❌ Does NOT 引入新依赖。

## Why（2026-09-27 核实）

- trait 已存在且形状正确（`src/tools/transport.rs:50-77`）：`read_file` / `write_file` /
  `list_dir` / `create_dir_all` / `exec_shell`（含 cwd / env / timeout / max_output_bytes）。
- 模块文档（`src/tools/transport.rs:4-9`）声称「Tools that need I/O (`ReadFile`, `WriteFile`,
  `ListDir`, `RunShell`) call methods on this trait instead of using `tokio::fs` / `tokio::process`
  directly」——**这是不成立的**：`rg "transport" src/tools/fs.rs` = 0 命中，
  `rg "transport" src/tools/shell.rs` = 0 命中。今天只有 `registry.rs` 持有这个 Arc。
- 也就是说：**接缝已经埋好但没有接电**（这正是 401/402 的入口），而本 goal 要在接电之前
  把契约补齐，否则 401/402 会把「宿主直读」换成「容器/远端直读」时，把错误处理与
  能力假设硬编码进去。
- 现状缺什么：`exec_shell` 只返回 `std::io::Result<ExecResult>`，调用方无法区分
  「VM 超时/网络抖动（可重试）」「环境缺工具（需要换策略）」「命令本身失败（模型该修代码）」。
  Agent 侧只能看到统一的 tool error，会把基础设施故障当成代码问题去「修」，白烧 step。

## Scope（do exactly this, no more）

### 1. `EnvironmentCapabilities`

新增（同文件）：

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentCapabilities {
    pub network: bool,          // 沙箱内是否有出网
    pub persistent: bool,       // 多次 exec 之间文件系统/进程是否保持
    pub path_root: PathBuf,     // 模型可见的根（本地 = workspace，容器 = /workspace 之类）
    pub user: Option<String>,   // 以什么身份执行（None = 未知）
    pub toolchain: Vec<String>, // 探测到的工具（cargo/node/rg/git…），未探测为空
    pub snapshot: bool,         // 是否支持快照/克隆（容器/VM 档）
}
```

- `ToolTransport` 增加 `fn capabilities(&self) -> EnvironmentCapabilities;`
  （给默认实现，返回 `LocalTransport` 的语义：`network: true, persistent: true,
  path_root: 空 PathBuf 表示「调用方注入的 root」, snapshot: false`），
  使所有既有实现与测试替身不需要立刻改。

### 2. 失败分类

新增：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportFailure {
    /// 瞬时故障：VM 超时、网络抖动、限流。调用方可重试，agent 不应改代码。
    Retryable,
    /// 环境问题：镜像缺工具、路径不存在、权限不足。agent 需要换策略或报告用户。
    Environment,
    /// 命令本身失败（非 0 退出等）。这是模型的正常反馈。
    Tool,
}
```

- `ExecResult` 增加字段 `failure: Option<TransportFailure>`（`LocalTransport` 恒为 `None`）。
- 保持 `exec_shell` 签名不变（返回 `std::io::Result<ExecResult>`），避免牵动
  `src/tools/shell.rs` / `run_background.rs` 的所有调用点。分类由实现填充，调用方
  （Goal 401/402/403）负责消费。
- 若发现 `ExecResult` 有构造点在别处（先 `rg "ExecResult"`），一并更新并保持默认值语义。

### 3. 语义契约（写进 doc comment，作为 401-404 的规范）

明确写下并在测试里 pin 的契约：

- **写后立即可读**：`write_file` 返回成功后，紧接着的 `read_file` 必须读到新内容
  （本地与容器档都必须成立；push/pull 型远端若不成立，必须在其 capabilities/文档里标注，
  并由工具层插入 barrier）。
- **路径语义**：trait 收到的是**宿主侧解析后的绝对路径**，还是**环境内路径**？
  本 goal 必须给出唯一答案并写清（推荐：trait 收环境内路径，`resolve_within` 在调用方
  按 `capabilities().path_root` 做映射——这条决定会直接影响 401 的实现，不允许含糊）。
- **`persistent: false` 的含义**：每次 `exec_shell` 可能是新进程/新文件系统，
  工具层不得依赖跨调用状态（cwd/env/临时文件）。

### 4. 测试（agent-presence / agent-mutants 门）

- 单测：`LocalTransport::capabilities()` 的值符合上述契约。
- 单测：假 transport 返回 `failure: Some(Retryable)` → 消费方能区分（本 goal 只测
  「值可传递、可比较」，消费逻辑在 401/402）。
- 单测：`ExecResult` 的默认构造（若新增 `Default`）`failure == None`。
- 文档测试不需要，但 `rg` 可验证契约文本存在（见 Acceptance）。

## Files NOT to touch

- `src/tools/fs.rs`、`src/tools/shell.rs`、`src/tools/glob.rs`、`src/tools/search.rs`
  （接电是 401/402；本 goal 只动 `transport.rs`）。
- `src/tool_set_provider.rs` 与 `SandboxMode`（档位选择是 403）。
- `src/tools/dispatch.rs` 的 `resolve_within`（invariant #3）。
- `src/run_core.rs`、`src/kernel.rs`、`src/runtime.rs`。

## Acceptance

- `cargo test --workspace`、`cargo clippy --workspace --all-targets --all-features -- -D warnings`、
  `cargo fmt --all` 全绿。
- 新测试按名可跑：`cargo test --lib transport`.
- Grep: `rg "EnvironmentCapabilities|TransportFailure" src/tools/transport.rs | wc -l` ≥ 8
  （类型 + 方法 + 实现 + 测试）。
- Grep: `rg "写后立即可读|write-then-read|path_root" src/tools/transport.rs` 能命中契约文本
  （中文或英文均可，关键是契约成文）。
- **行为不变证明**：`git diff --stat` 只包含 `src/tools/transport.rs`（若不得不碰
  `ExecResult` 的构造点，把这些点列进 journal 并说明为何无法避免）。
- Journal: `.dev/journal/manual-20260927-goal400-execution-environment-contract.md`。

## Notes for the agent (traps)

- **不要新建 `ExecutionEnvironment` trait**：评审与后续 goal 都以 `ToolTransport` 为接缝。
  如果认为它命名不当，正确做法是**改名 + 保留别名**，而不是并存两套。
- **不要在这一步顺手把 `ReadFile` 改成走 transport**：那会让 `fs.rs`、`Edit` 的
  read-before-edit 语义、`ReadFileState` 缓存与 sandbox invariant 一次性全动，
  是 401 的范围。本 goal 必须能独立回滚。
- `path_root` 的语义决定是本节最容易被含糊带过的点——它直接决定 401 里
  `resolve_within` 与容器 `/workspace` 的映射方式。宁可在这里多写 10 行文档，
  也不要在 401 里边写边猜。
- 不要新增依赖（invariant #6）；本 goal 全部可用 std + 既有类型完成。

