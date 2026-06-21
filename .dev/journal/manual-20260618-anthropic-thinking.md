# Manual edit: anthropic-thinking

**Date**: 2026-06-18
**Goal**: TUI 不显示思考内容。根因：用户配置走 DeepSeek 的 Anthropic 兼容端点
(`api_base = https://api.deepseek.com/anthropic`, `type = anthropic`,
`model = deepseek-v4-flash`)，而 `AnthropicProvider` 从未解析 extended-thinking
块——非流式把 `thinking` 块当 `ContentBlock::Unknown` 丢弃，流式只处理
`text_delta`/`input_json_delta`，`reasoning_content` 硬编码为 `None`。服务端确实
在产出思考（用户看到“思考耗时”），但内容被 provider 全部扔掉。OpenAI provider
从 SSE 的 `reasoning_content` 字段捕获思考，所以早期用 OpenAI provider 能看到。

**Files touched**:
- `src/llm/anthropic.rs`：
  - 流式 `parse_sse_stream`：`content_block_start` 处理 `thinking` 块的初始文本；
    `content_block_delta` 新增 `thinking_delta` 分支，累积到 `reasoning_content`
    （不转发到 `stream_tx`，让思考渲染成独立块，与 OpenAI provider 行为一致）；
    `Completion.reasoning_content` 按累积结果填充。
  - 非流式 `ContentBlock` 新增 `Thinking { thinking }` 变体，`parse_completion`
    累积到 `reasoning_content`；`redacted_thinking` 仍走 `Unknown`。
- `src/run_core.rs`：移除临时诊断日志（写 `/tmp/recursive-reasoning-debug.log`）。

**Tests added**:
- `parses_thinking_block_into_reasoning_content`（非流式）
- `test_e2_stream_thinking_deltas_populate_reasoning`（流式，校验思考进
  `reasoning_content` 而答案文本进 `content`，且思考不转发到 live stream）

**Notes**: 未发送 `thinking` 请求参数——DeepSeek 端点对 `deepseek-v4-flash`
默认产出思考，无需显式开启；只需在 provider 侧捕获。若后续接官方 Anthropic
extended thinking（需 `thinking: {type:"enabled", budget_tokens}`、temperature=1
等约束），再在 `build_request` 里按需启用。思考目前作为单个 `Reasoning` 事件在
step 结束时一次性发出（非逐 token 流式），渲染为独立思考块。
