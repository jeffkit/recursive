# Goal 321 — Firecracker + virtiofsd: 持久化共享 Workspace

**Roadmap**: Phase 21 — 轻量沙箱层（存算分离 L3-local）

**Design principle check**:
- 修改 `src/tools/firecracker_provider.rs`，为 `FirecrackerConfig` 增加 workspace 配置
- ❌ 不修改 `agent.rs` 主循环
- ✅ 正交性：virtiofsd 集成仅影响 FirecrackerToolSetProvider
- ✅ 平台守卫：`#[cfg(target_os = "linux")]`，macOS 构建无错误
- ✅ 不引入新 Cargo crate（virtiofsd 是外部二进制，非库依赖）

## Why

Goal 320 实现的 FirecrackerToolSetProvider 中，VM 的文件系统是临时的——VM 关闭后文件丢失。
这限制了 Firecracker 在需要跨 session 持久化状态的场景中的使用（如长运行 agent）。

**virtiofsd** 是 Linux 项目维护的官方 virtio-fs 守护进程（Rust 实现），可将宿主机目录
透明地挂载进 VM。文件读写由 VM 内核 virtiofs 驱动完成，自动持久化到宿主机目录。

好处：
- **无需 sync**：VM 内文件操作直接作用于宿主机目录，实时生效
- **无需快照**：宿主机目录天然持久，VM 重启后文件依然存在
- **无需 DbFs**：宿主机目录本身就是持久存储，可以是 tmpfs/ext4/NFS/任意 POSIX FS
- **利用现有工具**：virtiofsd 是成熟的标准工具，不重复造轮子

## 前置条件（运行时，非构建时）

- Linux 宿主机，KVM 可用
- `virtiofsd` 二进制在 PATH 中（或通过 `RECURSIVE_VIRTIOFSD_BIN` 指定）
  - Fedora/RHEL: `dnf install virtiofsd`
  - Ubuntu 22.04+: `apt install virtiofsd`
  - 手动编译: `cargo install virtiofsd`
- Firecracker >= 1.0（支持 virtio_fs 设备）
- Guest 内核带有 virtiofs 支持（`CONFIG_VIRTIO_FS=y`，Linux 5.4+ 已内置）
- Guest initrd/init 中有 `mount -t virtiofs host /workspace` 初始化脚本

## Scope（精确做这些，不多不少）

### 1. `FirecrackerConfig` 扩展

在现有 `FirecrackerConfig` 中增加可选 workspace 配置：

```rust
pub struct FirecrackerConfig {
    // ... 现有字段 ...

    /// Host directory to share with the VM via virtiofs.
    /// If Some, virtiofsd will be spawned and the VM will mount this directory
    /// at `/workspace` via virtiofs.
    /// If None, no virtiofs device is configured (VM has ephemeral rootfs only).
    pub workspace_dir: Option<PathBuf>,

    /// Path to the virtiofsd binary (default: looks up PATH).
    pub virtiofsd_bin: PathBuf,

    /// Mount tag used inside the guest for virtiofs mount (default: "host").
    pub virtiofs_tag: String,

    /// Unix socket path for virtiofsd <-> Firecracker communication.
    /// Auto-generated if None.
    pub virtiofsd_socket: Option<PathBuf>,
}
```

`Default` 实现：
- `workspace_dir = None`
- `virtiofsd_bin = PathBuf::from("virtiofsd")`
- `virtiofs_tag = "host".to_string()`
- `virtiofsd_socket = None`（运行时自动生成 `/tmp/recursive-virtiofsd-{uuid}.sock`）

### 2. virtiofsd 生命周期管理

在 `FirecrackerVm` 中增加 virtiofsd 进程管理：

```rust
pub struct FirecrackerVm {
    _fc_process: tokio::process::Child,
    /// virtiofsd child process (Some only when workspace_dir is configured).
    _virtiofsd_process: Option<tokio::process::Child>,
    vsock_uds: PathBuf,
    shell_timeout_secs: u64,
}
```

spawn 逻辑（`#[cfg(target_os = "linux")]` 内）：

```rust
// 1. 如果 workspace_dir 配置了，先启动 virtiofsd
if let Some(ws_dir) = &config.workspace_dir {
    let socket = config.virtiofsd_socket.unwrap_or_else(|| {
        PathBuf::from(format!("/tmp/recursive-virtiofsd-{}.sock", uuid))
    });
    let virtiofsd = tokio::process::Command::new(&config.virtiofsd_bin)
        .args([
            "--socket-path", socket.to_str().unwrap(),
            "--shared-dir", ws_dir.to_str().unwrap(),
            "--sandbox", "none",    // 在已隔离的 VM 环境中无需二次 sandbox
            "--cache", "never",     // 禁用缓存确保一致性
        ])
        .spawn()?;
    virtiofsd_process = Some(virtiofsd);
    virtiofsd_socket_path = Some(socket);
}

// 2. 启动 Firecracker，PUT /vsock + PUT /virtio_fs（如果有 virtiofsd）
// ...

// 3. 等待 virtiofsd socket 就绪（最多 2 秒）
// ...
```

### 3. Firecracker virtio_fs 配置

通过 Firecracker REST API 配置 virtio_fs 设备：

```
PUT /virtio-fs
{
  "socket_path": "/tmp/recursive-virtiofsd-<uuid>.sock",
  "tag": "host"
}
```

> Firecracker 1.0+ 支持 `/virtio-fs` endpoint。
> 参考：https://github.com/firecracker-microvm/firecracker/blob/main/docs/virtio-fs.md

在 `FirecrackerApiClient` 中新增：

```rust
/// PUT /virtio-fs
pub async fn set_virtio_fs(
    &self,
    socket_path: &Path,
    tag: &str,
) -> Result<()>;
```

### 4. 环境变量支持

`FirecrackerConfig::from_env()` 扩展：
- `RECURSIVE_FC_WORKSPACE_DIR` → `workspace_dir`
- `RECURSIVE_VIRTIOFSD_BIN` → `virtiofsd_bin`

### 5. 测试

- `firecracker_config_workspace_defaults`: 验证新字段的默认值
- `firecracker_config_workspace_from_env`: 验证 `RECURSIVE_FC_WORKSPACE_DIR` 环境变量读取
- `firecracker_virtiofs_api_request_format`: 验证 `PUT /virtio-fs` 请求格式正确

**不要**写需要真实 virtiofsd 的集成测试。

## Acceptance

- `cargo test --workspace` 绿色
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- `cargo fmt --all` 无差异
- `FirecrackerConfig::default().workspace_dir` 为 `None`
- `FirecrackerConfig::from_env()` 正确读取 `RECURSIVE_FC_WORKSPACE_DIR`
- virtiofsd API 请求格式测试通过
- macOS 上 `cargo build --all-features` 不报错

## Notes for the agent

- virtiofsd 启动参数参考：`virtiofsd --socket-path <sock> --shared-dir <dir> --sandbox none`
- `--sandbox none` 表示不使用 virtiofsd 内置的 seccomp 沙箱（VM 已提供隔离）
- `--cache never` 确保 VM 对文件的修改立即反映到宿主机
- 等待 virtiofsd socket 就绪的方式：轮询 socket 文件是否存在（最多 2 秒，50ms 间隔）
- Firecracker `/virtio-fs` endpoint 只在 Firecracker >= 1.0 支持
- guest 侧的 `mount -t virtiofs host /workspace` 由 guest initrd 负责，本 goal 不涉及
- **不要修改 `agent.rs`**。
