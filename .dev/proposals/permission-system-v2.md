# Proposal: Permission System V2 — Full Alignment with Production Agent Model

> **Status**: Draft — pending review
> **Created**: 2026-06-02
> **Baseline**: Current HEAD（`permissions.rs` 静态两层模型已落地）
> **Scope**: 10 个 Gap，4 个 Phase，完整对齐 fake-cc 的权限架构

---

## 背景与动机

当前 Recursive 的权限模型已完成初始实现（参见 `auth-and-permissions.md`）：

- `PermissionsConfig` — 静态 allow/deny/interactive 三列表 + 前缀通配符
- `HookRegistry` — Rust trait 生命周期 hooks（PreToolCall/PostToolCall 等）
- FS Sandbox — `resolve_within()` 强制工作区路径限制

这是一个**可用的最小权限系统**，但与生产级 Agent（如 Claude Code）相比存在 10 个显著 Gap。本提案定义如何将 Recursive 的权限系统演进到生产级能力。

### 参照系

对比对象：fake-cc（Claude Code）源码中的 `src/utils/permissions/permissions.ts`、`src/types/permissions.ts`、`src/Tool.ts`，以及 `EnterPlanModeTool` / `ExitPlanModeV2Tool`。

---

## Gap 清单与优先级

| # | Gap | 当前状态 | 影响 | Phase |
|---|-----|---------|------|-------|
| G1 | 无权限 Mode | 只有全局 allow/deny | 无法 bypassPermissions/dontAsk/acceptEdits | P1 |
| G1b | plan-mode 未纳入 PermissionMode | AtomicBool 与权限系统正交 | 模式组合语义缺失 | P1 |
| G2 | 无内容感知规则 | 只能按工具名匹配 | 无法 `shell(git *)` 精细控制 | P2 |
| G3 | 无 AI 分类器 (auto mode) | — | 无法自动决策 | P4 |
| G4 | 无多源规则分层 | 只有全局 config | 无 project/session 级规则 | P1 |
| G5 | 工具无 `check_permissions` | 工具不参与权限决策 | shell 子命令无法细粒度控制 | P2 |
| G6 | 无安全路径保护 | — | `.git/.recursive` 可被 bypass 修改 | P2 |
| G7 | 无运行时规则更新 | 需重启生效 | session 内无法动态授权 | P3 |
| G8 | 无决策原因追踪 | 只有 Allowed/Denied | 调试/审计困难 | P1 |
| G9 | Hook 不可外部进程化 | 只有 Rust trait | 可扩展性弱 | P3 |
| G10 | 无无头 Agent 专用路径 | — | 后台 agent 无法处理权限 | P3 |

---

## Phase 1 — 权限基础架构（G1 / G1b / G4 / G8）

### P1-1：PermissionMode 枚举

**目标**：将权限决策模式化，plan-mode 纳入权限体系。

```rust
// src/permissions.rs

/// 权限决策模式。
///
/// - Default: 遇到需要确认的工具时，通过 interactive 列表或 Hook 询问用户
/// - AcceptEdits: 工作区内文件写入操作自动放行（类 acceptEdits）
/// - BypassPermissions: 跳过几乎所有检查（危险，仅 CI/测试用）
/// - DontAsk: 将所有 ask 转为 deny（非交互无头模式）
/// - Plan: 只读探索阶段，写工具被拦截；保存进入前的 mode 以供还原
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    Default,
    AcceptEdits,
    BypassPermissions,
    DontAsk,
    Plan {
        /// 进入 plan-mode 前的模式，exit_plan_mode 时还原
        pre_plan_mode: Box<PermissionMode>,
        /// 若进入前是 BypassPermissions，则 plan-mode 下人工确认仍走 bypass
        bypass_available: bool,
    },
}

impl Default for PermissionMode {
    fn default() -> Self {
        Self::Default
    }
}
```

**配置集成**（`config.toml`）：

