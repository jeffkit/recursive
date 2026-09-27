# Goal 404 — 会话级环境绑定：生命周期归宿主层 + 能力注入系统提示 + 后台任务随环境销毁

**Roadmap**: milestone 批次 4 收尾。让「执行环境」从「一个可选的工具集合」变成
**会话的一个属性**——这是沙箱真正可用的最后一块。

**依赖**: Goal 395（宿主层）、Goal 403（容器档实现）。建议 394（会话级工具状态）已落。

**Design principle check**:
- Implemented as: 环境句柄由 `SessionHost`（395）按会话创建/销毁；会话开始时若
  `sandbox != none`，构造环境并把其 transport 注入该会话的工具注册表；
  会话驱逐/关闭时销毁环境（含其中运行的后台任务）。
- ❌ Does NOT 改默认档的 prompt 或行为（`sandbox = none` 时系统提示**逐字节不变**）。
- ❌ Does NOT 实现跨会话共享环境或环境池（warm pool 属 Goal 405 之后的平台工作）。
- ❌ Does NOT 改 `resolve_within`（invariant #3）。

## Why（2026-09-27 核实）

- 环境今天无处安放：`DockerShellTool` 把容器挂在**工具实例**上
  （`src/tools/docker_sandbox.rs:40-60`，「Each instance owns one container that is cleaned
  up on `Drop`」）。而 `ToolRegistry::clone`/`fork_session` 会复制工具 Arc 句柄
  （`src/tools/registry.rs:104-111`）——在共享注册表的架构下，容器生命周期与「会话」
  没有任何绑定关系：fork 出的会话会共享或丢失容器，两者都是错的。
- 后台任务同样是进程级/注册表级的：`BackgroundJobManager` 由
  `build_standard_tools_with_roots` 构造（`src/tools/registry.rs:683-687`），
  `run_background` 启动的进程今天跑在**宿主**上。容器档下必须跑在环境内，且环境销毁时
  必须被终止，否则要么泄漏进程、要么任务在宿主上越界执行。
- 能力对模型不可见：模型不知道自己有没有网络、根目录在哪、镜像里有没有 `cargo`。
  结果是它靠试错发现（实测：LLM 请求体里带 26 个工具 schema，模型会直接尝试
  `cargo build` 然后浪费 step 处理「命令不存在」）。把 capabilities 写进提示是
  零成本的行为修正——但必须只在启用环境时注入，否则会改动 replay fixture。

## Scope（do exactly this, no more）

### 1. 会话 ↔ 环境绑定

- 在 `SessionHost`（Goal 395）的会话创建路径上：当 `sandbox != none` 时，
  调用 `ToolSetProvider`（`src/tool_set_provider.rs:35-45`）构造该会话的 transport，
  并用 `ToolRegistry` 的 transport 注入点（Goal 401/402 建立的构造路径）生成
  **该会话专属的** registry。
- 在驱逐/关闭/删除会话路径上：`environment.destroy().await`（在锁外，遵守 395 的锁范围
  规则），确保容器/进程/临时资源全部回收；失败只记日志，不中断驱逐。
- 会话创建失败（镜像拉取失败、docker 不可用）必须返回明确错误（HTTP 503/500 的语义
  按现有 `ApiError` 形状选一个），并在 metrics/日志里可见——不要让会话处于
  「存在但没有环境」的半死状态。

### 2. capabilities 注入系统提示（仅环境档）

- 复用既有 `PromptSegments`（`src/system_prompt.rs:74-86`）结构与
  `assemble_system_prompt`（`:105`），新增一个 segment（例如
  `environment: String`），内容形如：

  ```
  <environment>
  network: off | persistent: yes | root: /workspace | user: agent
  available: cargo, node, rg, git
  </environment>
  ```

- **硬要求**：`sandbox == none` 时该 segment 为空字符串，且最终 system prompt
  与今天**逐字节相同**（用快照测试 pin：对同一 config 断言 prompt 相等）。
- 不在提示里暴露宿主路径、凭据或内网地址。

### 3. 后台任务随环境走

- `run_background` 的进程必须通过**该会话的 transport** 启动（而不是 `tokio::process`
  直连宿主），并在环境销毁时被终止（容器销毁即终止；本地档沿用现有 kill 语义）。
- `BackgroundJobManager` 的实例必须是**会话级**（Goal 394 已让它 per-session；
  本 goal 只需确认它随环境销毁时被 drain/cleanup）。
- 子代理（`src/tools/agent.rs`）：默认**继承父会话的 transport**（不新开环境），
  并在 journal 记录该选择；独立环境（每个子代理一个沙箱）是后续能力。

### 4. 可观测

- 会话维度新增 gauge/日志：当前活跃环境数、环境创建失败数（接 Goal 392 的 metrics 面）。
- 启动日志打印生效的 sandbox 档与镜像（便于运维核对）。

### 5. 测试（agent-presence / agent-mutants 门）

- 单测：`sandbox = none` → 注入后的 system prompt 与基线**完全相等**（快照/字符串相等）。
- 单测：`sandbox = container`（用假 transport/provider，不起真容器）→
  segment 出现在 prompt 里且包含 capabilities 的关键字段。
- 单测：会话销毁调用 `destroy()`，且即使 `destroy()` 返回错误也不影响其他会话
  （与 395 的驱逐测试同风格）。
- 单测：后台任务通过会话 transport 启动（假 transport 记录调用），
  会话销毁后 manager 为空。
- 门控集成（`RECURSIVE_TEST_DOCKER=1`）：容器档下 `run_background` 起的进程
  在容器内可见，会话删除后不可见。

## Files NOT to touch

- `src/run_core.rs`、`src/kernel.rs`、`src/runtime.rs`。
- `src/tools/policy_sandbox.rs` 的规则、`src/tool_set_provider.rs` 的既有档位语义
  （只消费，不重定义）。
- e2e 既有 fixture（prompt 不变是硬要求，改了 fixture 就等于掩盖回归）。

## Acceptance

- `cargo test --workspace`、`cargo clippy --workspace --all-targets --all-features -- -D warnings`、
  `cargo fmt --all` 全绿。
- 默认档 prompt 不变：快照测试绿 + `sh .dev/scripts/e2e-run.sh 00-smoke` 与
  `01-basic-tools` 通过（replay，无需 key）。
- 新测试按名可跑：`cargo test --lib environment_binding`（journal 记录确切名）。
- Grep: `rg "destroy\(\)" src/session_host.rs src/tools/container_transport.rs` 有命中
  （创建/销毁成对）。
- Journal: `.dev/journal/manual-20260927-goal404-session-environment-binding.md`，
  含默认档 prompt 相等的证据（diff 为空）。

## Notes for the agent (traps)

- **prompt 不变是硬约束**：e2e 是 replay 模式，prompt 变化会让 fixture 失配。任何
  「顺手把环境信息加进默认提示」都会破门禁。只在环境档注入。
- **销毁路径必须幂等**：会话可能因超时、删除、关停三条路径被销毁；`destroy()` 重复调用
  必须安全（容器不存在 = 成功）。
- **不要给每个子代理开新环境**：容器冷启动（业界 100 ms–数秒，含镜像拉取可能几十秒）
  会让宽 manifest 的并行子代理变成灾难。默认继承，独立环境留给显式的分叉能力。
- **环境创建失败不要静默降级到宿主执行**：那等于悄悄取消隔离。必须失败并报告
  （这是安全语义，不是可用性取舍）。

