# Goal 319 — MemFsToolProvider: 纯内存虚拟文件系统 ToolSetProvider (L0)

**Roadmap**: Phase 21 — 轻量沙箱层（存算分离 L0）

**Design principle check**:
- 新增 `src/tools/memfs_provider.rs`，实现 `ToolSetProvider` trait
- ❌ 不修改 `agent.rs` 主循环
- ✅ 正交性：MemFs 是 ToolSetProvider 的一个实现，不影响其他层
- ✅ 无外部依赖（纯 std + 已有 crate）

## Why

现有 ToolSetProvider 实现分别是：
- `LocalToolSetProvider`（L0+：真实 FS + 真实 shell）
- `PolicyToolSetProvider`（L1：策略检查 + 真实 FS）
- `DockerToolSetProvider`（L2：容器隔离）
- `E2bToolSetProvider`（L3：Firecracker microVM via e2b.dev）

缺少最轻量的 L0 层：**完全不依赖文件系统和进程的虚拟执行环境**。

用途：
1. **测试**：agent 单元测试无需真实 tmpdir，内存中构造场景
2. **诊断**：给用户展示"agent 在虚拟环境中做什么"而不真正执行
3. **批量 SaaS**：数千个轻量 agent cell，KB 级别开销，无需进程隔离
4. **预热/预计算**：在决定是否需要真实执行前先试跑一遍

## Scope（精确做这些，不多不少）

### 1. 新建 `src/tools/memfs_provider.rs`

实现包含两部分：

#### a) `MemFs` 内存文件系统

```rust
/// In-memory filesystem backed by a `HashMap<PathBuf, Vec<u8>>`.
///
/// Operations are synchronous and thread-safe via `Arc<Mutex<...>>`.
pub struct MemFs {
    files: std::collections::HashMap<std::path::PathBuf, Vec<u8>>,
    cwd: std::path::PathBuf,
}

impl MemFs {
    pub fn new() -> Self { ... }

    /// Pre-populate with files. Useful for tests.
    pub fn with_files(files: Vec<(impl Into<PathBuf>, impl Into<Vec<u8>>)>) -> Self { ... }

    pub fn read(&self, path: &Path) -> Result<Vec<u8>> { ... }
    pub fn write(&mut self, path: &Path, content: Vec<u8>) -> Result<()> { ... }
    pub fn list(&self, dir: &Path) -> Result<Vec<String>> { ... }
    pub fn exists(&self, path: &Path) -> bool { ... }
    pub fn delete(&mut self, path: &Path) -> Result<()> { ... }
    pub fn glob_match(&self, pattern: &str) -> Vec<PathBuf> { ... }
    pub fn grep_content(&self, pattern: &str, case_insensitive: bool) -> Vec<(PathBuf, Vec<usize>)> { ... }
}
```

#### b) MemFs 虚拟 Shell 模拟器

支持以下命令（其余返回 `Err(unsupported)`）：

| 命令 | 行为 |
|------|------|
| `ls [path]` | 返回 `MemFs::list()` 结果 |
| `ls -la [path]` | 同上，加权限占位符 |
| `pwd` | 返回 `MemFs::cwd` |
| `cd <path>` | 更新 `MemFs::cwd` |
| `cat <file>` | 返回 `MemFs::read()` 内容 |
| `echo <text>` | 原样返回 text |
| `mkdir -p <dir>` | 创建占位目录（写入一个 `.keep` 文件） |
| `rm <file>` | 删除文件 |
| `which <cmd>` | 返回 `/usr/bin/<cmd>` |
| `true` / `false` / `:` | 返回 exit 0 / exit 1 |

#### c) MemFs 工具实现

每个工具都包装 `Arc<Mutex<MemFs>>`：

- `MemFsReadTool` — 实现 `Tool` trait，调用 `MemFs::read`
- `MemFsWriteTool` — 调用 `MemFs::write`
- `MemFsEditTool` — 读取内容 → 应用 str_replace → 写回
- `MemFsBashTool` — 调用虚拟 shell 模拟器
- `MemFsGlobTool` — 调用 `MemFs::glob_match`
- `MemFsGrepTool` — 调用 `MemFs::grep_content`