```toml
[permissions]
mode = "default"   # default | acceptEdits | bypassPermissions | dontAsk
```

**plan-mode 工具对接**：

```rust
// EnterPlanModeTool::execute() 修改
async fn execute(&self, _args: Value) -> Result<String> {
    let current_mode = self.permissions.read().mode.clone();
    let bypass_available = matches!(current_mode, PermissionMode::BypassPermissions);
    let new_mode = PermissionMode::Plan {
        pre_plan_mode: Box::new(current_mode),
        bypass_available,
    };
    self.permissions.write().mode = new_mode;
    // 保留 exploring_plan_mode AtomicBool 向后兼容，由 mode 驱动设置
    self.gate.exploring_plan_mode.store(true, Ordering::Relaxed);
    Ok(json!({ "entered": true }).to_string())
}

// ExitPlanModeTool::execute() 修改
async fn execute(&self, arguments: Value) -> Result<String> {
    let mut perms = self.permissions.write();
    let restored_mode = if let PermissionMode::Plan { pre_plan_mode, .. } = &perms.mode {
        *pre_plan_mode.clone()
    } else {
        PermissionMode::Default
    };
    perms.mode = restored_mode;
    self.gate.exploring_plan_mode.store(false, Ordering::Relaxed);
    // ... 原有审批等待逻辑不变 ...
}
```

**`check_static()` 的 mode 语义**：

```rust
pub fn check_static(&self, tool_name: &str, is_readonly: bool) -> Permission {
    // plan mode 拦截所有写工具（exit_plan_mode 豁免）
    if let PermissionMode::Plan { bypass_available, .. } = &self.mode {
        if !is_readonly && tool_name != "exit_plan_mode" {
            if *bypass_available {
                // bypass 继承：写操作仍放行，由 interactive 层决定
            } else {
                return Permission::Denied(DecisionReason::Mode(self.mode.clone()),
                    "write tools are blocked in plan mode".into());
            }
        }
    }

    // bypassPermissions：跳过 allow/deny 检查
    if matches!(self.mode, PermissionMode::BypassPermissions) {
        // 安全路径保护仍生效（Phase 2 实现）
        return Permission::Allowed(DecisionReason::Mode(self.mode.clone()));
    }

    // dontAsk：将 interactive 列表工具视为 deny
    if matches!(self.mode, PermissionMode::DontAsk) {
        if self.interactive.iter().any(|p| matches_pattern(p, tool_name)) {
            return Permission::Denied(DecisionReason::Mode(self.mode.clone()),
                format!("tool `{tool_name}` requires interaction but mode is dontAsk"));
        }
    }

    // 原有 deny/allow 规则
    // ...（现有逻辑保持）
}
```

---

### P1-2：多源规则分层

**目标**：allow/deny/interactive 规则按来源分层，session 层优先于 project 层优先于 user 层。

```rust
/// 规则来源优先级（高到低）
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum RuleSource {
    Session,   // 运行时动态添加（最高优先级）
    Project,   // .recursive/config.toml（项目级）
    User,      // ~/.recursive/config.toml（全局用户）
}

/// 分层权限配置
#[derive(Debug, Clone, Default)]
pub struct LayeredPermissionsConfig {
    pub mode: PermissionMode,
    pub layers: Vec<PermissionLayer>,
}

#[derive(Debug, Clone)]
pub struct PermissionLayer {
    pub source: RuleSource,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub interactive: Vec<String>,
}
```

**合并语义**：deny 取并集（任一层 deny 即拒绝）；allow 取交集（必须所有 allow 都放行）；interactive 取并集。

**配置加载顺序**：
1. `~/.recursive/config.toml` → `RuleSource::User`
2. `<project>/.recursive/config.toml` → `RuleSource::Project`
3. 运行时 API → `RuleSource::Session`

---

### P1-3：决策原因追踪（DecisionReason）

