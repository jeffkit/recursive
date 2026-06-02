# Goal 207 — Hook System V2 P2-1: HTTP Hook 类型

**Roadmap**: Hook System V2 — Phase 2 类型扩展
**提案**: `.dev/proposals/hook-system-v2.md`
**依赖**: Goal 206（Settings 文件 + Matcher）

**Design principle check**:
- 修改 `src/hooks/external.rs`（或新建 `src/hooks/http.rs`）— 实现 HTTP hook 执行器
- ❌ 不新增 Cargo.toml 依赖（reqwest 已在 Cargo.toml 中）

## Why

当前外部 hook 只能是本地可执行文件。HTTP hook 允许将事件 POST 到
任意 webhook URL（如自建权限服务、审计平台、CI 系统），无需在本机
安装脚本，极大提升集成灵活性。

## Scope

### 1. `HookCommandType::Http` 执行逻辑

在 `ExternalHookRunner` 中增加 `run_http_hook` 方法：

```rust
async fn run_http_hook(
    &self,
    config: &HttpHookConfig,
    input: &HookInput,
    timeout_secs: u64,
) -> Result<HookResult> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .connect_timeout(Duration::from_secs(timeout_secs))
        .build()
        .unwrap_or_default();

    let mut builder = client.post(&config.url).json(input);

    // 注入自定义 headers（支持 $VAR 环境变量插值）
    if let Some(headers) = &config.headers {
        for (k, v) in headers {
            let interpolated = interpolate_env_vars(v, &config.allowed_env_vars);
            builder = builder.header(k, interpolated);
        }
    }

    let resp = builder.send().await.map_err(|e| Error::Config {
        message: format!("http hook request failed: {e}"),
    })?;

    let body = resp.text().await.map_err(|e| Error::Config {
        message: format!("http hook response read failed: {e}"),
    })?;

    let output: HookOutput = serde_json::from_str(body.trim()).map_err(|e| Error::Config {
        message: format!("http hook response parse failed: {e}"),
    })?;

    Ok(output.into_hook_result())
}
```

环境变量插值（`$VAR` 或 `${VAR}`）：只插值 `allowed_env_vars` 白名单中的变量。

### 2. `HttpHookConfig` 结构体

```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HttpHookConfig {
    pub url: String,
    pub headers: Option<HashMap<String, String>>,
    pub allowed_env_vars: Option<Vec<String>>,
    pub timeout: Option<u64>,
    pub status_message: Option<String>,
    pub once: Option<bool>,
}
```

在 `HookCommand::type = "http"` 时使用此配置。

### 3. 集成到 dispatch 路由

在 `ExternalHookRunner::run_single_hook` 中按 type 分派：
- `command` → 现有进程执行逻辑
- `http` → `run_http_hook`

## Tests to add

1. `http_hook_posts_json_input` — 使用 `mockito` 或 `wiremock` 验证 POST body
2. `http_hook_parses_response` — 服务器返回 `{"action":"skip"}` 被正确解析
3. `http_hook_timeout_returns_continue` — 服务器无响应时超时 → Continue（fail-open）
4. `http_hook_connection_error_returns_continue` — 无法连接时 → Continue
5. `env_var_interpolation_respects_allowlist` — 只插值白名单变量
6. `env_var_interpolation_empty_for_non_allowed` — 非白名单变量替换为空串

## Notes

- 测试中禁止使用真实外部 URL，必须用 mock server（`mockito` 或类似）
- reqwest client 必须设置显式 connect_timeout（防止测试挂起，见 AGENTS.md 规则）

## Acceptance

- `cargo test --workspace` 绿色（包括 mock server 测试）
- `cargo clippy` 干净
- `hooks.json` 中配置 `type: "http"` 后，工具调用时事件被 POST 到指定 URL
- 服务器返回的 `additionalContext`/`updatedInput` 被正确应用
