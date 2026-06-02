# Goal 208 — Hook System V2 P2-2: Prompt Hook 类型

**Roadmap**: Hook System V2 — Phase 2 类型扩展
**提案**: `.dev/proposals/hook-system-v2.md`
**依赖**: Goal 206（Settings 文件 + Matcher）

**Design principle check**:
- 修改 `src/hooks/external.rs`（或新建 `src/hooks/prompt.rs`）— 实现 Prompt hook 执行器
- Prompt hook 调用 LLM，需复用现有 `LlmProvider` trait

## Why

Prompt hook 允许用自然语言描述策略（而非写 shell 脚本），LLM 负责判断
是否允许/拒绝工具调用。适合复杂的内容感知策略（如"禁止删除 .env 文件"）。

fake-cc 的 prompt hook 支持在 `$ARGUMENTS` 占位符处注入工具调用 JSON，
用指定模型（默认轻量快速模型）评估，返回 `HookOutput` 格式。

## Scope

### 1. `PromptHookConfig` 结构体

```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PromptHookConfig {
    /// Prompt 模板，$ARGUMENTS 会被替换为序列化的 HookInput JSON。
    pub prompt: String,
    /// 使用的模型（None = 使用 Config 默认模型）。
    pub model: Option<String>,
    /// 超时秒数（默认 30）。
    #[serde(default = "default_prompt_timeout")]
    pub timeout: u64,
    pub status_message: Option<String>,
    pub once: Option<bool>,
}

fn default_prompt_timeout() -> u64 { 30 }
```

### 2. Prompt Hook 执行逻辑

```rust
async fn run_prompt_hook(
    llm: &Arc<dyn LlmProvider>,
    config: &PromptHookConfig,
    input: &HookInput,
) -> Result<HookResult> {
    let args_json = serde_json::to_string(input)
        .unwrap_or_else(|_| "{}".to_string());
    let prompt = config.prompt.replace("$ARGUMENTS", &args_json);

    let completion = timeout(
        Duration::from_secs(config.timeout),
        llm.complete(&[Message::user(prompt)], &[]),
    ).await
    .map_err(|_| Error::Config { message: "prompt hook timeout".into() })?
    .map_err(|e| Error::Config { message: format!("prompt hook llm error: {e}") })?;

    // LLM 输出应为 JSON 格式（与 HookOutput 相同）
    let output: HookOutput = serde_json::from_str(completion.content.trim())
        .unwrap_or(HookOutput { action: JsonAction::Continue, ..Default::default() });

    Ok(output.into_hook_result())
}
```

### 3. `ExternalHookRunner` 持有 `Arc<dyn LlmProvider>`（可选）

Prompt hook 需要 LLM 访问，因此 `ExternalHookRunner::from_config` 接受一个可选的 LLM：

```rust
impl ExternalHookRunner {
    pub fn from_config_with_llm(
        config: HooksConfig,
        llm: Option<Arc<dyn LlmProvider>>,
    ) -> Self { ... }
}
```

若没有 LLM 但配置了 prompt hook，则 fallback 到 `Continue`（warn log）。

### 4. 分派路由

```rust
match command.r#type {
    HookCommandType::Command => self.run_command_hook(...).await,
    HookCommandType::Http => self.run_http_hook(...).await,
    HookCommandType::Prompt | HookCommandType::Agent => {
        if let Some(llm) = &self.llm {
            self.run_prompt_hook(llm, ...).await
        } else {
            Ok(HookResult::continue_default())
        }
    }
}
```

`Agent` 类型与 `Prompt` 类型行为相同（简化实现，不启动独立 agent 子进程）。
可在后续 Goal 中将 `Agent` 升级为真正的 sub-agent 调用。

## Tests to add

1. `prompt_hook_replaces_arguments_placeholder` — `$ARGUMENTS` 被正确替换
2. `prompt_hook_uses_llm_response` — MockProvider 返回 JSON，hook 解析正确
3. `prompt_hook_falls_back_on_non_json_response` — LLM 返回非 JSON 时 → Continue
4. `prompt_hook_timeout_returns_continue` — LLM 超时时 → Continue
5. `no_llm_prompt_hook_returns_continue` — 未配置 LLM 时 → Continue + warn

## Acceptance

- `cargo test --workspace` 绿色
- `cargo clippy` 干净
- `hooks.json` 中配置 `type: "prompt"` 后，工具调用时 LLM 被调用以评估
- 测试中使用 MockProvider，不依赖外部 API
