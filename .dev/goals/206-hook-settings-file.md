# Goal 206 — Hook System V2 P1-3: Settings 文件 + Matcher 过滤

**Roadmap**: Hook System V2 — Phase 1 基础对齐
**提案**: `.dev/proposals/hook-system-v2.md`
**依赖**: Goal 205（HookResult 扩展）

**Design principle check**:
- 新建 `src/hooks/config.rs` — Hook 配置 Schema + 加载
- 修改 `src/hooks/external.rs` — 支持 from_config 构造
- 修改 `src/main.rs` — 加载 hooks.json 并初始化 ExternalHookRunner
- ❌ 不破坏旧目录扫描（向后兼容 fallback）

## Why

当前 hook 配置只能通过目录扫描（`~/.recursive/hooks/`），存在三个问题：
1. **无过滤能力**：所有 hook 对所有工具都触发，无法按工具名或参数过滤
2. **超时硬编码**：5s 全局超时，无法为不同 hook 单独配置
3. **无结构化配置**：无法在配置文件中声明 hook 列表、顺序、条件

fake-cc 用 `settings.json` 的 `hooks` 字段解决了所有这些问题。

## Scope

### 1. 新建 `src/hooks/config.rs`

```rust
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 整体 hook 配置（加载自 hooks.json）。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct HooksConfig {
    /// 事件名 -> 匹配器列表（对应 fake-cc 的 `hooks` 字段）。
    #[serde(flatten)]
    pub events: HashMap<String, Vec<HookMatcher>>,
}

/// 一组带过滤条件的 hook。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HookMatcher {
    /// 过滤模式（None = 匹配所有）。
    /// 语法：
    ///   "run_shell"           — 工具名精确匹配
    ///   "run_shell(git *)"    — 工具名 + command 参数前缀
    ///   "write_file(src/*)"   — 工具名 + path 参数前缀
    pub matcher: Option<String>,
    /// 匹配时执行的 hook 列表（顺序执行，遇到非 Continue 短路）。
    pub hooks: Vec<HookCommand>,
}

/// 单个 hook 命令。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HookCommand {
    /// Hook 类型。
    pub r#type: HookCommandType,
    /// Shell 命令（type=command）。
    pub command: Option<String>,
    /// HTTP URL（type=http）。
    pub url: Option<String>,
    /// LLM prompt（type=prompt/agent）。
    pub prompt: Option<String>,
    /// 超时秒数（默认 5）。
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    /// Spinner 展示消息。
    pub status_message: Option<String>,
    /// true = 执行一次后移除。
    #[serde(default)]
    pub once: bool,
    /// true = 后台执行，不阻塞 Agent。
    #[serde(default)]
    pub r#async: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HookCommandType {
    Command,
    Http,
    Prompt,
    Agent,
}

fn default_timeout() -> u64 { 5 }
```

加载函数：
```rust
pub fn load_hooks_config(dirs: &[std::path::PathBuf]) -> HooksConfig {
    for dir in dirs {
        let path = dir.join("hooks.json");
        if path.exists() {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(cfg) = serde_json::from_str::<HooksConfig>(&text) {
                    return cfg;
                }
            }
        }
    }
    HooksConfig::default()
}
```

### 2. Matcher 评估函数

```rust
pub fn matches_hook(matcher: &Option<String>, tool_name: &str, args: &serde_json::Value) -> bool {
    let Some(pattern) = matcher else { return true };

    // "run_shell(git *)" => tool="run_shell", arg_pattern="git *"
    if let Some(idx) = pattern.find('(') {
        let tool_pat = &pattern[..idx];
        let arg_pat = pattern[idx+1..].trim_end_matches(')');
        if tool_name != tool_pat { return false }
        // 从 args 中提取第一个字符串字段值进行前缀匹配
        if let Some(first_str) = first_string_arg(args) {
            return glob_match(arg_pat, &first_str);
        }
        return false;
    }
    // 纯工具名匹配
    tool_name == pattern.as_str()
}

fn first_string_arg(args: &serde_json::Value) -> Option<String> {
    if let Some(obj) = args.as_object() {
        // 优先找 command/path/goal 字段
        for key in &["command", "path", "goal", "input"] {
            if let Some(v) = obj.get(*key).and_then(|v| v.as_str()) {
                return Some(v.to_string());
            }
        }
        // fallback: 第一个字符串字段
        obj.values().find_map(|v| v.as_str().map(|s| s.to_string()))
    } else {
        None
    }
}

fn glob_match(pattern: &str, value: &str) -> bool {
    // 简单前缀通配：只支持结尾 *
    if let Some(prefix) = pattern.strip_suffix('*') {
        value.starts_with(prefix)
    } else {
        value == pattern
    }
}
```

### 3. 修改 `ExternalHookRunner`

新增 `from_config` 构造器（保留 `discover` 作为 fallback）：

```rust
impl ExternalHookRunner {
    /// 从结构化配置构造（推荐）。
    pub fn from_config(config: HooksConfig) -> Self { ... }

    /// 向后兼容：扫描目录中的可执行文件。
    pub fn discover(dirs: &[PathBuf]) -> Self { ... }
}
```

内部将 `HooksConfig` 转为 `Vec<ResolvedHook>`（展平后带 matcher/timeout/once/async 标志）。

dispatch 时按 matcher 过滤，再按 timeout 各自超时。

### 4. `src/main.rs` 集成

在 `build_runtime` 中加载 hooks.json（`~/.recursive/` 和 `<workspace>/.recursive/` 两处）：
```rust
let hook_dirs = [home_recursive_dir, workspace_recursive_dir];
let hooks_config = load_hooks_config(&hook_dirs);
let hook_runner = if hooks_config.events.is_empty() {
    // fallback to directory scan
    ExternalHookRunner::discover(&hook_dirs)
} else {
    ExternalHookRunner::from_config(hooks_config)
};
```

## Tests to add

1. `hooks_config_deserializes_from_json` — 完整 JSON 正确解析
2. `hooks_config_empty_is_default` — 空文件/不存在时返回 Default
3. `matcher_none_matches_all_tools` — None matcher 对任意工具名返回 true
4. `matcher_tool_name_exact` — "run_shell" 只匹配 run_shell
5. `matcher_tool_name_with_arg_prefix` — "run_shell(git *)" 匹配 git 开头命令
6. `matcher_tool_name_with_arg_prefix_no_match` — "run_shell(git *)" 不匹配 "ls -la"
7. `load_hooks_config_reads_from_dir` — 实际写文件后能加载
8. `from_config_respects_per_hook_timeout` — 每个 hook 使用各自的超时

## Acceptance

- `cargo test --workspace` 绿色
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- `hooks.json` 配置文件能被加载并执行
- matcher 过滤正确工作（不匹配的工具调用不触发 hook）
- 每个 hook 可独立配置超时
- 旧目录扫描方式仍然有效（向后兼容）
