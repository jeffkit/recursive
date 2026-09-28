# Goal 403 — `ContainerEnvironment`：容器档执行环境（rootless 容器 + 严格限额）与选择入口

**Roadmap**: milestone 批次 4。执行面契约（400）与接电（401/402）之后，落第一个
真正提供隔离的实现档。

**依赖**: Goal 400、401、402（工具必须已经走 transport，否则容器档只隔离了 Bash）。

**Design principle check**:
- Implemented as: 新增实现 `ToolTransport` 的容器环境（复用已有 `bollard`，
  经 `cloud-runtime`/新 feature 门控），并把 `SandboxMode::Container` 接到 CLI/HTTP 的
  选择入口；默认档仍是 `None`。
- ❌ Does NOT 让容器成为默认（密度优势必须保留：实测 in-process ~50 KB/会话 vs
  容器档数十 MB 起）。
- ❌ Does NOT 实现镜像体系 / warm pool / 预热快照（Goal 405 的设计范围，本 goal 只用
  一个可配置镜像）。
- ❌ Does NOT 新增外部依赖（`bollard` 已在 `Cargo.toml` 的 `cloud-runtime` feature 下）。
- ❌ Does NOT 声称「安全」：本 goal 只界定爆炸半径（见「已知限制」）。

## Why（2026-09-27 核实）

- 抽象已在：`SandboxMode::{None, Policy, Container, MicroVm}`
  （`src/tool_set_provider.rs:16-39`）、`ToolSetProvider`（`:35-45`）、
  `DockerShellTool`（`src/tools/docker_sandbox.rs:40-60`：容器 per instance、Drop 清理、
  512 MB / 1 CPU、bind-mount workspace、`sh -c`）。
- **但没有任何前端接线**：`rg "ToolSetProvider|DockerSandboxProvider" crates/ src/http/` = 0 命中；
  `SandboxMode` 只有 provider 自己产出 + `lib.rs` 再导出；CLI 走的是
  `build_standard_tools_with_roots`（进程内本地工具）。`[sandbox]` 配置段只描述
  **路径 roots**（`src/config_file.rs:37-41`），不是执行隔离。
- 现状风险：今天若把 `Bash` 放进容器而 `Read`/`Write`/`Edit` 留在宿主（401 不做的后果），
  会出现「容器里跑命令、宿主上写文件」的分裂行为——比完全不隔离更难排查。

## Scope（do exactly this, no more）

### 1. 容器环境的 transport 实现

新文件 `src/tools/container_transport.rs`（或 `src/tools/env/container.rs`）：

- 实现 `ToolTransport` 的全部方法：
  - `exec_shell` → `bollard` 的 `create_exec` / `start_exec`（已有先例：
    `src/tools/docker_sandbox.rs` 的用法可直接参考）。
  - `read_file` / `write_file` / `list_dir` / `create_dir_all` → 经 `exec_shell`
    用 `cat`/`tee`/`ls` 实现，**或**用 bollard 的 archive/upload API（实现前先读
    `docker_sandbox.rs` 与 bollard 版本能力，选一条并在 journal 说明；注意二进制安全：
    `cat` 路线对二进制文件不安全，若走它必须 base64 或改用 archive API）。
  - `walk`（Goal 402 引入）→ 环境内 `find`（或 `rg --files`），输出解析必须在
    402 的格式一致性测试下通过。
  - `capabilities()` → `network` 由配置决定、`persistent: true`、
    `path_root: /workspace`、`user: Some("agent")`、`toolchain` 由启动时探测
    （`which cargo node rg git` 之类）填入、`snapshot: false`（本 goal 不做）。
- **生命周期**：一个环境对应一个容器；容器的创建/销毁由调用方持有（Goal 404 会交给
  宿主层），本 goal 至少保证 `Drop`/显式 `destroy()` 不泄漏容器（沿用
  `docker_sandbox.rs` 的 Drop 清理模式）。

### 2. 安全基线与限额（写死默认，可配置）

- 非 root 用户、`cap_drop: ALL`、`no_new_privileges`、`pids_limit`、
  `memory`（默认 1 GB）、`nano_cpus`（默认 1.0）、`readonly_rootfs`（能开则开，
  并把 workspace 与 `/tmp` 作为可写挂载）。
- 网络：默认 `network_mode: none`；`RECURSIVE_SANDBOX_NETWORK=on` 才放行
  （包安装需要网络，这是有意的摩擦——让使用者显式决策）。
- 镜像：`RECURSIVE_SANDBOX_IMAGE`（默认给一个写明"需自备工具链"的保守镜像名，
  在文档里说明 coding agent 需要 cargo/node 等，建议用户自建基础镜像）。
