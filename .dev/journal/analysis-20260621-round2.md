# 代码质量分析：第2轮

**日期**: 2026-06-21  
**分析范围**: `src/llm/openai.rs`、`src/tools/web_fetch.rs`、`src/config.rs`、`src/http/handlers.rs`（P3-003 验证）、`src/llm/anthropic.rs`（测试覆盖审查）  
**前置条件**: 第1轮所有修复（P3-001/P3-003/P2-004）已合入 working tree  

---

## Clippy 检查结果

```
cargo clippy --all-targets --all-features -- -D warnings
→ 0 warnings, 0 errors ✓
```

---

## 第1轮遗留问题验证（P3-003）

**src/http/handlers.rs** 中 5 个 AG-UI 事件映射验证通过：

| AgentEvent 变体 | 映射为 Custom name | 状态 |
|---|---|---|
| `HookStarted` | `"agui-tui/hook_started"` | ✓ 正确 |
| `HookProgress` | `"agui-tui/hook_progress"` | ✓ 正确 |
| `HookFinished` | `"agui-tui/hook_finished"` | ✓ 正确 |
| `HookSystemMessage` | `"agui-tui/hook_system_message"` | ✓ 正确 |
| `TodoUpdated` | `"agui-tui/todo_updated"` | ✓ 正确 |

所有事件均携带了合理的 payload JSON，符合 AG-UI 协议。

---

## 新发现问题

### P2-005 ⚠️ [高] `web_fetch.rs:329` — 非 ASCII 内容截断时 UTF-8 边界 panic

**文件**: `src/tools/web_fetch.rs:329`

**问题描述**:

```rust
// 有问题的代码
let truncated_body = &body[..max_bytes];
```

`body` 是 `response.text()` 返回的 UTF-8 字符串。`max_bytes` 默认为 65536 字节，或由用户通过参数覆盖。当该字节偏移恰好落在多字节 UTF-8 字符（如中文、日文、Emoji）的中间时，Rust 的字符串切片会触发 **panic**（"byte index N is not a char boundary"），导致整个工具调用崩溃。

**对比**：同一文件第 303 行对错误体截断时已正确使用 `crate::truncate_str()`：

```rust
// 第 303 行——正确做法
format!("{}...", crate::truncate_str(&body, 200))
```

但第 329 行忘记使用，形成不一致。

**复现条件**: 使用 `WebFetch` 工具抓取包含中文/日文/表情符号的网页，且响应体大小 > `max_bytes`。

**修复方案**:

```rust
// 修复后
let truncated_body = crate::truncate_str(&body, max_bytes);
let msg = format!(
    "{}\n\n[…truncated at {} bytes; total body was {} bytes]",
    truncated_body, max_bytes, total_bytes
);
```

`crate::truncate_str` 已实现 UTF-8 安全截断（`src/lib.rs:155`），直接复用即可。

---

### P2-006 ⚠️ [中] `openai.rs` `process_sse_line` — 硬编码 `"openai"` 导致错误信息具误导性

**文件**: `src/llm/openai.rs:732-735`

**问题描述**:

```rust
let chunk: Value = serde_json::from_str(data).map_err(|e| Error::Llm {
    provider: "openai".into(),   // ← 硬编码
    message: format!("SSE parse error: {e}; data: {data}"),
})?;
```

`OpenAiProvider` 被用于 OpenAI、DeepSeek、GLM、Moonshot、Together、Ollama 等多个兼容服务商。当 DeepSeek 的 SSE 流解析失败时，错误消息显示 `provider: "openai"`，在日志中制造混乱，增加线上排障成本。

**原因**: `process_sse_line` 是静态方法（无 `&self`），无法访问 `self.model`，所以无法使用模型名。

**修复方案**: 给 `process_sse_line` 增加 `provider: &str` 参数，由 `parse_sse_stream` 传入 `&self.model`：

```rust
fn process_sse_line(
    line: &str,
    provider: &str,            // ← 新增
    content: &mut String,
    // …其余参数不变…
) -> Result<()> {
    // …
    let chunk: Value = serde_json::from_str(data).map_err(|e| Error::Llm {
        provider: provider.into(),   // ← 使用传入的名称
        message: format!("SSE parse error: {e}; data: {data}"),
    })?;
    // …
}
```

---

### P2-007 ⚠️ [中] `config.rs` 两个 stuck 检测测试不经过 `Config::from_env()`

**文件**: `src/config.rs:1252-1270`

**问题描述**:

```rust
#[test]
fn stuck_window_env_override() {
    std::env::set_var("RECURSIVE_STUCK_WINDOW", "5");
    let window = std::env::var("RECURSIVE_STUCK_WINDOW")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(10);     // ← 内联了解析逻辑，而非调用 Config::from_env()
    assert_eq!(window, 5);
    std::env::remove_var("RECURSIVE_STUCK_WINDOW");
}
```

