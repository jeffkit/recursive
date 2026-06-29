# Goal 322 — WorkspaceStore: SQLite 持久化文件存储 + FUSE Bridge

**Roadmap**: Phase 21 — 轻量沙箱层（存算分离）

**Design principle check**:
- 新增 `src/storage/workspace_store.rs`（trait + SQLite 实现，跨平台）
- 新增 `src/storage/workspace_fuse.rs`（Linux only，FUSE filesystem）
- 修改 `src/storage/mod.rs` 注册新模块
- 修改 `Cargo.toml`：将 `rusqlite` 从 optional 升为 default-on（workspace-store feature），
  新增 `fuser` optional dep（linux-only feature）
- ❌ 不修改 `agent.rs` 主循环
- ✅ 正交性：不影响现有工具或 Provider

## Why

Goal 321 实现了 Firecracker + virtiofsd，将宿主机目录透明地挂载进 VM。
目前 `workspace_dir` 只支持普通宿主机目录，文件存在磁盘上、无多租户隔离。

`WorkspaceStore` 和 `WorkspaceFuse` 补全这一层：
- **`WorkspaceStore`** 把文件存入 SQLite（已有依赖），实现 agent 级隔离 + 跨 session 持久化
- **`WorkspaceFuse`** 把 `WorkspaceStore` 暴露为 FUSE 挂载点，virtiofsd 可读写
- **组合**：`SqliteWorkspaceStore` → `WorkspaceFuse` → 挂载目录 → `virtiofsd` → Firecracker VM

这样 VM 内的文件操作透明地写入 SQLite，VM 关机不丢数据。

## Scope

### 1. `WorkspaceStore` trait（`src/storage/workspace_store.rs`）

```rust
/// Persistent, agent-scoped file storage.
///
/// Implementations must be `Send + Sync` so they can be shared across
/// async tasks and FUSE callbacks.
pub trait WorkspaceStore: Send + Sync + 'static {
    fn read_file(&self, agent_id: &str, path: &Path) -> Result<Vec<u8>>;
    fn write_file(&self, agent_id: &str, path: &Path, data: &[u8]) -> Result<()>;
    fn list_dir(&self, agent_id: &str, dir: &Path) -> Result<Vec<PathEntry>>;
    fn remove_file(&self, agent_id: &str, path: &Path) -> Result<()>;
    fn mkdir(&self, agent_id: &str, dir: &Path) -> Result<()>;
    fn file_len(&self, agent_id: &str, path: &Path) -> Result<u64>;
}

pub struct PathEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}
```

### 2. `SqliteWorkspaceStore`（`src/storage/workspace_store.rs`）

- 表结构：`workspace_files(agent_id TEXT, path TEXT, content BLOB, is_dir BOOL, created_at INT, updated_at INT)`
- 主键：`(agent_id, path)`
- 用 `rusqlite::Connection`（已有依赖），不引入新 Crate

```rust
pub struct SqliteWorkspaceStore {
    db: Arc<Mutex<rusqlite::Connection>>,
}

impl SqliteWorkspaceStore {
    pub fn new(db_path: &Path) -> Result<Self>;
    pub fn in_memory() -> Result<Self>;  // 用于测试
}
```

- `new()` 建表（`CREATE TABLE IF NOT EXISTS`）
- 所有操作通过 `Arc<Mutex<Connection>>` 保证线程安全

### 3. `WorkspaceFuse`（`src/storage/workspace_fuse.rs`，`#[cfg(target_os = "linux")]`）

使用 `fuser` crate 实现 FUSE filesystem：

