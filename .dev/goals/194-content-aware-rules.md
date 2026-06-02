# Goal 194 — P2-1: 内容感知规则解析器

**Roadmap**: Permission System V2 — Phase 2 规则能力增强

**依赖**: Goal 191（LayeredPermissionsConfig）、Goal 193（check_static mode 语义）

**Design principle check**:
- 修改 `src/permissions.rs`，添加规则解析器
- ❌ 不修改 `agent.rs` 主循环

## Why

当前规则只能按工具名匹配（`shell`、`read_file`），无法区分
`shell(git status)` 和 `shell(rm -rf /)` 这两种截然不同的风险。
内容感知规则允许在 `config.toml` 中写 `allow = ["shell(git *)"]`，
精细控制子命令级别的权限。

## Scope

### 1. `src/permissions.rs` — 规则结构与解析

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRule {
    pub tool_name: String,
    pub content_pattern: Option<String>,  // None 表示只匹配工具名
}

impl PermissionRule {
    pub fn parse(s: &str) -> Self {
        if let Some(idx) = s.find('(') {
            if s.ends_with(')') {
                let tool_name = s[..idx].to_string();
                let content = s[idx + 1..s.len() - 1].to_string();
                return Self { tool_name, content_pattern: Some(content) };
            }
        }
        Self { tool_name: s.to_string(), content_pattern: None }
    }

    /// 匹配工具名；若有 content_pattern 则还需 content 匹配
    pub fn matches(&self, tool_name: &str, content: Option<&str>) -> bool {
        if !matches_pattern(&self.tool_name, tool_name) {
            return false;
        }
        match (&self.content_pattern, content) {
            (None, _) => true,
            (Some(pat), Some(c)) => matches_pattern(pat, c),
            (Some(_), None) => false,  // 规则要求 content，但工具未提供
        }
    }
}
```

### 2. `PermissionLayer` 存储解析后的规则

```rust
pub struct PermissionLayer {
    pub source: RuleSource,
    pub allow: Vec<PermissionRule>,
    pub deny: Vec<PermissionRule>,
    pub interactive: Vec<PermissionRule>,
}
```

从 TOML 加载时（字符串列表），在 `load_permission_layer()` 中调用
`PermissionRule::parse()` 转换。

### 3. `check_static()` 接受可选 content 参数

```rust
pub fn check_static(
    &self,
    tool_name: &str,
    is_readonly: bool,
    content: Option<&str>,  // 新增参数
) -> Permission
```

现有调用点传 `None` 保持兼容；`ToolRegistry::invoke()` 在调用
`prepare_permission_matcher`（Goal 198）后传入实际 content。

### 4. 配置文件示例（注释说明）

```toml
[permissions]
allow = [
    "read_file",
    "shell(git *)",         # git 子命令全部放行
    "shell(cargo test*)",   # cargo test 放行
]
deny  = ["shell(rm -rf*)"]
interactive = ["shell(npm publish*)"]
```

### 5. 单元测试

- `parse_rule_no_content`: `parse("read_file")` → tool="read_file", content=None
- `parse_rule_with_content`: `parse("shell(git *)")` → tool="shell", content=Some("git *")
- `parse_rule_malformed`: `parse("shell(git *")` （缺右括号）→ 整体作为 tool_name
- `rule_matches_name_only`: content_pattern=None 时，content 参数无论何值都匹配
- `rule_matches_with_content`: `shell(git *)`.matches("shell", Some("git status")) == true
- `rule_no_match_wrong_content`: `shell(git *)`.matches("shell", Some("npm install")) == false
- `rule_requires_content_when_specified`: content_pattern=Some("x"), content=None → false

## Acceptance

- `cargo test --workspace` 绿色（含上述 7 个测试）
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- `PermissionRule::parse("shell(git *)")` 正确解析
- `check_static()` 接受可选 `content` 参数，向后兼容（传 None 行为不变）

## Notes for the agent

- `matches_pattern` 已有前缀通配符逻辑；content_pattern 复用同一函数。
- 括号嵌套（如 `shell(echo (hello))`）当前不支持，遇到第一个 `(` 和最后
  一个字符 `)` 截取；复杂 shell 命令不在本 Goal 范围内。
- Goal 198 实现 `prepare_permission_matcher` 后，`run_shell` 会提供 content；
  本 Goal 只保证接口和解析器正确，content 传递集成在 Goal 198。