这两个测试（`stuck_window_env_override` 和 `stuck_error_rate_env_override`）直接内联了与 `Config::from_env()` 相同的 env var 解析逻辑，而不是调用 `Config::from_env()` 本身。这意味着：

- 如果有人把 `Config::from_env()` 中的 env var 名从 `RECURSIVE_STUCK_WINDOW` 改成别的，这两个测试仍然通过，提供虚假的安全感。
- 其他 config 测试（如 `retry_env_overrides_apply`、`shell_timeout_default_and_env_override`）都正确地通过 `Config::from_env()` 验证，这里形成不一致。

**修复方案**: 改为调用 `Config::from_env()` 并断言 config 字段：

```rust
#[test]
fn stuck_window_env_override() {
    let _env_lock = crate::test_util::env_lock();
    std::env::set_var("RECURSIVE_MODEL", "test-model");
    std::env::set_var("RECURSIVE_API_KEY", "test-key");
    std::env::set_var("RECURSIVE_STUCK_WINDOW", "5");
    let config = Config::from_env().unwrap();
    assert_eq!(config.stuck_window, 5);
    std::env::remove_var("RECURSIVE_STUCK_WINDOW");
}
```

---

### P3-005 [低] `openai.rs` — `run_search_loop` 与 `run_stream_search_loop` ~80 行重复

**文件**: `src/llm/openai.rs:355-516`

两个方法的逻辑几乎完全相同：构建 `call_tools`、调用 LLM、检查 `ToolSearchTool`、更新消息历史、递归。唯一区别是调用 `self.complete()` vs `self.stream_inner()`，以及 `stream_tx` 参数。

目前代码量约 80×2=160 行，如果 ToolSearchTool 的解析逻辑需要变更，两处都要同步修改。可考虑提取共享部分为私有辅助方法，但鉴于异步 trait 对象的限制，重构有一定复杂度。建议作为下一轮 refactor 目标。

---

### P3-006 [低] `web_fetch.rs` — 2 个集成测试 `#[ignore]` 且无替代覆盖

**文件**: `src/tools/web_fetch.rs:536, 582`

`test_c_body_exceeds_max_bytes` 和 `web_fetch_tool_on_mock_server` 两个测试由于 SSRF 防护拦截 `127.0.0.1` 而被标记为 `#[ignore]`：

```rust
#[ignore = "hangs: SSRF guard blocks 127.0.0.1 before HTTP request is made"]
```

这导致：
1. **截断逻辑完全无测试覆盖**——而 P2-005 中的 bug 就在截断路径上。
2. 全工具集成路径（execute 方法本身）没有 passing 测试。

**建议修复**: 将截断逻辑提取为独立的 `truncate_body(body: &str, max_bytes: usize) -> String` 纯函数，单独编写单元测试，绕开 SSRF 防护限制。

---

## 问题汇总

| ID | 严重级别 | 文件 | 简述 |
|---|---|---|---|
| P2-005 | 高 | `src/tools/web_fetch.rs:329` | 非 ASCII 截断 UTF-8 panic |
| P2-006 | 中 | `src/llm/openai.rs:733` | 错误消息硬编码 "openai" 提供商名 |
| P2-007 | 中 | `src/config.rs:1252-1270` | 两个测试绕过 Config::from_env() |
| P3-005 | 低 | `src/llm/openai.rs:355-516` | search_loop / stream_search_loop 80行重复 |
| P3-006 | 低 | `src/tools/web_fetch.rs:536,582` | 2 个集成测试 ignored，截断路径无覆盖 |

**统计**: 高 1 个 / 中 2 个 / 低 2 个，共 5 个新问题

---

## 前3个最值得修复的问题

### 1. P2-005（首选修复）— web_fetch.rs UTF-8 panic

单行修复，零副作用，修复后可直接增加 `#[test]` 覆盖截断路径，连带解决 P3-006。改动仅 1 行：

```rust
// 改前
let truncated_body = &body[..max_bytes];
// 改后
let truncated_body = crate::truncate_str(&body, max_bytes);
```

### 2. P2-007 — config.rs 假测试

改动小（2 个测试，各约 5 行），但能修复两个长期存在的虚假安全感来源，让 stuck 检测配置真正有回归保护。需要用 `env_lock` 串行化，可参考 `shell_timeout_default_and_env_override` 的写法。

### 3. P2-006 — openai.rs 硬编码 provider 名称

改动中等（`process_sse_line` 加一个参数，调用方更新），影响 DeepSeek/GLM 等所有通过 OpenAI 适配器使用的服务商的错误可读性，且改动无语义风险。

---

## 排除项（第1轮遗留）

以下已知遗留问题本轮不分析：
- `runtime.rs` 文件过长（P2-001）
- `run_core.rs::run_inner` 函数过长（P2-002）  
- `handlers.rs` 文件过长（P2-003）
- `cli/output.rs` Plan Mode TODO（P3-002）
- `mcp.rs` 文件过长（P3-004）