- 容器运行时：默认连本地 Docker/Podman socket；
  `DOCKER_HOST` 尊重既有环境变量（rootless podman 兼容性在 journal 里记录实测结果）。

### 3. 选择入口（CLI + HTTP）

- CLI：`--sandbox <none|policy|container|microvm>`（env `RECURSIVE_SANDBOX`），
  默认 `none`；实现方式接到既有 `ToolSetProvider` / `SandboxMode` 抽象，
  **不要新增平行开关**。
- HTTP：`RECURSIVE_SANDBOX=container` 时，会话创建时构造容器环境（每会话一个），
  未设置时行为与今天**逐字节一致**。
- `policy` 档复用已有 `PolicyToolSetProvider`/`policy_sandbox.rs`（纯 Rust 层校验）
  ——本 goal 只需把它接上开关，不必改其行为。

### 4. 已知限制（必须写进文档与 journal，不得含糊）

- **数据不隔离**：本 goal 用 bind-mount 共享宿主 workspace ⇒ 容器内仍可破坏工作区。
  隔离的是**宿主 OS 与进程边界**（无出网、无多余能力、非 root、限额）。
  工作区级隔离（overlayfs / CoW / 快照）是后续 goal，且是「并行候选探索」能力的前提。
- 文件读取经 exec 会有性能损失；大文件与 `walk` 是已知放大点。

### 5. 测试（agent-presence / agent-mutants 门）

- 单测：`capabilities()` 的值与配置一致；`walk` 的解析函数对固定 `find` 输出
  （作为 fixture 字符串）给出与 `LocalTransport` 相同的结果（不必真起容器）。
- 门控集成测试：`tests/sandbox_container.rs`，**未设置 `RECURSIVE_TEST_DOCKER=1`
  时跳过**（沿用 `RECURSIVE_TEST_REDIS_URL` 的 skip-if-absent 模式）。设置后验证：
  - 容器内 `pwd`/写入/读取一致；
  - 网络默认不可达（`curl`/`wget` 失败）；
  - 非 root（`id -u != 0`）；
  - 销毁后容器不存在（`docker ps -a` 无残留）。
- e2e：若 `e2e/` 的容器环境支持 docker-in-docker（需要挂 socket），再加一个套件；
  **若不可行，在 journal 里明确记录并保留门控集成测试**——不允许写一个永远 skip 的
  e2e 充当证据。

## Files NOT to touch

- `src/run_core.rs`、`src/kernel.rs`、`src/runtime.rs`。
- `src/tools/fs.rs` / `edit.rs` / `glob.rs` / `search.rs` 的工具逻辑（接电已完成）。
- `src/tools/policy_sandbox.rs` 的校验规则（`policy` 档只接线）。
- e2e 既有套件与 fixture（新增可以，改不行）。

## Acceptance

- `cargo test --workspace`、`cargo clippy --workspace --all-targets --all-features -- -D warnings`、
  `cargo fmt --all` 全绿（含 `--no-default-features` 与启用容器 feature 的组合，
  与 CI 既有 feature 组合检查保持一致）。
- 默认档不变证明：`cargo test --test smoke` / 既有 HTTP 集成测试全绿；
  `git diff` 不包含默认行为变化。
- 门控集成测试可跑：`RECURSIVE_TEST_DOCKER=1 cargo test --test sandbox_container -- --nocapture`
  （journal 贴输出，包含 `id -u`、网络不可达、销毁无残留三条证据）。
- Grep: `rg "SandboxMode::Container" src/ crates/` 出现**消费点**（不再只有 provider 自产）。
- Journal: `.dev/journal/manual-20260927-goal403-container-environment.md`，
  含镜像选择、限额、rootless 实测、已知限制。

## Notes for the agent (traps)

- **e2e 里跑 Docker 可能不可行**：`e2e-run.sh` 本身在容器里跑，docker socket 未必可用。
  先探测（`docker info`），不可行就走门控集成测试路线，并在 journal 里如实记录——
  不要为了「有 e2e」而写一个永远跳过的套件。
- **二进制安全**：走 `cat`/`tee` 的读写路线会破坏二进制文件与 CRLF/编码。优先用
  bollard 的 archive API；若只能用 exec，必须 base64 且显式测试二进制往返。
- **限额写死默认值不要过小**：512 MB 对 `cargo build` 很容易 OOM（业界实测峰值 2-4 GB）。
  默认 1 GB 起步，并在文档里说明如何按负载调整；OOM 的表现是容器被杀、工具报错，
  不要把它归类为 `Tool` 失败（Goal 400 的分类里它属于 `Environment`）。
- **不要顺手接 warm pool / 快照**：那是 Goal 405 的设计范围。

