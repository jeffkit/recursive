# 2026-09-27 — OpenAI 协议 assistant tool_calls 空 content 发空串致严格网关 400

**Date:** 2026-09-27
**Goal:** issue #16 — 修 `src/llm/openai.rs::serialize_message`：带 `tool_calls`
的 assistant 消息无条件写 `"content": ""`，被转发到 Anthropic Messages API 的
OpenAI 兼容网关（Bedrock 后端 claude-*）拒绝，报 HTTP 400
`messages: text content blocks must be non-empty`，导致首次工具调用之后的每一轮都失败。

**Files touched:**
- `src/llm/openai.rs` — `serialize_message`：仅当 `content` 非空、或该消息没有
  `tool_calls` 时才写入 `content`；其余情况省略该键（OpenAI 规范中带 `tool_calls`
  时 `content` 可空；bisect 显示省略与 `null` 均返回 200，省略更贴合规范）。
  新增 3 个单测覆盖三种形状。

**Tests added:**
- `serialize_message_assistant_with_tool_calls_omits_empty_content`（回归，issue 主诉）
- `serialize_message_assistant_with_tool_calls_keeps_non_empty_content`（防止误删有文本的 content）
- `serialize_message_keeps_empty_content_without_tool_calls`（无 tool_calls 的空串消息形状不变）

**Notes:**
- 原生 Anthropic 路径 (`src/llm/anthropic.rs::serialize_message`) 本就只发 `tool_use`
  block，不受影响；本改动只动 OpenAI 协议侧。
- 手工构造第二轮请求（user → assistant(tool_calls, 空文本) → tool）并 dump
  序列化结果，确认 assistant 条目只剩 `role` + `tool_calls`，wire 形状符合
  issue 中 bisect 的“omitted → 200”变体。
- 质量门：`cargo test --workspace` 全绿（38 个 test binary，0 failed）；
  `cargo clippy --all-targets --all-features -- -D warnings` 干净；`cargo fmt --all` 已跑。
