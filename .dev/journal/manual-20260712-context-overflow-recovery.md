# Manual edit: context-overflow-recovery

**Date**: 2026-07-12
**Goal**: 在 compaction 阈值计算修复之外，增加一道"响应式"防护：当 LLM API 因上下文溢出返回错误时，自动触发紧急压缩并重试，而不是直接崩溃。
**Files touched**:
- `src/runtime.rs`

**Tests added**:
- `runtime::tests::is_context_window_exceeded_*`（3 个检测函数测试）
- `runtime::tests::compact_on_overflow_*`（2 个强制压缩测试）
- `runtime::tests::context_overflow_triggers_compact_and_retry`（端到端恢复测试）

**Notes**:

### 分析结论

原有代码中，context window 溢出错误（HTTP 400 `context_length_exceeded`）会被 `?` 直接传播，整个 `run()` 调用失败，用户看不到任何回复。

### 新增机制

**`is_context_window_exceeded(err) -> bool`**（自由函数）
- 检测 `Error::Llm.message` 是否包含上下文溢出特征关键词
- 覆盖 OpenAI / NVIDIA NIM / DeepSeek / 各主流 provider 的常见措辞

**`compact_on_overflow() -> Result<bool>`**（AgentRuntime 私有方法）
- 不做阈值检查，强制执行一次 compaction
- 与 `maybe_compact_cross_turn` 共享相同的事件发送和 hook 调用逻辑
- transcript 过短无法 compact 时返回 `false`，调用方直接传播原始错误

**`run()` 中的错误恢复路径**：
```rust
let turn_outcome = match self.execute_kernel_turn().await {
    Ok(outcome) => outcome,
    Err(e) if is_context_window_exceeded(&e) => {
        // compact then retry once
        if self.compact_on_overflow().await? {
            self.execute_kernel_turn().await?
        } else {
            return Err(e);
        }
    }
    Err(e) => return Err(e),
};
```

### Buffer 大小分析

当前 token 阈值（GLM-5.2 200K context）：
- `threshold_prompt_tokens = 144K`（80% of 180K effective window）
- 触发时剩余 56K tokens 余量（28% 窗口）

两层防护：
1. **主动防护**：每轮结束后，`maybe_compact_cross_turn` 检查 prompt_tokens，超过 144K 则压缩
2. **响应式防护**：若主动防护未能及时触发（例如单轮增量过大），overflow 错误发生后立即压缩并重试

### 真实会话重放说明

针对问题会话（72 条消息 ≈ 199K tokens）的重放验证被 NVIDIA NIM API 不稳定性阻断：
- `GET /v1/models` 响应正常（<2s）
- `POST /v1/chat/completions`（非流式）约 50 秒才响应
- `POST /v1/chat/completions`（流式，recursive CLI 默认）直接返回 404

单元测试以 MockProvider 完整模拟了"首次调用 → context overflow 错误 → compact → 重试成功"的全路径，验证逻辑正确性与 API 无关。
API 稳定后可运行：
```bash
RECURSIVE_MODEL=z-ai/glm-5.2 \
RECURSIVE_API_KEY=$NVIDIA_API_KEY \
RECURSIVE_API_BASE=https://integrate.api.nvidia.com/v1 \
RECURSIVE_COMPACT_THRESHOLD=400000 \
  recursive --workspace /Users/kong/projects \
  resume 2026-07-12T03-06-15Z-Users-kong-projects \
  -p "Hi, quick test"
```
