# Proposal: Authentication & Tool Permission System

> **Status**: Draft — pending review
> **Created**: 2026-05-28
> **Baseline**: v0.6.0
> **Scope**: Phase 17.2 (Authentication) + Phase 17.3 (Tool Permission)

---

## Design Principles

来自 ROADMAP-v4 和 AGENTS.md：

- **#9: Auth 是中间件 — 核心逻辑不感知 auth。** Agent 不知道调用者是谁；身份仅在 HTTP 层解析。
- **#2: 正交性。** 权限检查不侵入 agent 循环或单个工具实现。
- **#10: `cargo test` 必须全绿。** 默认配置下（无 auth、无权限限制），所有现有测试行为不变。

---

## 双层权限模型

```
ToolRegistry::invoke()
    │
    ├─ 第1层：静态策略（PermissionsConfig）    硬门禁，无需人参与
    │   来源：config.toml / 环境变量 / CLI 标志
    │   判定：进程内即时返回 Allow / Deny
    │   场景：CI 禁 run_shell、HTTP API 限只读工具
    │
    └─ 第2层：交互式同意（PermissionHook）     软门禁，需要人实时参与
        来源：终端 y/n 提示 / 未来 macOS 对话框 / web UI 回调
        判定：阻塞等待用户输入
        场景：本地 `recursive run`，"agent 要执行 rm -rf，确认？"

两层是串联关系：静态拒绝的调用不打扰用户（直接 Deny），静态放行的调用进入交互式检查。
```

---

## Part 1: Authentication（Phase 17.2）

### 当前状态

- HTTP API 无认证：`/run`、`/sessions`、`/tools` 对所有请求开放
- CLI 模式（`recursive run`）天然不需要 auth
- Rate limiter 使用 `X-API-Key` header 仅作分桶键，不验证

### 方案：预共享密钥（Pre-Shared Key），不引入 JWT

| 维度 | JWT | 预共享密钥 |
|------|-----|-----------|
| 新依赖 | `jsonwebtoken` crate | 0 |
| 复杂度 | 签发/验证/过期/刷新 | 单次字符串比较 |
| 运维 | 密钥管理 + token 轮换 | 一个环境变量 |
| Recursive 实际场景 | ❌ 无多用户需求 | ✅ HTTP API 是开发者自用 |

### 配置入口

三个优先级层次：

```
1. 环境变量: RECURSIVE_SERVER_KEY
2. 配置文件: ~/.recursive/config.toml → [server].api_key
3. 默认值:  无 auth（向后兼容）
```

`config.toml` 新增 `[server]` 段：

```toml
[server]
api_key = "my-secret-token"

[server.permissions]
allow = ["read_file", "list_dir", "search_files", "grep_files"]
deny = ["run_shell", "write_file", "apply_patch"]
interactive = ["run_shell", "write_file", "apply_patch"]
```

### Auth 中间件

在 `src/http.rs` 新增，插入 rate limiter 之后、handler 之前。`/health` 永远豁免。

```rust
fn auth_middleware(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let expected = match &state.server_config.api_key {
        Some(key) => key,
        None => return Ok(next.run(req).await),
    };
    let provided = req.headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok());
    match provided {
        Some(key) if constant_time_eq(key.as_bytes(), expected.as_bytes()) => {
            Ok(next.run(req).await)
        }
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}
```

`constant_time_eq` 纯标准库实现，零依赖。对 CLI 模式无影响。

---

## Part 2: 静态工具权限（Phase 17.3 — 硬门禁）

### 当前状态

- `PermissionHook` 类型在 `agent.rs:51` 已定义，`AgentBuilder::permission_hook()` 方法存在
- 但 **没有任何调用者实际设置它** — 管线铺好了，水龙头没接
- agent 循环在 `agent.rs:407` 已有完整调用点

### 目标

不依赖 `PermissionHook` 闭包，直接在 `ToolRegistry::invoke()` 中做静态规则检查。无论 Agent、AgentRuntime、HTTP handler 还是 sub-agent，权限统一生效。

### 策略定义

```rust
// src/permissions.rs（新文件）

#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct PermissionsConfig {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub interactive: Vec<String>,
}

impl PermissionsConfig {
    pub fn check_static(&self, tool_name: &str) -> Permission {
        if self.deny.iter().any(|d| matches_pattern(d, tool_name)) {
            return Permission::Denied(format!("tool '{}' is denied by config", tool_name));
        }
        if !self.allow.is_empty()
            && !self.allow.iter().any(|a| matches_pattern(a, tool_name))
        {
            return Permission::Denied(format!(
                "tool '{}' is not in allow list", tool_name
            ));
        }
        Permission::Allowed
    }

    pub fn is_interactive(&self, tool_name: &str) -> bool {
        if self.deny.iter().any(|d| matches_pattern(d, tool_name)) {
            return false;
        }
        self.interactive.iter().any(|i| matches_pattern(i, tool_name))
    }
}

/// 通配符: "run_*" 匹配 "run_shell", "run_background"
fn matches_pattern(pattern: &str, name: &str) -> bool {
    if pattern == name { return true; }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return name.starts_with(prefix);
    }
    false
}
```

### 策略来源（优先级）