所有工具的 `spec().name` **与标准工具完全相同**（`"Read"`、`"Write"` 等），
这样 agent 的 system prompt 不需要改变。

#### d) `MemFsToolSetProvider`

```rust
pub struct MemFsToolSetProvider {
    fs: Arc<Mutex<MemFs>>,
}

impl MemFsToolSetProvider {
    pub fn new() -> Self { ... }
    pub fn with_files(files: Vec<(impl Into<PathBuf>, impl Into<Vec<u8>>)>) -> Self { ... }
    /// Expose the underlying MemFs for inspection (testing / diagnostics).
    pub fn memfs(&self) -> Arc<Mutex<MemFs>> { ... }
}

impl ToolSetProvider for MemFsToolSetProvider {
    fn build_registry(&self) -> ToolRegistry { ... }
    fn sandbox_mode(&self) -> SandboxMode { SandboxMode::None }
}
```

> 注意：`sandbox_mode()` 返回 `None`，因为 MemFs 不是沙箱，它是一个
> 不同的**执行域**。沙箱是关于安全隔离的；MemFs 是关于轻量替换后端的。

### 2. `src/tools/mod.rs` — 导出

```rust
pub mod memfs_provider;
pub use memfs_provider::MemFsToolSetProvider;
```

### 3. `src/lib.rs` — 重新导出

```rust
pub use tools::MemFsToolSetProvider;
```

### 4. 测试

在 `src/tools/memfs_provider.rs` 的 `#[cfg(test)] mod tests` 中：

- `memfs_read_write`: 写入文件，读取验证
- `memfs_list_dir`: 列出目录内容
- `memfs_glob_pattern`: glob 模式匹配（`*.rs`）
- `memfs_grep_content`: 内容搜索
- `memfs_bash_ls`: 虚拟 shell `ls` 命令
- `memfs_bash_cat`: 虚拟 shell `cat` 命令
- `memfs_bash_echo`: 虚拟 shell `echo` 命令
- `memfs_bash_unsupported_falls_back_to_error`: 不支持的命令返回错误而非 panic
- `memfs_provider_builds_registry`: `build_registry()` 包含 `Read`、`Write`、`Bash` 工具
- `memfs_provider_sandbox_mode`: `sandbox_mode()` 返回 `SandboxMode::None`
- `memfs_edit_tool_applies_str_replace`: `MemFsEditTool` 正确应用 str_replace
- `memfs_tools_have_correct_names`: 所有工具的 `spec().name` 与标准名称一致

## Acceptance

- `cargo test --workspace` 绿色（含新测试）
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- `cargo fmt --all` 无差异
- `MemFsToolSetProvider::build_registry()` 返回的 registry 包含与标准工具同名的工具
- MemFs 基础操作（读写列删）正确
- 虚拟 shell 正确模拟 ls/cat/echo/pwd/cd
- 不支持的 shell 命令返回明确错误，不 panic

## Notes for the agent

- 读 `src/tools/e2b_provider.rs` 了解如何注册工具到 `ToolRegistry`
- 读 `src/tool_set_provider.rs` 了解 `ToolSetProvider` trait 接口
- 读 `src/tools/fs.rs` 了解标准 `Read`/`Write` 工具的 `spec()` 格式（以便 name 对齐）
- 读 `src/tools/shell.rs` 了解标准 `Bash` 工具的输出格式
- `MemFs::glob_match` 使用 `glob` crate（已在 `Cargo.toml` 中，见 `src/tools/glob.rs` 的用法）
- `MemFs::grep_content` 使用 `regex` crate（已在 `Cargo.toml` 中）
- **不要修改 `agent.rs`**
- 所有 async Tool 调用中，`Arc<Mutex<MemFs>>` 用 `tokio::sync::Mutex` 以避免 blocking
