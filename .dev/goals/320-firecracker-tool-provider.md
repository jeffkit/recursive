# Goal 320 — FirecrackerToolSetProvider: 本地 Firecracker MicroVM ToolSetProvider (L3-local)

**Roadmap**: Phase 21 — 轻量沙箱层（存算分离 L3-local）

**Design principle check**:
- 新增 `src/tools/firecracker_provider.rs`，实现 `ToolSetProvider` trait
- ❌ 不修改 `agent.rs` 主循环
- ✅ 正交性：不影响现有 L0/L1/L2/E2b 层
- ✅ 平台守卫：`#[cfg(target_os = "linux")]` + feature flag `firecracker-sandbox`

## Why

现有 L3 沙箱（E2bToolSetProvider）依赖 e2b.dev 云 API，需要外网 + API Key。

对于以下场景需要**本地 Firecracker**：
1. **自建数据中心**（on-premise SaaS）
2. **高密度 To C 场景**（数千并发 agent，成本敏感）
3. **数据合规场景**（代码不能出境）

Firecracker 特性：
- VMM 进程本身 < 5 MB 内存开销
- ~125ms 冷启动（有内存快照则 < 10ms）
- KVM 强隔离（独立内核，攻击面最小）
- 支持**共享内核快照 + CoW rootfs**（批量 agent 高密度关键）

## 前置条件（运行时，非构建时）

- Linux 宿主机，KVM 可用（`/dev/kvm` 存在）
- Firecracker 二进制在 PATH 中（或通过 `RECURSIVE_FIRECRACKER_BIN` 指定）
- 一个 rootfs 镜像（基础 Alpine 或 Ubuntu + busybox）
- 一个内核镜像（Linux vmlinux，建议 5.10+）

> 在 macOS 上构建不报错（feature 可编译），但运行时 `build_registry()` 返回
> `KvmUnavailable` 错误，上层可降级到 DockerToolSetProvider。

## Scope（精确做这些，不多不少）

### 1. Firecracker API 客户端（Unix socket REST）

Firecracker 通过 Unix domain socket 暴露 REST API。实现以下最小子集：

```rust
/// Client for the Firecracker VMM REST API (Unix socket transport).
pub struct FirecrackerApiClient {
    socket_path: PathBuf,
    client: reqwest::Client,  // 使用 reqwest 的 Unix socket 支持
}

impl FirecrackerApiClient {
    /// PUT /machine-config
    pub async fn set_machine_config(&self, vcpus: u32, mem_mib: u32) -> Result<()>;
    /// PUT /boot-source  
    pub async fn set_boot_source(&self, kernel_path: &Path, boot_args: &str) -> Result<()>;
    /// PUT /drives/rootfs (root block device)
    pub async fn set_rootfs(&self, rootfs_path: &Path, read_only: bool) -> Result<()>;
    /// PUT /vsock (virtio vsock device)
    pub async fn set_vsock(&self, cid: u32, uds_path: &Path) -> Result<()>;
    /// PUT /actions (InstanceStart)
    pub async fn start(&self) -> Result<()>;
    /// GET /
    pub async fn describe_instance(&self) -> Result<InstanceInfo>;
}
```

> `reqwest` 支持 Unix socket via `reqwest::Client::builder().unix_socket(path)` (Linux only)。
> 如果 reqwest 版本不支持，改用 `hyper` + `hyperlocal` crate。

### 2. Vsock 命令执行协议

VM 内部运行一个极简 init 脚本，通过 vsock 接受 JSON 命令：

**宿主侧发送** (via Unix socket `<uds_path>`):
```json
{"cmd": "exec", "command": "ls /", "timeout_secs": 30}
{"cmd": "read_file", "path": "/workspace/main.rs"}
{"cmd": "write_file", "path": "/workspace/main.rs", "content_b64": "<base64>"}
{"cmd": "list_dir", "path": "/workspace"}
```

**VM 侧响应**:
```json
{"exit_code": 0, "stdout": "...", "stderr": "..."}
{"content_b64": "..."}
{"entries": ["a.rs", "b.rs"]}
```

> 注意：这个协议定义在 goal 文档里是为了让实现可预测。
> VM 内部的 agent 是一个简单的 busybox ash 脚本或小型 Rust 程序，
> **本 goal 只实现宿主侧**；VM 侧 agent 假设已存在于 rootfs 中。

### 3. `FirecrackerVm` 生命周期管理

```rust
pub struct FirecrackerVm {
    config: FirecrackerConfig,
    /// Firecracker process handle.
    process: tokio::process::Child,
    /// API client over the control socket.
    api: FirecrackerApiClient,
    /// Vsock UDS path for command execution.
    vsock_path: PathBuf,
}

impl FirecrackerVm {
    /// Spawn a new Firecracker process and boot the VM.
    pub async fn spawn(config: FirecrackerConfig) -> Result<Self>;
    /// Execute a shell command in the VM via vsock.
    pub async fn exec(&self, command: &str, timeout_secs: u64) -> Result<ExecResult>;
    /// Read a file from the VM filesystem.
    pub async fn read_file(&self, path: &Path) -> Result<Vec<u8>>;
    /// Write a file to the VM filesystem.
    pub async fn write_file(&self, path: &Path, content: &[u8]) -> Result<()>;
    /// List a directory in the VM.
    pub async fn list_dir(&self, path: &Path) -> Result<Vec<String>>;
}

impl Drop for FirecrackerVm {
    fn drop(&mut self) {
        // Send SIGTERM to the Firecracker process.
    }
}
```

