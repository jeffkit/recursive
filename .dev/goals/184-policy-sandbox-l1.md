# Goal 184 — L1 Policy 沙盒：FS / 网络策略封装层

**Roadmap**: Phase 20.4 — 存算分离基础架构（4/4）

**依赖**: Goal 183 合并后开始

**Design principle check**:
- 新增 `src/tools/policy_sandbox.rs`，通过包装器模式添加策略检查
- ❌ 不修改 `agent.rs` 主循环
- 正交性：PolicySandbox 包装已有工具，不改变工具内部逻辑

## Why

在引入重量级容器/VM 沙盒（L2/L3）之前，先用纯 Rust 的策略层（L1）提升安全性：
- 阻止 Agent 读写 workspace 外部的路径（比现有路径限制更显式）
- 阻止 `run_shell` 执行特定高危命令（如 `rm -rf /`、`curl evil.com`）
- 为未来 L2/L3 沙盒提供统一接口

这个 Goal 是"策略沙盒包装器"的核心实现，对应 fake-cc 的 `@anthropic-ai/sandbox-runtime` 的 L1 功能。

## Scope

### 1. 新建 `src/tools/policy_sandbox.rs`

```rust
//! L1 policy-based sandbox wrapper for tools.
//!
//! Wraps any tool with configurable FS and shell policy checks.
//! No OS-level isolation; violations are blocked at the Rust layer.

use crate::error::{Error, Result};
use serde::Deserialize;

/// Policy rule for filesystem access.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct FsPolicy {
    /// Paths the agent is allowed to read (empty = allow all).
    #[serde(default)]
    pub read_allow: Vec<String>,
    /// Paths the agent is allowed to write (empty = allow all).
    #[serde(default)]
    pub write_allow: Vec<String>,
    /// Paths explicitly denied for read or write (takes priority over allow).
    #[serde(default)]
    pub deny: Vec<String>,
}

/// Policy rule for shell command execution.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ShellPolicy {
    /// Shell command prefixes/patterns that are denied.
    #[serde(default)]
    pub deny_patterns: Vec<String>,
}

/// Combined policy configuration.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct PolicyConfig {
    #[serde(default)]
    pub fs: FsPolicy,
    #[serde(default)]
    pub shell: ShellPolicy,
}

impl PolicyConfig {
    /// Default restrictive policy: deny the most dangerous shell commands.
    pub fn default_restrictive() -> Self {
        Self {
            fs: FsPolicy::default(),
            shell: ShellPolicy {
                deny_patterns: vec![
                    "rm -rf /".into(),
                    "rm -rf ~".into(),
                    "mkfs".into(),
                    "> /dev/".into(),
                ],
            },
        }
    }

    /// Check whether a shell command is allowed.
    pub fn check_shell(&self, command: &str) -> Result<()> {
        for pattern in &self.shell.deny_patterns {
            if command.contains(pattern.as_str()) {
                return Err(Error::PermissionDenied {
                    message: format!("shell command blocked by policy: matches pattern `{pattern}`"),
                });
            }
        }
        Ok(())
    }

    /// Check whether a filesystem path access is allowed.
    pub fn check_fs_path(&self, path: &str, write: bool) -> Result<()> {
        for denied in &self.fs.deny {
            if path.starts_with(denied.as_str()) {
                return Err(Error::PermissionDenied {
                    message: format!("path `{path}` blocked by fs deny policy"),
                });
            }
        }
        let allow_list = if write { &self.fs.write_allow } else { &self.fs.read_allow };
        if !allow_list.is_empty() {
            let allowed = allow_list.iter().any(|prefix| path.starts_with(prefix.as_str()));
            if !allowed {
                return Err(Error::PermissionDenied {
                    message: format!("path `{path}` not in fs allow list"),
                });
            }
        }
        Ok(())
    }
}
```

### 2. 在 `src/error.rs` 中添加 `PermissionDenied` 变体（如果还没有）

```rust
/// A tool invocation was blocked by the policy sandbox.
PermissionDenied { message: String },
```

### 3. `src/tools/mod.rs` — 导出新模块

```rust
pub mod policy_sandbox;
pub use policy_sandbox::{FsPolicy, PolicyConfig, ShellPolicy};
```

### 4. 在 `src/tool_set_provider.rs` 中添加 `PolicyToolSetProvider`

```rust
use crate::tools::policy_sandbox::PolicyConfig;

/// ToolSetProvider that wraps the default registry with L1 policy checks.
///
/// Used when SandboxMode::Policy is configured.
pub struct PolicyToolSetProvider {
    pub policy: PolicyConfig,
}

impl ToolSetProvider for PolicyToolSetProvider {
    fn build_registry(&self) -> ToolRegistry {
        // Future work: wrap each sensitive tool with PolicyWrapper.
        // For now, store the policy in the registry for tools to query.
        let mut registry = crate::tools::build_default_registry();
        registry.set_policy(self.policy.clone());
        registry
    }
    fn sandbox_mode(&self) -> SandboxMode {
        SandboxMode::Policy
    }
}
```

（如果 `ToolRegistry::set_policy` 不存在，在 `ToolRegistry` 中添加一个
`pub policy: Option<PolicyConfig>` 字段即可。）

### 5. 测试

在 `src/tools/policy_sandbox.rs` 的 `#[cfg(test)] mod tests` 中：

- `check_shell_allows_safe_command`: `ls -la` 不被阻止
- `check_shell_blocks_rm_rf`: `rm -rf /` 返回 `Err(PermissionDenied)`
- `check_shell_blocks_pattern_substring`: 命令中含 deny_pattern 时也阻止
- `check_fs_deny_blocks_path`: 路径在 deny 列表时被阻止（读和写均适用）
- `check_fs_allow_list_blocks_outside`: allow_list 非空时，不在列表内的路径被阻止
- `check_fs_empty_allow_list_allows_all`: allow_list 为空时所有路径通过

## Acceptance

- `cargo test --workspace` 绿色（含新测试）
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- `PolicyConfig::check_shell` 和 `check_fs_path` 按预期工作
- `PermissionDenied` error 变体存在
- `PolicyToolSetProvider` 实现 `ToolSetProvider`
- 现有所有测试通过（policy 是增量，本地模式默认不启用）

## Notes for the agent

- 读 `src/error.rs` 检查是否已有类似的 permission 变体，避免重复。
- 读 `src/permissions.rs` 了解现有 `PermissionsConfig`，本 Goal 的 `PolicyConfig`
  是对它的**补充**，不是替换。两者可以共存：PermissionsConfig 是静态工具名黑白名单，
  PolicyConfig 是命令内容 / 路径内容层面的策略。
- `PolicyToolSetProvider` 的 `build_registry` 目前只是存储 policy 供工具使用，
  真正的工具包装（run_shell 调用前检查 PolicyConfig）是未来增量工作，
  **本 Goal 只做框架，不修改 `run_shell` 等工具的实现**。
- **不要修改 `src/agent.rs`**。
