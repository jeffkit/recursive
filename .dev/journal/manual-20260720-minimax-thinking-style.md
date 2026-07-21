# Manual edit: minimax-thinking-style

**Date**: 2026-07-20
**Goal**: 修复国内 MiniMax-M3 在 TUI 里思考过程与结束样式的问题。

**症状**:
1. 流式阶段：思考内容以普通助手绿点 `•` 输出，没有 `∴ Thinking…` 头。
2. 结束后：标题仍是进行时 `∴ Thinking…`，不像 Claude Code 的 `∴ Thought for Xs`。

**根因**:
- MiniMax-M3 默认把 chain-of-thought 嵌在 `content` 的 `<think>…</think>` 里，
  不走 `reasoning_content` SSE 字段。流式时走 `AssistantPartial`（绿点），
  只有 turn 结束时 `extract_inline_reasoning` 才把它抬进 Reasoning 块。
- TUI `render_reasoning` 忽略 `streaming` 标志，定稿后仍写 `Thinking…`。

**Fix**:
1. `src/llm/openai.rs` — MiniMax 模型请求体加 `reasoning_split: true`，
   让思考走专用 `reasoning_content` 增量（已有 `StreamChunk::Reasoning`
   管道）。其它 provider 不带该字段。
2. TUI — Reasoning 块增加 `duration_ms`；流式显示 `∴ Thinking…`，
   定稿显示 `∴ Thought for Xs`（无时长则 `∴ Thought`）。

**Files touched**:
- `src/llm/openai.rs`
- `crates/recursive-tui/src/model.rs`
- `crates/recursive-tui/src/ui/transcript.rs`
- `crates/recursive-tui/src/app/event_loop.rs`
- `crates/recursive-tui/src/app/render.rs`

**Tests added**:
- `llm::openai::builds_request_minimax_sets_reasoning_split`
- `llm::openai::model_wants_reasoning_split_detects_minimax_token`
- `ui::transcript::reasoning_block_streaming_shows_thinking_header_and_italic_body`
- `ui::transcript::reasoning_block_finalised_shows_thought_for_duration`
- `ui::transcript::reasoning_block_finalised_without_duration_shows_thought`
- 更新 `reasoning_partials_stream_then_finalise_above_answer` 断言 duration stamp

**Notes**:
- Impact: `build_request` MEDIUM（仅 OpenAI adapter 请求体）；
  `render_reasoning` LOW。
- 需重新 `cargo build` / 重装 binary 后，用 `minimax-cn` + MiniMax-M3
  验证：流式应先见 `∴ Thinking…` 灰斜体，定稿变为 `∴ Thought for Xs`，
  答案仍用绿点 `•`。
- `tui-mutants.sh` 未跑（手动编辑 advisory，per CLAUDE.md）。
