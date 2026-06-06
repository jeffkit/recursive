# Recursive 架构 Review 总报告

**Date**: 2026-06-06
**Reviewer**: Architecture Critic + Security Auditor (AI, 6-agent parallel review)
**Scope**: 全项目 (`src/` 全部 ~110 个文件)
**Report Files**: `01-core-engine.md` ~ `06-interfaces.md`

---

## Executive Summary

Recursive 是一个设计理念清晰的 Rust AI Agent 框架：精简的 Agent loop、正交的工具扩展点、多后端存储抽象。整体工程质量中上，Rust 惯用法基本到位，关键路径有测试覆盖。

然而本次 review 发现了 **5 个 P0 级安全/数据损坏问题**，必须在下一个版本前修复；另有 **8 个 P1 级架构缺陷** 影响生产可靠性。

---

## P0 — 必须立即修复（数据损坏 / 安全漏洞）

| ID | 模块 | 问题 | 文件:行 |
|----|------|------|---------|
| **C-1** | 核心引擎 | 并行 tool call 的 audit 结果匹配错位 → 静默数据损坏 | `run_core.rs:426-456` |
| **SEC-001** | 安全 | SSRF：`web_fetch` 不过滤内网地址 → 云凭证泄露 | `tools/web_fetch.rs:36-44` |
| **SEC-003** | 安全 | HTTP 默认无认证（空 keys → 全放行） → 未授权 RCE | `http/auth.rs:62-65` |
| **C1-storage** | 存储 | `session_lock.rs` TOCTOU race → 两进程同时持锁，数据乱序 | `session_lock.rs:192-237` |
| **B1-interface** | HTTP | 请求体无大小限制 → OOM 崩溃（无认证即可触发） | `http/mod.rs:378-420` |

### P0 修复指引

**C-1**（5 分钟）：`execute_tool_calls` 中将 `batch_results` 改为 `HashMap<tool_call_id, row>` 做 O(1) 查找，消除并发批次的 ID 错位。

**SEC-001**（30 分钟）：`validate_url` 增加 DNS 解析后 IP 段检查，拒绝 RFC 1918 / link-local / loopback / `169.254.0.0/16`。

**SEC-003**（10 分钟）：`is_valid()` 中 `keys.is_empty()` 时返回 `false`，并在启动日志打印 `WARN: HTTP auth is DISABLED`。

**C1-storage**（20 分钟）：`session_lock.rs` 用 `OpenOptions::new().create_new(true)` 替换 `is_file()` + `write` 的 check-then-write 模式。

**B1-interface**（5 分钟）：路由器加 `.layer(DefaultBodyLimit::max(1 * 1024 * 1024))`。

---

## P1 — 本迭代应修复（生产可靠性 / 架构缺陷）

| ID | 模块 | 问题 | 文件 |
|----|------|------|------|
| **SEC-002** | 安全 | `sh -c` + 无效 deny 列表 → 主机 RCE | `tools/shell.rs:89` |
| **SEC-008** | 安全 | Policy sandbox 未接入工具调用链，`default_restrictive` 形同虚设 | `tools/policy_sandbox.rs`, `tools/mod.rs` |
| **M-4 (multi)** | LLM | `multi.rs` 声称并行实际串行，子 Agent 挂死无超时 | `multi.rs:451-479` |
| **M-1 (LLM)** | LLM | OpenAI SSE 流式 UTF-8 跨 chunk 截断 → 静默数据损坏 | `llm/openai.rs:609-628` |
| **M-4 (tools)** | 工具 | `build_sub_registry` 逻辑错误：Explore worker 实际拥有 Write/Bash 全量工具 | `tools/mod.rs:435` |
| **SEC-007** | 安全 | Hook `updated_input` 可替换 Bash 命令，绕过所有上游策略检查 | `hooks/external.rs:125` |
| **C2-storage** | 存储 | `.meta.json` 非原子写，crash 导致 session 永久丢失 | `session.rs:826,863` |
| **M1 (interface)** | MCP | MCP dispatcher 两套实现行为已分叉（`initialize` 响应格式不同） | `main.rs:1922`, `mcp_server.rs:110` |

---

## P2 — 下迭代修复（次要架构问题）

| ID | 模块 | 问题 |
|----|------|------|
| SEC-004 | 安全 | Docker volume 读写挂载，可覆盖宿主机 `.git/hooks` |
| SEC-006 | 安全 | Rate limiter 以 key 值为 bucket，暴力破解不触发 429 |
| C-2 (core) | 核心 | `run_goal_loop` budget 超出路径的 TOCTOU（双重 clear_goal） |
| M-2 (core) | 核心 | intra-turn compaction 检测依赖字符串 `contains("[compacted:")` |
| M-5 (core) | 核心 | plan_mode 工具名硬编码在 ReAct loop 中 |
| M-2 (storage) | 存储 | `sqlite_vec` 全表扫描 + embedding 维度不匹配静默降级 |
| B2 (interface) | HTTP | SSE 端点无超时无 heartbeat，连接永久保活内存泄漏 |
| M3 (interface) | HTTP | session reaper task 未在启动时 spawn，会话只增不减 |

---

## P3 — 技术债（建议规划）