**目标**：`Permission` 枚举携带决策原因，便于调试和审计日志。

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Permission {
    Allowed(DecisionReason),
    Denied(DecisionReason, String),  // (reason, human-readable message)
    /// 工具自身未决定，由上层 mode/rules 决定（对应 fake-cc 的 passthrough）
    Passthrough,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionReason {
    /// 被某条规则决定
    Rule { source: RuleSource, pattern: String },
    /// 被当前权限模式决定
    Mode(PermissionMode),
    /// 被 Hook 决定
    Hook { name: String },
    /// 被安全路径保护决定（bypass 也无法绕过）
    SafetyCheck { path: String },
}
```

向后兼容：现有调用 `check_static()` 只关心 `is Allowed` 的地方无需修改接口。

---

## Phase 2 — 规则能力增强（G2 / G5 / G6）

### P2-1：内容感知规则

**目标**：支持 `shell(git *)` 格式，按命令内容精细匹配。

规则语法扩展：`<toolname>(<content_pattern>)`

示例：
```toml
[permissions]
allow = ["read_file", "shell(git *)", "shell(cargo test*)"]
deny  = ["shell(rm -rf*)"]
interactive = ["shell(npm publish*)"]
```

**解析器**：

```rust
pub struct PermissionRuleValue {
    pub tool_name: String,
    pub content_pattern: Option<String>,  // None = 整个工具
}

fn parse_rule(s: &str) -> PermissionRuleValue {
    if let Some(idx) = s.find('(') {
        let tool_name = s[..idx].to_string();
        let content = s[idx+1..s.len()-1].to_string();  // strip 括号
        PermissionRuleValue { tool_name, content_pattern: Some(content) }
    } else {
        PermissionRuleValue { tool_name: s.to_string(), content_pattern: None }
    }
}
```

**工具接入点** — `prepare_permission_matcher` trait 方法（见 P2-2）。

---

### P2-2：工具级 `check_permissions`

**目标**：`Tool` trait 增加可选的 `check_permissions` 方法，允许工具参与权限决策。

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    // ... 现有方法 ...

    /// 工具级权限检查。默认返回 Passthrough（由外部规则决定）。
    /// run_shell 实现：解析子命令并按 content 规则匹配。
    async fn check_permissions(
        &self,
        args: &Value,
        mode: &PermissionMode,
    ) -> Permission {
        Permission::Passthrough
    }

    /// 为权限匹配器准备内容提取函数。
    /// run_shell 实现：返回 Some(|pattern| command.starts_with(pattern_prefix))
    fn prepare_permission_matcher(
        &self,
        args: &Value,
    ) -> Option<Box<dyn Fn(&str) -> bool + Send + Sync>> {
        None
    }
}
```

**`run_shell` 实现**：

```rust
fn prepare_permission_matcher(
    &self,
    args: &Value,
) -> Option<Box<dyn Fn(&str) -> bool + Send + Sync>> {
    let command = args["command"].as_str()?.to_string();
    Some(Box::new(move |pattern: &str| {
        matches_pattern(pattern, &command)
    }))
}
```

**`ToolRegistry::invoke()` 集成**：

```rust
// 在静态规则检查后，调用工具自身的 check_permissions
let tool_perm = tool.check_permissions(&arguments, &perms.mode).await;
match tool_perm {
    Permission::Denied(reason, msg) => return Err(Error::PermissionDenied { reason, message: msg }),
    Permission::Passthrough => { /* 继续上层规则 */ }
    Permission::Allowed(_) => { /* 工具主动放行 */ }
}
```

---

### P2-3：安全路径保护

**目标**：部分路径即使在 `bypassPermissions` 模式下也必须保护。

**受保护目录**（硬编码）：

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
];
```

**在 `check_static()` 中插入**（优先级最高，在 bypassPermissions 检查之前）：

```rust
// 安全路径检查 — bypass 不豁免
if let Some(path) = extract_file_path(tool_name, args) {
    for protected in PROTECTED_PATHS {
        if path.contains(protected) {
            return Permission::Denied(
                DecisionReason::SafetyCheck { path: path.to_string() },
                format!("writing to `{path}` requires explicit user confirmation"),
            );
        }
    }
}
```

---

## Phase 3 — 运行时控制（G7 / G9 / G10）

### P3-1：运行时规则更新

**目标**：session 期间可动态添加/删除规则，无需重启。

**设计**：

```rust
// Arc<RwLock<...>> 使工具和运行时共享可变权限状态
pub type SharedPermissions = Arc<RwLock<LayeredPermissionsConfig>>;

