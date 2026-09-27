# Goal 405 — microVM 档：设计成文 + E2B 适配接线（不自建 Firecracker）

**Roadmap**: milestone 批次 5（收尾）。为「不可信代码 / 强多租户」提供硬件级隔离档，
但**不在本 goal 自建 microVM 平台**。

**依赖**: Goal 403（容器档实现与选择入口）、Goal 404（会话级绑定）。

**Design principle check**:
- Implemented as: (a) 把执行环境的档位、能力矩阵、平台需求写成设计文档；
  (b) 把已有 `E2bProvider`（`src/tools/e2b_provider.rs`）适配到 Goal 400 的
  `ToolTransport` 契约，接到 `--sandbox=microvm`（`e2b-sandbox` feature 门控）。
- ❌ Does NOT 自建 Firecracker/Cloud Hypervisor 集群、镜像体系、warm pool、快照编排。
- ❌ Does NOT 新增外部依赖（E2B 走 REST + 已有 `reqwest`）。
- ❌ Does NOT 把 microVM 设为默认档，也不承诺「安全」（只界定爆炸半径；
  出网与凭据策略留给下一个 milestone）。

## Why（2026-09-27 核实）

- `src/tools/e2b_provider.rs` 已存在且头注释明确写着「E2B Firecracker microVM-backed
  `ToolSetProvider` (L3 sandbox)…hardware-isolated, <150ms cold start」，
  配置项 `RECURSIVE_E2B_API_KEY` / `RECURSIVE_E2B_TEMPLATE` / `RECURSIVE_E2B_TIMEOUT_SECS`
  / `RECURSIVE_E2B_API_BASE`（`:1-15`）。
- 但 `e2b-sandbox = []`（`Cargo.toml:44`）是一个**空 feature**，且与 `ToolSetProvider`
  一样**没有任何前端接线**（`rg "E2bProvider" crates/ src/http/` = 0 命中）。
- 也就是说：microVM 档今天等于「有一份未接线的第三方适配器 + 一个空的 feature 名」。
  本 goal 把它变成「可被选择的一条档位」，并用文档划清自建平台的边界。

## Scope（do exactly this, no more）

### 1. 设计文档 `docs/architecture/execution-environments.md`

必须包含：

- **档位矩阵**：`none` / `policy` / `container` / `microvm(remote)`，每档给出
  「隔离维度覆盖」表（文件系统、进程/内核、网络、资源、数据/租户、时间/快照）
  与「密度量级」（本地实测：in-process ~50 KB/会话；容器档数十 MB 起；VM 档 GB 级），
  并说明**为什么默认必须是 none**（密度是产品差异点）。
- **能力矩阵**：每档的 `EnvironmentCapabilities`（Goal 400 的字段）典型取值，
  以及能力如何影响 agent 行为（网络关闭时提示注入、工具链缺失时的降级路径）。
- **自建 microVM 的需求清单与判据**（明确写「本 milestone 不做」）：
  guest kernel/rootfs/init 的维护、`/dev/kvm` 前提、网络（TAP + NAT/CNI）、
  存储（CoW rootfs + virtio-fs）、快照/恢复、warm pool、镜像分层（参考仓库 e2e 已用的
  cargo-chef 分层思路）、以及为什么这些构成一个**独立平台产品**而非 agent 仓库的内部工作。
- **决策判据**：什么时候该选容器档（默认推荐）、什么时候值得付 microVM 的密度代价
  （不可信代码、多租户、需要快照/回滚），以及「先用托管（E2B/同类）验证需求」的路径。
- **非目标**：出网策略、凭据 broker、审计——列为下一个 milestone。

### 2. E2B 适配到 transport 契约

- 为 `E2bProvider`/其资源实现 `ToolTransport`：
  - `exec_shell` → E2B 的 commands API；
  - `read_file` / `write_file` / `list_dir` / `create_dir_all` → E2B filesystem API；
  - `walk`（Goal 402 引入）→ 环境内 `find`；
  - `capabilities()` → 如实填写（`network` 按 E2B 沙箱实际配置、`persistent: true`、
    `path_root` 用 E2B 的默认工作目录、`user`、`toolchain` 由探测/模板决定、
    `snapshot: false`——除非实现时确认可用）。
- 接线：`e2b-sandbox` 启用且 `--sandbox=microvm`（或 `RECURSIVE_SANDBOX=e2b`）时，
  经 `ToolSetProvider` 构造会话环境（Goal 404 的绑定路径），**未启用 feature 时给出
  清晰的报错信息**（而不是静默回退到本地执行）。
- 会话超时：E2B 沙箱有 TTL（`RECURSIVE_E2B_TIMEOUT_SECS`，默认 3600）；
  必须与宿主层的会话 TTL 协同（环境过期时要么续期、要么让会话明确失败，
  **不允许静默降级到宿主执行**）。

### 3. 测试（agent-presence / agent-mutants 门）

- 单测：`E2bProvider` 的 capability 映射与配置解析（`RECURSIVE_E2B_*`，env 测试合并为一个）；
- 单测：请求构造（用本地 mock HTTP 服务或已有测试替身模式，
  **注意 `.dev/AGENTS.md` 的网络测试陷阱**：`reqwest` 无默认超时，
  测试必须显式设 `timeout` + `connect_timeout`，否则会挂死整个 `cargo test`）；
- 门控集成：`RECURSIVE_TEST_E2B_API_KEY` 存在时才跑真沙箱往返（skip-if-absent），
  并在 journal 里贴出真实输出（若本地无 key，如实记录「未执行」）；
- 编译门：`cargo build --features e2b-sandbox` 与默认构建都必须绿（CI 无 key 也要过）。

## Files NOT to touch

- `src/run_core.rs`、`src/kernel.rs`、`src/runtime.rs`。
- `src/tools/container_transport.rs`（403 的范围）。
- `src/tools/docker_*`（403 的范围）。
- 既有 e2e 套件与 fixture。

## Acceptance

- `docs/architecture/execution-environments.md` 存在，且含上述四类内容
  （档位矩阵 / 能力矩阵 / 自建需求与判据 / 非目标）。
- `cargo test --workspace`、`cargo clippy --workspace --all-targets --all-features -- -D warnings`、
  `cargo fmt --all` 全绿；`cargo build --features e2b-sandbox` 绿。
- Grep: `rg "SandboxMode::MicroVm" src/ crates/` 出现**消费点**（不再只有 provider 自产）。
- Grep: `rg "RECURSIVE_E2B_" src/ crates/ README.md` 覆盖解析 + 文档。
- Journal: `.dev/journal/manual-20260927-goal405-microvm-tier.md`，
  含「未自建 Firecracker」的明确声明与理由。

## Notes for the agent (traps)

- **不要把「接了 E2B」写成「我们有 microVM 档了」**：托管沙箱意味着控制面在外部，
  文档必须写清依赖（可用性、配额、计费、数据出境）与自建路线的差距。
- **不要在没有 key 的环境里让测试失败**：门控 + skip 是正确的；但也不要写一个永远
  跳过的测试来充数——journal 必须如实记录是否真的执行过。
- **E2B 的 TTL 与会话 TTL 是两套时钟**：设计时明确谁负责续期/回收，
  否则会出现「会话还在、环境没了」的半死状态。
- 本 goal 的文档是给**未来决策**用的：判据要能被验证（数字、前提、失效条件），
  不要写成宣传材料。