| 问题 | 涉及模块 |
|------|---------|
| `openai.rs.bak` 未删除，是活体混淆源（含旧 bug） | `llm/` |
| `providers.rs:54` `expect()` 违反 Invariant #5 | `providers.rs` |
| `cost.rs` 定价数据无维护机制，unknown model 写 `0.0` 而非 `null` | `cost.rs` |
| `MessageBus.messages` 无界增长，长期 agent pool 内存泄漏 | `multi.rs` |
| `SendMessageTool` 错误分类为 `ReadOnly`，实为状态变更 | `tools/send_message.rs` |
| `a2a.rs` async_mode 依赖 `python3`+`curl`，alpine 容器不可用 | `tools/a2a.rs` |
| `SshTransport` 硬编码 `StrictHostKeyChecking=no` | `tools/transport.rs` |
| WeChat `/l`、`/c`、`/r` 命令已声明但未实现 | `weixin/` |
| `list_sessions` HTTP 分页无稳定排序 | `http/handlers.rs` |
| Rate limiter bucket key 存储明文 API key | `http/rate_limit.rs` |
| 日志缺乏结构化字段和 trace_id | 全项目 |

---

## 跨模块系统性问题

### 1. Policy Sandbox 是孤岛

`PolicyConfig` 存在于 `ToolRegistry`，但 `invoke_with_audit` 从不自动调用它，依赖工具实现者主动集成。现有 `RunShell`、`ReadFile`、`WriteFile` 均未集成。架构声明的安全层在工具执行路径上完全缺席（SEC-008 + M-1 tools）。

**建议**：在 `invoke_with_audit` 框架层强制调用，不依赖约定。

### 2. 并行与串行语义不一致

- `spawn_workers_parallel.rs` 存在但 `multi.rs` 的 `TeamOrchestrator` 实际串行（M-4 LLM）
- `sub_agent.rs`/`spawn_worker.rs`/`spawn_workers_parallel.rs` 三者职责重叠，大量复制代码（N-1 tools）
- `build_sub_registry` 的只读子 registry 语义承诺被实现破坏（M-4 tools）

**建议**：统一并发 API，审计 `is_readonly_for_args` 所有调用链。

### 3. 原子写缺失

本地存储三处非原子写（`session_lock.rs`、`session.rs`、`storage/local.rs`），crash 均可产生损坏文件。这是统一的设计缺失，应建立 `atomic_write(path, bytes)` 工具函数供所有持久化路径使用。

### 4. 默认配置不安全

- HTTP 无认证（SEC-003）
- Docker volume 读写挂载（SEC-004）
- SSRF 无内网过滤（SEC-001）

三个问题都是"默认不安全"，偏离了最小权限原则。任何未经配置的部署都暴露在风险中。

---

## 正面评价（做得好的地方）

- **`resolve_within` 双重检查**（词法 + canonicalize）：路径遍历防护设计严谨，所有 fs 工具一致调用
- **`PROTECTED_PATHS` 组件级匹配**：使用 `Path::components()` 避免字符串误判，BypassPermissions 模式下仍强制执行
- **常量时间 API key 比较**：使用 XOR 累计防止时序侧信道
- **JWT 强制 `exp` 字段**：常见 JWT 实现遗漏，这里正确处理
- **TUI raw-mode RAII guard**：任何退出路径（含 panic）均恢复终端状态
- **自动分类器失败关闭**：JSON 解析失败默认 `block=true`
- **SubAgent 深度限制**：防止递归爆栈，边界有测试
- **`StrReplaceTool` 五级模糊匹配**：精准适配 LLM 输出特性
- **会话恢复的孤儿检测**：OrphanPolicy 机制对外部副作用有二次警告

---

## 各模块风险评估

| 模块 | 风险等级 | 主要关切 |
|------|---------|---------|
| 核心引擎 | 🔴 High | audit 错位静默数据损坏，goal TOCTOU |
| 工具系统 | 🔴 High | sub_registry 安全语义破坏，SSRF，命令注入 |
| LLM 提供者 | 🟠 Medium | SSE UTF-8 截断，多 Agent 伪并行，.bak 混淆 |
| **安全与权限** | 🔴 Critical | 默认无认证，SSRF，Policy sandbox 孤岛，sh -c 注入 |
| 持久化存储 | 🟠 Medium | 三处非原子写，session lock TOCTOU |
| 接口层 | 🟠 Medium | 无请求体限制 OOM，MCP 实现分叉，SSE 泄漏 |

---

## 建议工作顺序

```
Week 1 (P0 修复):
  1. HTTP 请求体限制 (5min)
  2. HTTP 默认认证 (10min)
  3. session_lock TOCTOU (20min)
  4. execute_tool_calls audit 错位 (30min)
  5. web_fetch SSRF 过滤 (30min)

Week 2 (P1 修复):
  6. Policy sandbox 接入调用链
  7. build_sub_registry 逻辑修复
  8. session.rs 原子写
  9. OpenAI SSE UTF-8 修复
  10. MCP dispatcher 合并

Week 3 (P2 + 技术债清理):
  11. 删除 openai.rs.bak
  12. multi.rs 真并行 + 超时
  13. Docker volume :ro
  14. session reaper spawn
  15. 结构化日志
```