```rust
/// FUSE filesystem backed by a WorkspaceStore.
///
/// Mount this at a temp directory, then point virtiofsd at that directory.
/// Files read/written by the VM are transparently persisted to the store.
#[cfg(target_os = "linux")]
pub struct WorkspaceFuse<S: WorkspaceStore> {
    store: Arc<S>,
    agent_id: String,
}

#[cfg(target_os = "linux")]
impl<S: WorkspaceStore> WorkspaceFuse<S> {
    pub fn new(store: Arc<S>, agent_id: impl Into<String>) -> Self;

    /// Mount the FUSE filesystem at `mountpoint` in a background thread.
    ///
    /// Returns a handle that unmounts when dropped.
    pub fn mount_background(
        self,
        mountpoint: &Path,
    ) -> Result<WorkspaceFuseHandle>;
}

/// A mounted WorkspaceFuse session. Unmounts on drop.
#[cfg(target_os = "linux")]
pub struct WorkspaceFuseHandle {
    _thread: std::thread::JoinHandle<()>,
    mountpoint: PathBuf,
}

#[cfg(target_os = "linux")]
impl Drop for WorkspaceFuseHandle {
    fn drop(&mut self) {
        // unmount via fuser::unmount() or libc::umount2()
    }
}
```

需要实现的 `fuser::Filesystem` 方法（最小子集）：
- `lookup(parent, name)` — 查找目录项
- `getattr(ino)` — 返回文件属性（size, kind, permissions）
- `read(ino, offset, size, ...)` — 读文件内容
- `write(ino, offset, data, ...)` — 写文件内容
- `readdir(ino, ...)` — 列目录
- `create(parent, name, mode, ...)` — 创建文件
- `mkdir(parent, name, mode, ...)` — 创建目录
- `unlink(parent, name)` — 删除文件

Inode 映射策略：
- 维护 `HashMap<u64, PathBuf>` (ino → path) 和反向 `HashMap<PathBuf, u64>` (path → ino)
- 从 1（根目录）开始分配
- 持久化到 WorkspaceStore 的特殊键 `"__fuse_inode_map__"`

### 4. Cargo.toml 变更

```toml
[features]
# 将 rusqlite 变为 workspace-store feature 的默认依赖
workspace-store = ["dep:rusqlite"]
# Linux-only FUSE bridge
workspace-fuse = ["workspace-store", "dep:fuser"]

[dependencies]
rusqlite = { version = "0.32", features = ["bundled"], optional = true }
fuser = { version = "0.14", optional = true }
```

> `fuser` 只依赖 `libc`（已有），无其他重型依赖。
> `rusqlite` bundled 模式包含 SQLite 的 C 源码，已经在项目里用于 vector-memory feature。

### 5. 导出（`src/storage/mod.rs`）

```rust
pub mod workspace_store;
#[cfg(target_os = "linux")]
pub mod workspace_fuse;

pub use workspace_store::{PathEntry, SqliteWorkspaceStore, WorkspaceStore};
#[cfg(target_os = "linux")]
pub use workspace_fuse::{WorkspaceFuse, WorkspaceFuseHandle};
```

### 6. 测试

**`workspace_store.rs` 测试**（跨平台）：
- `sqlite_workspace_store_write_read`: 写入 + 读取文件
- `sqlite_workspace_store_list_dir`: 列目录
- `sqlite_workspace_store_agent_isolation`: 不同 agent_id 不互相可见
- `sqlite_workspace_store_in_memory`: in-memory 模式基本操作
- `sqlite_workspace_store_mkdir_and_read`: 创建目录 + 验证存在

**`workspace_fuse.rs` 测试**（`#[cfg(target_os = "linux")]`）：
- `workspace_fuse_mount_unmount`: 挂载 + 创建文件 + 读取 + 卸载（集成测试，需要 FUSE 权限，可跳过）

## Acceptance

- `cargo test --workspace` 绿色
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- `cargo fmt --all` 无差异
- `SqliteWorkspaceStore` 跨平台可用（macOS + Linux）
- `WorkspaceFuse` 只在 Linux 编译
- macOS `cargo build --all-features` 不报错

## Notes for the agent

- `fuser` 版本：`0.14`（2024 年最新稳定版）
- Inode 分配：用 `Arc<Mutex<HashMap<u64, PathBuf>>>` 跨 FUSE 回调共享
- FUSE 错误返回用 `libc::ENOENT` 等 errno 常量
- `WorkspaceFuse::mount_background` 在新线程里调用 `fuser::mount2`
- 卸载：`fuser::unmount()` 或 `std::process::Command::new("fusermount").args(["-u", path])` 作为 fallback
- 不要在 FUSE 回调里调用 async（fuser 是同步 API）
- **不要修改 `agent.rs`**
