# Manual edit: stream-reasoning-live

**Date**: 2026-06-18
**Goal**: 思考内容只在答案流式输出后才整块补在顶部，体验不对。理想是先流式输出
思考，再流式输出答案。改造为：思考逐 token 实时流式，渲染在答案之上。

之前 reasoning 只在 step 结束时作为单个 `AgentEvent::Reasoning` 整块发出；流式
通道 `StreamSender = UnboundedSender<String>` 只承载答案文本。本次把流式通道升级
为带类型的 `StreamChunk`，让 provider 同时实时推送 reasoning / text 增量。

**Files touched**:
- `src/llm/chat.rs`：新增 `StreamChunk { Text(String), Reasoning(String) }`，
  `StreamSender` 改为 `UnboundedSender<StreamChunk>`。
- `src/llm/mod.rs`：导出 `StreamChunk`；默认 `stream` 兜底实现先发 `Reasoning`
  再发 `Text`。
- `src/llm/anthropic.rs`：`thinking` 块初始文本 + `thinking_delta` 实时转发为
  `StreamChunk::Reasoning`；`text`/`text_delta` 转发为 `StreamChunk::Text`。
  更新流式测试断言，新增 `test_e2` 校验 reasoning 先于 text 实时到达。
- `src/llm/openai.rs`：`reasoning_content` SSE 增量实时转发为
  `StreamChunk::Reasoning`；`content` 增量转发为 `StreamChunk::Text`。
- `src/llm/mock.rs`：兜底先发 reasoning 再发 text。
- `src/event.rs`：新增 `AgentEvent::PartialReasoning { text, step }`。
- `src/run_core.rs`：转发任务按 `StreamChunk` 类型分派为 `PartialToken` /
  `PartialReasoning`；step 结束仍发 `Reasoning` 作为权威全文 finaliser。
- `src/tui/events.rs`：新增 `UiEvent::ReasoningPartial { text }`。
- `src/tui/backend.rs`：`PartialReasoning → ReasoningPartial` 映射。
- `src/tui/model.rs`：`TranscriptBlock::Reasoning` 增加 `streaming: bool`。
- `src/tui/app/event_loop.rs`：`ReasoningPartial` 走 `append_streaming_reasoning`
  （创建/追加 streaming Reasoning 块，天然位于 streaming Assistant 块之上）；
  `Reasoning` 走 `finalise_streaming_reasoning`（找到 streaming 块定稿；无则按
  原逻辑插到 streaming Assistant 之前或 push，兼容非流式 / 内联 `<think>` 回收）。
- `src/tui/ui/transcript.rs`：render 匹配臂改 `{ text, .. }`；更新两处测试构造。

**Tests added**:
- `llm::anthropic::test_e2_stream_thinking_deltas_populate_reasoning`（断言 chunk
  顺序：Reasoning, Reasoning, Text）
- `tui::app::event_loop::reasoning_partials_stream_then_finalise_above_answer`
- `tui::app::event_loop::reasoning_without_partials_inserts_before_streaming_answer`
- 更新 openai/anthropic 既有流式测试断言为 `StreamChunk`。

**追加修复（真流式）**：用户反馈思考仍像假流式——全部内容到齐后才一把显示思考、
再放答案。根因：`AnthropicProvider::parse_sse_stream` 用 `resp.text().await` 把整个
HTTP 响应**缓冲完**才逐行处理，所有增量在响应结束时才一次性推出。OpenAI provider
则用 `resp.bytes_stream()` 增量处理（真流式）。已将 Anthropic 改为同样的增量字节流：
- `parse_sse_stream` 改用 `resp.bytes_stream()` + UTF-8 安全的跨 chunk 缓冲（避免多字节
  中文被 lossy 解码截断），逐 `\n` 切行实时处理。
- 抽出 `process_sse_line(&self, line, &mut SseAccum, &stream_tx)` 方法 + `SseAccum`
  累加器结构承载跨行状态（含 `current_event`）。
- 新增 `use futures_util::StreamExt;`。
现在 reasoning / text 增量在到达网络的瞬间即转发给 UI，真正逐 token 流式。

**Notes**: HTTP/SSE 与 AGUI 转换层的 `AgentEvent` 匹配都是 `_ => None`/`_ => {}`
兜底，`PartialReasoning` 目前对它们是 no-op（SDK 侧暂不透出思考增量，后续可加
`SseEvent` 变体）。run_core 在 LLM 调用返回后 `await` 转发任务，保证所有
reasoning/text 增量在 finaliser 事件之前送达 UI，避免重复块。模型先思考后回答，
故 reasoning 块先建、Assistant 块后建，视觉顺序自然正确。