impl LayeredPermissionsConfig {
    pub fn add_session_rule(&mut self, behavior: RuleBehavior, pattern: String) {
        let session_layer = self.layers.iter_mut()
            .find(|l| l.source == RuleSource::Session)
            .expect("session layer always present");
        match behavior {
            RuleBehavior::Allow => session_layer.allow.push(pattern),
            RuleBehavior::Deny => session_layer.deny.push(pattern),
            RuleBehavior::Interactive => session_layer.interactive.push(pattern),
        }
    }

    pub fn remove_session_rule(&mut self, behavior: RuleBehavior, pattern: &str) {
        // ... 类似实现 ...
    }
}
```

**HTTP 接口**（`/permissions` 端点，可选）：

```
POST /sessions/{id}/permissions
{ "action": "add", "behavior": "allow", "pattern": "shell(git *)" }

DELETE /sessions/{id}/permissions
{ "behavior": "deny", "pattern": "shell(rm *)" }
```

---

### P3-2：外部 Hook 进程化

**目标**：`~/.recursive/hooks/` 下的可执行文件可参与工具生命周期。

**Hook 发现**：启动时扫描 `~/.recursive/hooks/` 和 `<project>/.recursive/hooks/`。

**协议**（stdin/stdout JSON）：

```
输入（传给 hook 进程 stdin）:
{
  "event": "PreToolCall",
  "tool_name": "run_shell",
  "args": { "command": "git status" },
  "mode": "default"
}

输出（hook 进程 stdout）:
{
  "action": "continue" | "skip" | "error",
  "message": "optional reason"
}
```

**实现**：新建 `src/hooks/external.rs`，实现 `Hook` trait，用 `tokio::process::Command` 调用外部可执行文件，JSON 读写 stdin/stdout，超时 5s。

---

### P3-3：无头 Agent 权限路径

**目标**：`AgentConfig` 支持 `headless` 模式，无法交互时自动走 hook 路径或 deny。

```rust
pub struct AgentConfig {
    // ... 现有字段 ...
    /// 无头模式：interactive 列表的工具不等待用户输入，走 PermissionRequest hooks
    pub headless: bool,
}
```

**`ToolRegistry::invoke()` 集成**：

```rust
if config.headless && perms.is_interactive(tool_name) {
    // 先走外部 hooks
    let hook_decision = registry.dispatch_permission_hooks(tool_name, args).await;
    match hook_decision {
        HookAction::Continue => { /* hook 放行 */ }
        HookAction::Skip => return Err(Error::PermissionDenied {
            reason: DecisionReason::Hook { name: "PermissionRequest".into() },
            message: "auto-denied: headless mode, no hook decision".into(),
        }),
        HookAction::Error(msg) => return Err(Error::PermissionDenied { ... }),
    }
}
```

---

## Phase 4 — AI 智能分类（G3）

### P4-1：Auto Mode — LLM 分类器

**目标**：新增 `Auto` 权限模式，用同一 LLM provider 评估是否放行工具调用。

```rust
pub enum PermissionMode {
    // ... 其他模式 ...
    Auto,  // LLM 分类器决策
}
```

**分类器 Prompt 模板**（`src/permissions/auto_classifier.rs`）：

```
You are a security classifier for an AI coding agent.
Current tool call:
  tool: {tool_name}
  args: {args_summary}