1. 环境变量: `RECURSIVE_TOOL_ALLOW` / `RECURSIVE_TOOL_DENY` / `RECURSIVE_TOOL_INTERACTIVE`
2. 配置文件: `[server.permissions]`
3. CLI 标志: `--tool-allow` / `--tool-deny` / `--tool-interactive`
4. 默认: 全部允许，无交互

### 实施点：ToolRegistry::invoke()

```rust
impl ToolRegistry {
    pub fn with_permissions(mut self, permissions: PermissionsConfig) -> Self {
        self.permissions = Some(permissions);
        self
    }

    pub async fn invoke(&self, name: &str, arguments: Value) -> Result<String> {
        if let Some(ref perms) = self.permissions {
            match perms.check_static(name) {
                Permission::Denied(reason) => {
                    return Err(Error::Tool { name: name.into(), message: reason });
                }
                Permission::Allowed => {}
            }
        }
        // ... 现有 invoke 逻辑 ...
    }
}
```

---

## Part 3: 交互式用户同意（Phase 17.3 — 软门禁）

### 设计思路

交互式同意在静态策略放行后、工具执行前插入确认环节。利用已有 `PermissionHook` 机制注入一个组合了静态策略 + 终端交互的闭包。

### 组装逻辑（main.rs）

```rust
fn build_interactive_hook(permissions: &PermissionsConfig) -> Option<PermissionHook> {
    let perms = permissions.clone();
    if perms.interactive.is_empty() {
        return None;
    }
    Some(Arc::new(move |name: &str, _args: &Value| {
        match perms.check_static(name) {
            Permission::Denied(reason) => return PermissionDecision::Deny(reason),
            Permission::Allowed => {}
        }
        if !perms.is_interactive(name) {
            return PermissionDecision::Allow;
        }
        // 格式化参数摘要，提示用户
        let summary = summarize_args(_args);
        eprint!("\n⚠ Agent wants to run `{name}`\n   args: {summary}\nAllow? [y/N] ");
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        match std::io::stdin().read_line(&mut input) {
            Ok(_) if input.trim().to_lowercase() == "y" => {
                eprintln!("  ✓ allowed");
                PermissionDecision::Allow
            }
            _ => {
                eprintln!("  ✗ denied");
                PermissionDecision::Deny("user declined".into())
            }
        }
    }))
}
```

### 串联方式

在 `run_once()` 中：

```rust
let permissions = load_permissions(&config, &cli);
let registry = build_tools(&config)
    .await
    .with_permissions(permissions.clone());   // 静态检查在 ToolRegistry

let mut runtime = build_runtime(...).await?;
if let Some(hook) = build_interactive_hook(&permissions) {
    runtime = runtime.permission_hook(hook);   // 交互式通过 PermissionHook
}
```

### 对非交互模式的影响

- `--json` 模式：不注入交互式 hook
- HTTP API：仅使用静态策略，不注入 PermissionHook（HTTP 无终端可交互）
- 非 TTY 环境（CI、管道）：检测 `!atty::is(atty::Stream::Stdin)` 时回退，工具被 deny 而非阻塞

### 与现有机制的关系

| 层次 | 实施点 | 覆盖范围 | 当前状态 |
|------|--------|---------|---------|
| 静态 deny | `ToolRegistry::invoke()` | 所有路径（CLI + HTTP + sub-agent） | **新加** |
| 交互确认 | `PermissionHook` 闭包 | 仅 CLI 交互模式 | **新加**（利用已有接口） |
| 生命周期 hook | `HookRegistry` / `ToolTimingHook` | 所有路径 | 已有，不改动 |

---

## Part 4: 实施路线

### 拆分为 5 个独立 Goal

| Goal | 范围 | 文件 | 依赖 | 复杂度 |
|------|------|------|------|--------|
| **G1** — ServerConfig + 配置加载 | `ServerConfig` 结构体、`[server]` 段解析、env var | `config_file.rs`, `config.rs` | 无 | S |
| **G2** — Auth 中间件 | `auth_middleware`、`/health` 豁免、`constant_time_eq` | `http.rs` | G1 | S |
| **G3** — PermissionsConfig 基础设施 | 类型、通配符、`ToolRegistry` 集成、invoke() 检查 | `permissions.rs`（新）, `tools/mod.rs` | G1 | M |
| **G4** — 交互式同意 | `build_interactive_hook`、TTY 检测、`--json` 豁免 | `permissions.rs`, `main.rs` | G3 | M |
| **G5** — 完整串联 + CLI + 测试 | `--tool-allow/deny/interactive` 标志、集成测试 | `main.rs`, `http.rs`, `tests/` | G2+G4 | M |

### 分批建议

- **Batch A**（可并行）: G1 + G2 + G3 — 文件互不冲突
- **Batch B**: G4 → G5（串行依赖）

---

## Part 5: 不做的

- JWT / OAuth2 / OIDC — 预共享密钥满足单租户场景
- RBAC 角色模型 — 后续通过 `PermissionsConfig.roles: HashMap` 扩展
- 路径级权限 — `allowed_paths` 预留字段，MVP 不实现
- macOS 系统对话框 — 先用 stdin/stderr，平台对话框是 UI 优化
- 交互式同意的"记住 N 分钟" — 单次确认，不引入会话级记忆
- 审计日志 — Phase 15 tracing 已覆盖
