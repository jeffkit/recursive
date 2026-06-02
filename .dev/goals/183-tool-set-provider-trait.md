# Goal 183 — ToolSetProvider Trait + 工具别名支持（tool aliases）

**Roadmap**: Phase 20.3 — 存算分离基础架构（3/4）

**依赖**: Goal 181 合并后开始（与 Goal 182 可并行）

**Design principle check**:
- 在 `src/tools/mod.rs` 中扩展 `ToolRegistry`，新增 alias 查找支持
- 新增 `src/tool_set_provider.rs` 定义 `ToolSetProvider` trait
- ❌ 不修改 `agent.rs` 主循环

## Why

为了支持"工具替换"——在不修改 system prompt 的情况下，将本地 `run_shell` 
换成沙盒化的实现——需要两件事：

1. **ToolRegistry 支持 alias**：注册工具时可以指定别名，查找时主名和别名都能命中
2. **ToolSetProvider trait**：让 `AgentKernel` 接受可插拔的工具集来源，
   而不是硬编码 `build_tool_registry()`

这是实现本地/云端工具切换的关键基础设施。

## Scope

### 1. `src/tools/mod.rs` — 给 `ToolSpec` 和 `ToolRegistry` 增加 alias 支持

在 `ToolSpec` 中新增可选字段：

```rust
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    /// Optional aliases (e.g. old names kept for backward compatibility,
    /// or canonical names that route to alternative implementations).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}
```

在 `ToolRegistry` 的查找方法中，支持通过 alias 查找：

```rust
impl ToolRegistry {
    /// Find a registered tool by its primary name or any alias.
    pub fn find_by_name(&self, name: &str) -> Option<&RegisteredTool> {
        self.tools.iter().find(|t| {
            t.spec.name == name || t.spec.aliases.iter().any(|a| a == name)
        })
    }
}
```

确保现有的 `call(name, ...)` 方法也走这个查找路径。

### 2. 新建 `src/tool_set_provider.rs`

```rust
//! Tool set provider trait for pluggable tool implementations.

use crate::tools::ToolRegistry;

/// Controls how aggressively the sandbox restricts tool side-effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxMode {
    /// No sandbox — direct local execution (current default).
    #[default]
    None,
    /// Policy-based: FS path and network restrictions enforced at the Rust layer.
    Policy,
    /// Container-based: tools execute inside Docker/gVisor.
    Container,
    /// MicroVM-based: tools execute inside Firecracker/E2B.
    MicroVm,
}

/// Provides the ToolRegistry for a given runtime mode.
///
/// The local implementation returns the standard registry.
/// Cloud implementations may wrap tools with sandbox adapters.
pub trait ToolSetProvider: Send + Sync + 'static {
    fn build_registry(&self) -> ToolRegistry;
    fn sandbox_mode(&self) -> SandboxMode;
}

/// Returns the standard local tool registry with no sandboxing.
pub struct LocalToolSetProvider;

impl ToolSetProvider for LocalToolSetProvider {
    fn build_registry(&self) -> ToolRegistry {
        crate::tools::build_default_registry()
    }
    fn sandbox_mode(&self) -> SandboxMode {
        SandboxMode::None
    }
}
```

（如果 `build_default_registry()` 函数不存在，将现有的注册逻辑提取出来。）

### 3. `src/lib.rs` — 导出

```rust
pub mod tool_set_provider;
pub use tool_set_provider::{LocalToolSetProvider, SandboxMode, ToolSetProvider};
```

### 4. 测试

在 `src/tools/mod.rs` 的 `#[cfg(test)] mod tests` 中：

- `find_by_name_uses_primary_name`: 通过主名能找到工具
- `find_by_name_uses_alias`: 通过别名能找到工具
- `find_by_name_returns_none_for_unknown`: 不存在的名字返回 `None`
- `aliases_not_serialized_when_empty`: `ToolSpec` 无 alias 时 JSON 序列化不包含 `aliases` 字段

在 `src/tool_set_provider.rs` 的 `#[cfg(test)] mod tests` 中：

- `local_provider_sandbox_mode_is_none`: `LocalToolSetProvider` 的 `sandbox_mode()` 为 `SandboxMode::None`
- `local_provider_registry_non_empty`: `build_registry()` 返回的 registry 至少包含 `run_shell`

## Acceptance

- `cargo test --workspace` 绿色
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- `ToolSpec` 有 `aliases` 字段，默认为空，空时不序列化
- `ToolRegistry::find_by_name` 支持 alias 查找
- `LocalToolSetProvider` 实现 `ToolSetProvider` trait
- 所有现有测试通过（alias 是纯增量，不改变现有行为）

## Notes for the agent

- 读 `src/tools/mod.rs` 了解现有 `ToolRegistry` 和 `ToolSpec` 结构。
- `ToolSpec.aliases` 默认空，现有代码不用改——`#[serde(default)]` 保证反序列化兼容。
- `build_default_registry()` 函数可能需要从 `main.rs` 或 `agent.rs` 提取，
  注意不要破坏现有调用点——只是提取为函数，不改逻辑。
- **不要修改 `agent.rs` 主循环**；ToolSetProvider 在 AgentKernel 的集成留给 Goal 184。
