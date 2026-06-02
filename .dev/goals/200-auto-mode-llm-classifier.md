# Goal 200 — P4-1: Auto Mode — LLM 权限分类器

**Roadmap**: Permission System V2 — Phase 4 AI 智能分类

**依赖**: Goal 190（PermissionMode）、Goal 191（LayeredPermissionsConfig）、
Goal 192（DecisionReason）、Goal 197（SharedPermissions）、Goal 198（hooks）

**Design principle check**:
- 新建 `src/permissions/auto_classifier.rs`
- 修改 `src/permissions.rs`，添加 `Auto` variant
- ❌ 不修改 `agent.rs` 主循环

## Why

在复杂 session 中，静态规则难以覆盖所有情况。`Auto` 模式通过调用同一
LLM provider 对工具调用进行安全分类，自动决定放行或拒绝，减少用户中断。
连续拒绝计数器防止分类器陷入循环拒绝，提供安全降级路径。

## Scope

### 1. `src/permissions.rs` — 添加 `Auto` variant

```rust
pub enum PermissionMode {
    Default,
    AcceptEdits,
    BypassPermissions,
    DontAsk,
    Plan { pre_plan_mode: Box<PermissionMode>, bypass_available: bool },
    Auto,  // LLM 分类器决策
}
```

### 2. 新建 `src/permissions/auto_classifier.rs`

```rust
use crate::error::Result;
use crate::llm::LlmProvider;
use std::sync::Arc;

const CLASSIFIER_PROMPT: &str = r#"
You are a security classifier for an AI coding agent.
Current tool call:
  tool: {tool_name}
  args: {args_summary}

Recent conversation context (last 10 turns):
{transcript_snippet}

Is this action safe to auto-approve?
Respond ONLY with JSON (no markdown): { "block": true|false, "reason": "..." }
"#;

#[derive(Debug, Default)]
pub struct DenialTracker {
    pub consecutive: u32,
    pub total: u32,
}

const MAX_CONSECUTIVE: u32 = 3;
const MAX_TOTAL: u32 = 10;

impl DenialTracker {
    pub fn record_denial(&mut self) {
        self.consecutive += 1;
        self.total += 1;
    }
    pub fn record_allow(&mut self) {
        self.consecutive = 0;
    }
    pub fn is_over_limit(&self) -> bool {
        self.consecutive >= MAX_CONSECUTIVE || self.total >= MAX_TOTAL
    }
}

pub struct AutoClassifier {
    provider: Arc<dyn LlmProvider>,
    pub tracker: DenialTracker,
}

#[derive(serde::Deserialize)]
struct ClassifierResponse {
    block: bool,
    reason: String,
}

impl AutoClassifier {
    pub fn new(provider: Arc<dyn LlmProvider>) -> Self {
        Self { provider, tracker: DenialTracker::default() }
    }

    pub async fn classify(
        &mut self,
        tool_name: &str,
        args_summary: &str,
        transcript_snippet: &str,
    ) -> Result<(bool, String)> {
        // 超限则直接 block（不再调用 LLM）
        if self.tracker.is_over_limit() {
            return Ok((true, "denial limit reached".into()));
        }

        let prompt = CLASSIFIER_PROMPT
            .replace("{tool_name}", tool_name)
            .replace("{args_summary}", args_summary)
            .replace("{transcript_snippet}", transcript_snippet);

        // 调用 LLM，temperature=0.0，timeout=60s
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            self.provider.complete_simple(&prompt, 0.0),
        )
        .await
        .map_err(|_| crate::error::Error::Config {
            message: "auto classifier timeout".into(),
        })??;

        match serde_json::from_str::<ClassifierResponse>(&response) {
            Ok(r) => {
                if r.block {
                    self.tracker.record_denial();
                } else {
                    self.tracker.record_allow();
                }
                Ok((r.block, r.reason))
            }
            Err(_) => {
                // 解析失败 → 保守策略：放行（不误伤），记录 log
                Ok((false, "classifier parse error, defaulting to allow".into()))
            }
        }
    }
}
```

### 3. `ToolRegistry::invoke()` — Auto mode 分支

```rust
if matches!(perms.mode, PermissionMode::Auto) {
    let args_summary = serde_json::to_string(&arguments)
        .unwrap_or_else(|_| "{}".into());
    let (block, reason) = classifier.classify(tool_name, &args_summary, "").await?;
    if block {
        // 检查是否超限，触发 FinishReason
        if classifier.tracker.is_over_limit() {
            return Err(Error::PermissionDenied {
                reason: DecisionReason::Mode(PermissionMode::Auto),
                message: "permission denial limit reached".into(),
            });
        }
        return Err(Error::PermissionDenied {
            reason: DecisionReason::Mode(PermissionMode::Auto),
            message: reason,
        });
    }
}
```

### 4. `LlmProvider` trait 扩展

新增简化调用方法（若不存在）：

```rust
async fn complete_simple(&self, prompt: &str, temperature: f32) -> Result<String>;
```

各 provider 实现该方法，使用单条 user message 调用。

### 5. 单元测试

- `denial_tracker_consecutive`: 3 次连续 denial → `is_over_limit() == true`
- `denial_tracker_reset_on_allow`: deny×2 → allow → deny×1 → consecutive==1
- `denial_tracker_total_limit`: 10 次 total → over limit
- `classifier_parse_block_true`: JSON `{"block":true,"reason":"unsafe"}` → block
- `classifier_parse_allow`: JSON `{"block":false,"reason":"ok"}` → allow
- `classifier_parse_error_defaults_allow`: 非 JSON 响应 → allow（保守策略）
- `classifier_over_limit_skips_llm`: tracker 超限 → 不调用 provider，直接 block

## Acceptance

- `cargo test --workspace` 绿色（含上述 7 个测试，LLM 调用用 mock provider）
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- `PermissionMode::Auto` 在 `check_static()` 中正确路由到分类器
- 连续 3 次拒绝后触发 limit，不再调用 LLM
- mock provider 可注入测试，不需真实 API key

## Notes for the agent

- `complete_simple` 的 mock 实现：`MockProvider` 返回预设 JSON 字符串。
- `transcript_snippet` 当前传空字符串；完整对话上下文提取留给后续优化。
- Auto mode 是 Phase 4，仅在 P1-P3 完全稳定后实现；本 Goal 是最后一个 goal。
- 分类器调用使用现有 `LlmProvider`，不新增 API 密钥或 provider 配置。
- `AgentOutcome::FinishReason::PermissionDenialLimit` 若不存在，在
  `src/agent.rs` 中添加该 variant（仅新增枚举 variant，不修改主循环逻辑）。