### 4. Tool 实现

通过 `Arc<FirecrackerVm>` 包装的工具（复用 `DockerShellTool` 的模式）：

- `FirecrackerBashTool` — 通过 vsock 执行 shell 命令
- `FirecrackerReadTool` — 通过 vsock 读取 VM 文件
- `FirecrackerWriteTool` — 通过 vsock 写入 VM 文件

名称分别为 `"Bash"`、`"Read"`、`"Write"`（与标准工具一致，透明替换）。

### 5. `FirecrackerToolSetProvider`

```rust
/// Configuration for the Firecracker tool provider.
pub struct FirecrackerConfig {
    /// Path to the Firecracker binary (default: looks up PATH).
    pub binary_path: PathBuf,
    /// Path to the guest Linux kernel image (vmlinux or bzImage).
    pub kernel_path: PathBuf,
    /// Path to the rootfs block device image.
    pub rootfs_path: PathBuf,
    /// Number of vCPUs (default: 1).
    pub vcpus: u32,
    /// Guest RAM in MiB (default: 128).
    pub mem_mib: u32,
    /// Shell command timeout in seconds (default: 60).
    pub shell_timeout_secs: u64,
}

pub struct FirecrackerToolSetProvider {
    config: FirecrackerConfig,
    skills: Vec<crate::skills::Skill>,
}

impl ToolSetProvider for FirecrackerToolSetProvider {
    fn build_registry(&self) -> ToolRegistry {
        // 1. Check KVM availability.
        // 2. Spawn FirecrackerVm (blocking via block_in_place like DockerProvider does).
        // 3. Build standard registry.
        // 4. Replace Bash/Read/Write with Firecracker variants.
        // If KVM unavailable or spawn fails, return Err propagated to caller.
    }

    fn sandbox_mode(&self) -> SandboxMode { SandboxMode::MicroVm }
}
```

### 6. KVM 可用性检测

```rust
/// Returns true if /dev/kvm exists and is accessible.
pub fn kvm_available() -> bool {
    std::path::Path::new("/dev/kvm").exists()
}
```

### 7. Feature flag + Cargo.toml

在 `[features]` 中添加：
```toml
firecracker-sandbox = ["dep:reqwest-unix-socket"]  # 或等价 crate
```

如果 reqwest 的 unix socket 支持通过 feature flag 控制，使用相应配置。

如果需要额外 crate（如 `hyperlocal`），在 journal entry 中说明原因。

### 8. 测试

由于真实 Firecracker 测试需要 KVM，测试策略为：

- `kvm_available_returns_bool`: 只检查函数存在不 panic（不要求返回 true）
- `firecracker_config_defaults`: 验证 `FirecrackerConfig` 默认值正确
- `firecracker_api_client_request_format`: 使用 mock HTTP 服务器验证 API 调用格式
  - 参考 `src/llm/openai.rs` 中的 mock server 测试模式
  - 测试 `set_machine_config`、`set_boot_source`、`start` 的请求体
- `exec_result_deserialize`: 验证 vsock 响应反序列化
- `firecracker_provider_sandbox_mode`: `sandbox_mode()` 返回 `SandboxMode::MicroVm`

**不要**写需要真实 KVM 的集成测试（CI 环境不支持）。

## Acceptance

- `cargo test --workspace` 绿色（含新测试，可在无 KVM 环境通过）
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- `cargo fmt --all` 无差异
- `FirecrackerToolSetProvider::sandbox_mode()` 返回 `SandboxMode::MicroVm`
- `kvm_available()` 函数存在，不 panic
- Firecracker API 客户端的请求格式测试通过
- 在 macOS 上 `cargo build --all-features` 不报错（平台 feature 正确处理）

## Notes for the agent

- 读 `src/tools/docker_provider.rs` 和 `src/tools/docker_sandbox.rs` 了解 L2 实现模式
- 读 `src/tools/e2b_provider.rs` 了解 L3 实现模式
- 读 `src/tool_set_provider.rs` 了解 trait 接口
- Unix socket HTTP 请求：reqwest 在 Linux 上支持 `ClientBuilder::unix_socket()`，
  但这是隐藏的或需要 feature。如果不支持，使用 `hyperlocal` crate。
- Vsock 通信：使用 `std::os::unix::net::UnixStream` 连接到 `<vsock_path>`。
  这是一个本地 Unix domain socket，不是真正的 vsock（真正的 vsock 需要更复杂的设置）。
- **不要修改 `agent.rs`**。
- **不要在无 KVM 机器上期望 `build_registry()` 成功**——它会返回 `Err`，
  这是预期行为，由上层（如 CLI 或配置层）决定是否降级。
