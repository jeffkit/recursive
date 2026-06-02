# Goal 196 — P2-3: 安全路径保护

**Roadmap**: Permission System V2 — Phase 2 规则能力增强

**依赖**: Goal 192（DecisionReason）、Goal 193（check_static mode 语义）

**Design principle check**:
- 修改 `src/permissions.rs`，添加安全路径检查
- 修改 `src/tools/fs.rs`（或相关文件工具），添加路径提取
- ❌ 不修改 `agent.rs` 主循环

## Why

当前 `bypassPermissions` 模式会跳过所有权限检查，意味着 `.git`、`.recursive`、
`.ssh` 等敏感目录也可被写入，存在安全风险。安全路径保护是一道硬性防线，
即使在 bypass 模式下也不可绕过。

## Scope

### 1. `src/permissions.rs` — 受保护目录列表

```rust
const PROTECTED_PATHS: &[&str] = &[
    ".git",
    ".recursive",
    ".ssh",
    ".gnupg",
    ".bashrc",
    ".zshrc",
    ".profile",
    ".bash_profile",
    ".bash_logout",
    ".env",
];
```

### 2. `check_static()` 中插入安全路径检查

**优先级最高**（在 bypassPermissions 检查之前）：

```rust
pub fn check_static(
    &self,
    tool_name: &str,
    is_readonly: bool,
    content: Option<&str>,
) -> Permission {
    // 0. 安全路径保护（bypass 不豁免，只读操作豁免）
    if !is_readonly {
        if let Some(path) = extract_file_path_from_content(tool_name, content) {
            for protected in PROTECTED_PATHS {
                if path_contains_protected(&path, protected) {
                    return Permission::Denied(
                        DecisionReason::SafetyCheck { path: path.clone() },
                        format!("writing to `{path}` is protected and requires explicit confirmation"),
                    );
                }
            }
        }
    }

    // 1. Plan mode 检查（Goal 193）
    // ...（现有逻辑）
}
```

### 3. 路径提取辅助函数

```rust
/// 从工具名和 content 中提取文件路径（仅适用于文件操作工具）。
fn extract_file_path_from_content(tool_name: &str, content: Option<&str>) -> Option<String> {
    match tool_name {
        "write_file" | "read_file" | "apply_patch" => content.map(|s| s.to_string()),
        _ => None,
    }
}

fn path_contains_protected(path: &str, protected: &str) -> bool {
    let path = std::path::Path::new(path);
    path.components().any(|c| {
        c.as_os_str().to_string_lossy().as_ref() == protected
    })
}
```

> 注：`content` 参数在文件工具中是文件路径字符串；
> 对 shell 工具，路径提取不在本 Goal 范围内（shell 命令过于复杂）。

### 4. `src/tools/fs.rs` / 文件工具 — 传递路径

文件工具（`WriteFile`、`ApplyPatch` 等）的 `prepare_permission_matcher`
实现返回文件路径作为 content，供 `check_static` 的路径提取使用：

```rust
impl Tool for WriteFile {
    fn prepare_permission_matcher(
        &self,
        args: &Value,
    ) -> Option<Box<dyn Fn(&str) -> bool + Send + Sync>> {
        let path = args["path"].as_str()?.to_string();
        Some(Box::new(move |pattern: &str| matches_pattern(pattern, &path)))
    }
}
```

`ToolRegistry::invoke()` 提取 path 字符串后传入 `check_static`。

### 5. 单元测试

- `protected_path_denied_in_default_mode`:
  tool="write_file", path=".git/config" → Denied(SafetyCheck)
- `protected_path_denied_in_bypass_mode`:
  mode=BypassPermissions, path=".ssh/id_rsa" → Denied(SafetyCheck)
- `protected_path_readonly_allowed`:
  is_readonly=true, path=".git/config" → 不触发保护（只读豁免）
- `non_protected_path_not_blocked`:
  path="src/main.rs" → 不触发保护
- `nested_protected_path_detected`:
  path="some/dir/.recursive/config.toml" → Denied(SafetyCheck)
- `path_contains_protected_fn`: 直接测试辅助函数边界条件

## Acceptance

- `cargo test --workspace` 绿色（含上述 6 个测试）
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- `.git`、`.ssh`、`.recursive` 路径在写操作时无论何种 mode 均被拒绝
- 只读操作不触发保护

## Notes for the agent

- `path_contains_protected` 用 `std::path::Path::components()` 而非字符串
  包含检查，避免 `legitimate_path.git_info` 被误判。
- `.env` 文件是特殊情况：项目根目录的 `.env` 通常含密钥；子目录的
  `.env.example` 无害。当前实现为简单前缀匹配，可接受误报。
- shell 命令中的路径参数提取（如 `rm .git/hooks/pre-commit`）不在本 Goal
  范围内，留给后续 Goal 或人工规则配置。
