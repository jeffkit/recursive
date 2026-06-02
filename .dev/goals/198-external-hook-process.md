# Goal 198 — P3-2: 外部 Hook 进程化

**Roadmap**: Permission System V2 — Phase 3 运行时控制

**依赖**: Goal 192（Permission/DecisionReason）

**Design principle check**:
- 新建 `src/hooks/external.rs`，实现外部进程 Hook
- ❌ 不修改 `agent.rs` 主循环

## Why

当前 Hook 系统只有 Rust trait 实现，无法从外部脚本或工具扩展。
生产级 agent（如 Claude Code）支持用户在 `~/.recursive/hooks/` 放置
可执行文件来拦截工具调用。外部 Hook 极大提升了系统可扩展性，
无需重新编译即可定制权限行为。

## Scope

### 1. 新建 `src/hooks/mod.rs`（若不存在）

```rust
pub mod external;
pub use external::{ExternalHookRunner, HookAction};
```

### 2. 新建 `src/hooks/external.rs`

```rust
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

const HOOK_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HookEvent {
    PreToolCall,
    PostToolCall,
    PermissionRequest,
}

#[derive(Debug, Clone, Serialize)]
pub struct HookInput {
    pub event: HookEvent,
    pub tool_name: String,
    pub args: serde_json::Value,
    pub mode: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HookOutput {
    pub action: HookAction,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HookAction {
    Continue,
    Skip,
    Error,
}

pub struct ExternalHookRunner {
    hooks: Vec<PathBuf>,
}

impl ExternalHookRunner {
    /// 扫描 hook 目录，收集可执行文件
    pub fn discover(dirs: &[PathBuf]) -> Self {
        let mut hooks = Vec::new();
        for dir in dirs {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if is_executable(&path) {
                        hooks.push(path);
                    }
                }
            }
        }
        Self { hooks }
    }

    /// 运行所有匹配的 hook，返回第一个非 Continue 的决策
    pub async fn dispatch(&self, input: &HookInput) -> HookAction {
        for hook in &self.hooks {
            match self.run_hook(hook, input).await {
                Ok(output) if output.action != HookAction::Continue => {
                    return output.action;
                }
                _ => continue,
            }
        }
        HookAction::Continue
    }

    async fn run_hook(&self, path: &PathBuf, input: &HookInput) -> Result<HookOutput> {
        let input_json = serde_json::to_string(input)
            .map_err(|e| Error::Config { message: format!("hook input serialize: {e}") })?;

        let child = Command::new(path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| Error::Config { message: format!("hook spawn {}: {e}", path.display()) })?;

        // 写 stdin + 读 stdout，带超时
        let output = timeout(HOOK_TIMEOUT, async {
            use tokio::io::AsyncWriteExt;
            let mut child = child;
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(input_json.as_bytes()).await;
            }
            child.wait_with_output().await
        })
        .await
        .map_err(|_| Error::Config { message: format!("hook timeout: {}", path.display()) })?
        .map_err(|e| Error::Config { message: format!("hook wait: {e}") })?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        serde_json::from_str::<HookOutput>(&stdout)
            .map_err(|e| Error::Config { message: format!("hook output parse: {e}") })
    }
}

fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}
```

### 3. Hook 目录约定

启动时扫描（按优先级）：
1. `~/.recursive/hooks/`
2. `<workspace>/.recursive/hooks/`

### 4. 注册到现有 `HookRegistry`

若 `src/hooks.rs` 或 `src/hook_registry.rs` 已存在，将
`ExternalHookRunner` 作为实现 `Hook` trait 的适配器注册进去。

### 5. 单元测试

- `discover_skips_non_executable`: 目录中有不可执行文件，不被收集
- `dispatch_continue_when_no_hooks`: hooks 为空时返回 Continue
- `hook_output_parse_continue`: `{"action":"continue"}` 解析为 Continue
- `hook_output_parse_skip`: `{"action":"skip","message":"blocked"}` 解析为 Skip
- `hook_timeout_returns_err`: 模拟挂起的 hook → 超时错误（用 mock 脚本）

## Acceptance

- `cargo test --workspace` 绿色
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- `ExternalHookRunner::discover` 正确扫描可执行文件
- `dispatch` 在 5s 内超时返回错误，不阻塞主流程
- stdin/stdout JSON 协议符合规格

## Notes for the agent

- Windows 不支持 Unix 执行位；`is_executable` 在 Windows 上可返回
  `path.extension() == Some("exe")`。当前只需支持 Unix/macOS。
- Hook 脚本接收完整 JSON 后必须在 5s 内输出响应；超时视为 Continue（宽松策略），
  避免用户的 hook 脚本误配置导致 agent 卡住。
- `dispatch` 返回第一个非 Continue 的决策即停止；若需要所有 hook 都同意才放行，
  可在后续迭代调整策略。
