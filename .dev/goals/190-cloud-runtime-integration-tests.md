# Goal 190 — CloudRuntime: 集成测试套件

**Roadmap**: Phase 21.5 — CloudRuntime 验收

**依赖**: Goal 181–189 全部完成

**Design principle check**:
- 测试覆盖 StorageBackend / SessionStore / ToolSetProvider 的真实组合路径
- 使用 `#[cfg(test)]` + feature 条件门控，不影响默认构建
- 不依赖真实外部服务（Redis/S3/E2B），使用 in-process mock / testcontainers 或 feature-gated 跳过

## Why

Goal 181–189 引入了大量 trait + 实现，目前仅有各模块的单元测试。
本 Goal 补充以下集成测试，确保：
1. `LocalStorageBackend` 端到端在 `AgentKernelBuilder` 中正常工作（无外部依赖）
2. `PolicySandbox`（L1）能在 `AgentKernelBuilder` 上按策略拒绝/放行工具调用
3. `NoopSessionStore` + `LocalStorageBackend` 的 builder 默认路径通过 round-trip 验证
4. `DockerToolSetProvider`（L2）在有 Docker 时能执行 `echo` 命令（可跳过）
5. `RedisSessionStore` / `S3StorageBackend` 正确序列化（单元 mock，无真实服务）

## Scope

### 1. `tests/storage_integration.rs` — StorageBackend 端到端

```rust
// 测试 LocalStorageBackend + AgentKernelBuilder 的完整 round-trip
// 1. 写入 transcript → 2. 重新加载 → 3. 验证内容一致
```

### 2. `tests/policy_sandbox_integration.rs` — L1 PolicySandbox + AgentKernelBuilder

```rust
// 1. 构造 PolicyConfig { deny_commands: ["rm"], allow_paths: ["/tmp"] }
// 2. 通过 AgentKernelBuilder::with_tool_set_provider(PolicyToolSetProvider)
// 3. 调用 ToolRegistry::invoke_with_audit("run_shell", { command: "rm /etc/passwd" })
// 4. 断言返回 PermissionDenied
```

### 3. `tests/kernel_builder_defaults.rs` — Builder 默认路径验证

```rust
// 不传任何 backend，验证 AgentKernelBuilder::build() 成功
// 验证 kernel.storage() 是 LocalStorageBackend 类型（通过 type_name 或 dyn downcast）
```

### 4. `tests/v060_storage_integration.rs` — 综合集成测试（含 feature-gated）

将以上 3 组测试合并为单一集成测试文件，与现有 `tests/v050_integration.rs` 风格对齐：

```rust
#[test]
fn local_storage_backend_round_trip() { ... }

#[test]
fn noop_session_store_save_load() { ... }

#[test]
fn policy_sandbox_blocks_forbidden_command() { ... }

#[test]
fn policy_sandbox_allows_permitted_command() { ... }

#[test]
fn kernel_builder_uses_defaults_when_no_backend_injected() { ... }

#[cfg(feature = "cloud-runtime")]
#[test]
fn redis_session_store_serializes_state() { ... } // 使用 MockRedis 或跳过

#[cfg(feature = "cloud-runtime")]
#[test]
fn s3_storage_backend_key_format() { ... } // 仅测试 key 生成逻辑，不真实请求
```

## 实现步骤

1. 创建 `tests/v060_storage_integration.rs`
2. 补 `src/storage/mod.rs` 中 `NoopSessionStore` 的序列化 round-trip 测试
3. 如发现 builder 默认路径有 bug，一并修复
4. `cargo test --features cloud-runtime` 全绿

## 验收标准

- `cargo test` （无任何 feature）全绿
- `cargo test --features cloud-runtime` 全绿
- `cargo clippy --all-targets --features cloud-runtime -- -D warnings` 无警告
