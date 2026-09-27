# manual-20260927-openai-empty-content-tool-calls

- **Date:** 2026-09-27
- **Goal:** fix issue #16 — OpenAI 协议下带 tool_calls 的 assistant 消息发 `content: ""`，被严格 Anthropic 转译网关以 400 `text content blocks must be non-empty` 拒绝。
- **Files touched:** `src/llm/openai.rs`
- **Changes:**
  - `serialize_message`：当 `role == Assistant && content.is_empty() && !tool_calls.is_empty()` 时发 `content: null`（OpenAI 自身对 tool-call-only assistant 消息的 canonical wire shape），否则维持原字符串。anthropic.rs 原生路径本就只发 tool_use 块，不受影响。
  - 新增 3 个回归测试：空 content + tool_calls → null；非空 content + tool_calls → 保留文本；tool result 空 content → 仍是字符串（界定 null 路径的作用域）。
- **Tests added:** `serialize_message_assistant_tool_calls_empty_content_is_null` / `serialize_message_assistant_tool_calls_keeps_nonempty_content` / `serialize_message_tool_result_empty_content_keeps_string`
- **Notes:** 选择 `null` 而非省略 key：reporter bisect 两者均 200，`null` 与 OpenAI 官方回放形态一致，且避免严格索引 `msg["content"]` 的转译层 KeyError。选非空 content 时保留文本，信息不丢。