Recent conversation context (last 10 turns):
{transcript_snippet}

Is this action safe to auto-approve?
Respond with JSON: { "block": true|false, "reason": "..." }
```

**调用**：通过现有 `LlmProvider` trait，低 temperature (0.0)，60s 超时。

**连续拒绝计数器**：

```rust
pub struct DenialTracker {
    pub consecutive: u32,
    pub total: u32,
}

const MAX_CONSECUTIVE: u32 = 3;
const MAX_TOTAL: u32 = 10;
```

超限后：交互模式回退人工确认；无头模式抛出 `AgentOutcome::finish = FinishReason::PermissionDenialLimit`。

---

## 实施路线图

### 各 Phase 任务拆分

| Goal | Phase | 范围 | 文件 | 工作量 |
|------|-------|------|------|--------|
| G-P1-1a | P1 | PermissionMode 枚举 + plan-mode 对接 | `permissions.rs`, `tools/plan_mode.rs` | M |
| G-P1-1b | P1 | mode 语义集成到 `check_static()` | `permissions.rs`, `tools/mod.rs` | M |
| G-P1-2 | P1 | 多源规则分层 (`LayeredPermissionsConfig`) | `permissions.rs`, `config_file.rs` | M |
| G-P1-3 | P1 | `DecisionReason` + `Permission` 枚举扩展 | `permissions.rs` | S |
| G-P2-1 | P2 | 内容感知规则解析器 | `permissions.rs` | S |
| G-P2-2 | P2 | `Tool::check_permissions` + `prepare_permission_matcher` | `tools/mod.rs`, `tools/shell.rs` | M |
| G-P2-3 | P2 | 安全路径保护 | `permissions.rs`, `tools/fs.rs` | S |
| G-P3-1 | P3 | 运行时规则更新 API + HTTP 端点 | `permissions.rs`, `http.rs` | M |
| G-P3-2 | P3 | 外部 Hook 进程化 | `hooks/external.rs`（新文件） | M |
| G-P3-3 | P3 | 无头 Agent 权限路径 | `agent.rs`, `permissions.rs` | S |
| G-P4-1 | P4 | Auto mode + LLM 分类器 + 拒绝计数器 | `permissions/auto_classifier.rs`（新文件） | L |

**工作量评级**: S = ~0.5天, M = ~1天, L = ~2天
**总估算**: ~11 工作天

---

### 批次建议

```
Batch A（可并行）: G-P1-1a + G-P1-2 + G-P1-3  （无文件冲突）
Batch B（依赖 A）: G-P1-1b（mode 语义）
Batch C（可并行）: G-P2-1 + G-P2-2 + G-P2-3
Batch D（可并行）: G-P3-1 + G-P3-2 + G-P3-3
Batch E（最后）:   G-P4-1（依赖稳定的 P1-P3）
```

---

## 设计原则与约束

1. **向后兼容**：默认配置（空 `[permissions]` 段）行为与当前完全一致。
2. **不侵入 agent.rs 主循环**：所有新权限逻辑在 `ToolRegistry::invoke()` 或工具层。
3. **`plan-mode` 双轨并行**：`exploring_plan_mode: AtomicBool` 保留以向后兼容，但其状态由 `PermissionMode::Plan` 驱动，不再独立控制。
4. **测试全覆盖**：每个新 public 函数/类型/模式对应至少一个单元测试。
5. **`cargo clippy -D warnings` 全绿**：Phase 每结束前强制验证。

---

## 不做的事（本版本范围外）

- RBAC 角色模型（后续通过 `PermissionLayer::roles` 扩展）
- macOS 系统对话框（先用 stdin/stderr）
- 权限审计日志持久化（Phase 15 tracing 已覆盖基础记录）
- 跨会话的 session 规则持久化（仅内存，重启清空）
- JWT/OAuth2 认证（预共享密钥满足当前场景）
