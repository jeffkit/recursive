# Goal 195 — P2-2: Tool::check_permissions + prepare_permission_matcher

**Roadmap**: Permission System V2 — Phase 2 规则能力增强

**依赖**: Goal 192（Permission/DecisionReason）、Goal 194（内容感知规则）

**Design principle check**:
- 修改 `src/tools/mod.rs`，扩展 `Tool` trait
- 修改 `src/tools/shell.rs`，实现 shell 工具的内容匹配
- ❌ 不修改 `agent.rs` 主循环

## Why

当前工具不参与权限决策——权限检查完全在外部 `PermissionsConfig` 中。
`run_shell` 有子命令，但权限系统看不到子命令内容，无法做精细控制。
`Tool::check_permissions` 让工具自身参与决策，`prepare_permission_matcher`
向权限系统提供内容提取函数，两者共同使内容感知规则生效。

## Scope

### 1. `src/tools/mod.rs` — `Tool` trait 扩展

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    // ... 现有方法 ...

    /// 工具级权限检查。默认返回 Passthrough（外部规则决定）。
    async fn check_permissions(
        &self,
        args: &Value,
        mode: &PermissionMode,
    ) -> Permission {
        Permission::Passthrough
    }

    /// 向权限系统提供内容提取函数，用于内容感知规则匹配。
    /// run_shell 返回 Some(|pattern| command 匹配 pattern)。
    fn prepare_permission_matcher(
        &self,
        args: &Value,
    ) -> Option<Box<dyn Fn(&str) -> bool + Send + Sync>> {
        None
    }

    /// 该工具是否为只读操作（用于 plan-mode 判断）。
    fn is_readonly(&self) -> bool {
        false
    }
}
```

### 2. `src/tools/shell.rs` — `RunShell` 实现

```rust
impl Tool for RunShell {
    fn prepare_permission_matcher(
        &self,
        args: &Value,
    ) -> Option<Box<dyn Fn(&str) -> bool + Send + Sync>> {
        let command = args["command"].as_str()?.to_string();
        Some(Box::new(move |pattern: &str| {
            matches_pattern(pattern, &command)
        }))
    }
    // is_readonly 保持默认 false
}
```

### 3. 只读工具标记

以下工具实现 `is_readonly() -> bool { true }`：
- `ReadFile`
- `GlobTool`（若存在）
- `ListFiles`（若存在）
- `SearchCode`（若存在）

### 4. `ToolRegistry::invoke()` 集成

在现有静态规则检查后，插入：

```rust
// 获取 content matcher，用于内容感知规则
let content = tool.prepare_permission_matcher(&arguments)
    .and_then(|matcher| {
        // 将 matcher 传给 check_static 供内容规则匹配
        // check_static 接受 Option<&dyn Fn(&str)->bool>
        Some(matcher)
    });

// 工具自身权限检查
let tool_perm = tool.check_permissions(&arguments, &perms.mode).await;
match tool_perm {
    Permission::Denied(reason, msg) => {
        return Err(Error::PermissionDenied { reason, message: msg });
    }
    Permission::Allowed(_) => { /* 工具主动放行，跳过外部规则 */ }
    Permission::Passthrough => {
        // 继续走外部规则，传入 content 信息
        let is_readonly = tool.is_readonly();
        let static_perm = perms.check_static(tool_name, is_readonly, content.as_deref());
        // 处理 static_perm ...
    }
}
```

> 注：`check_static` 接受 `content` 的方式根据 Goal 194 的具体接口调整；
> 若 content 是 matcher 函数而非字符串，调整 `check_static` 签名为
> `Option<&dyn Fn(&str) -> bool>`。

### 5. 单元测试

- `shell_prepare_matcher_returns_fn`: 传入 `{"command": "git status"}` →
  matcher 对 `"git *"` 返回 true，对 `"npm *"` 返回 false
- `readonly_tools_report_readonly`: ReadFile, GlobTool 等 `is_readonly()` == true
- `shell_is_not_readonly`: RunShell `is_readonly()` == false
- `check_permissions_default_passthrough`: 未覆盖的工具返回 `Passthrough`

## Acceptance

- `cargo test --workspace` 绿色
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- `RunShell::prepare_permission_matcher` 提供有效匹配函数
- `is_readonly()` 对读类工具返回 true，对写/shell 工具返回 false
- `ToolRegistry::invoke()` 在静态规则检查前调用 `check_permissions`

## Notes for the agent

- `Tool` trait 新增三个默认方法，所有现有工具实现无需改动（默认行为向后兼容）。
- `matches_pattern` 在 `permissions.rs` 中；`shell.rs` 需要访问它。
  可以将其提取到 `src/permissions/pattern.rs` 公共模块，或直接 re-export。
- `prepare_permission_matcher` 返回的 `Box<dyn Fn>` 是 Send+Sync，
  确保在 async invoke 中跨 await 点使用时不出现 lifetime 问题。
