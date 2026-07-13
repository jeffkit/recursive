# Goal 326 — stream-json `num_turns` 语义核对 + Client 模式文档化 (P2)

**Roadmap**: Claude Code JSON 对齐 follow-up — Goal 327 的姊妹项（小）。

**Design principle check**:
- Implemented as: `crates/recursive-cli/src/cli/claude_json.rs` 顶部的
  module doc-comment 补充 Client 模式（`--input-format stream-json` 多
  turn）语义说明；`num_turns` 字段加一行 doc 解释「本 turn 的 agent-loop
  step 数」。**无运行时行为改动**，除非核对发现 Claude 语义确实不同（见
  Scope §3）。
- ❌ Does NOT branch inside `src/run_core.rs::RunCore::run_inner`.
- ❌ Does NOT 改 `result` 事件的发送时机（每 turn 一个 result 是对齐
  Claude Client 模式的，不是 bug —— 见 Why）。
- ❌ Does NOT 重写 stream 模型、不动 `main.rs` 的多 turn 循环结构。

## Why

Goal 327 的设计讨论中纠正了一个之前的误判：我曾称「Claude 契约是一个
run 一个终态 result，多 turn = 多独立 run」，并据此把 `run
--input-format stream-json` 的「单 stream 多 turn、每 turn 一个 result」
判定为对齐 bug。**这个判定是错的。**

Claude Agent SDK 的 `--input-format stream-json` Client 模式（`ClaudeSDKClient`）
本身就是：保持 stdin 开放、同一 session 里接收多个 `query()`、每个 query
产出一个 `result` 事件（官方 Python SDK 文档示例明确展示同一 client 连续
`query` + `receive_response`，"Follow-up in same session"、"maintains state
across queries"）。所以 Recursive 当前行为**已对齐**，不需要重构 stream
模型。

唯一遗留的语义问题是 `result.num_turns`：`claude_json.rs:410`
`let num_turns = if steps > 0 { steps } else { self.num_turns };` 报的是**本
turn 的 agent-loop step 数**（即该 query 内的 LLM 调用轮数），不是「跨
query 的累计 turn 数」。这导致 E2E case `multiturn: num_turns rose to 3`
断言失败（已临时改为「≥2 个 result 事件」）。本 goal 核对 Claude 的
`num_turns` 究竟是 per-query 还是 cumulative，据此决定是只补文档、还是
微调字段含义，并把 E2E 断言改成对齐正确语义的稳定形式。

## Scope (do exactly this, no more)

### 1. 核对 Claude `num_turns` 语义

通过 Claude Agent SDK 官方文档（`code.claude.com/docs/en/agent-sdk/python`）
与 CLI 协议（社区整理 `cli-protocol.md`）确认：Client 模式下每个 `result`
的 `num_turns` 计的是**该 query 的 turn 数**还是**跨 query 累计**。把结论
写进 journal 与 `claude_json.rs` doc-comment。预期结论：per-query（因
每个 `result` 对应一次独立 `query()`），与 Recursive 的 `steps` 语义基本
一致。

### 2. 文档化 Client 模式行为（必做）

在 `crates/recursive-cli/src/cli/claude_json.rs` 顶部 module doc-comment
补充 4–8 行，说明：

- `--input-format stream-json` Client 模式 = 同一进程/session 内多 turn，
  每个 turn 结束发一个 `result`，对齐 Claude `ClaudeSDKClient`。
- `result.num_turns` = 本 turn 的 agent-loop step 数（该 query 内 LLM 调用
  轮数），**不是**跨 turn 累计。
- 单 turn（`run` 不带 `--input-format stream-json`）仍是 init + events +
  一个终态 result。

不动运行时代码。

### 3. 若核对发现 Claude 语义不同（条件做）

仅当 §1 核对结论与 Recursive 当前 `num_turns = steps` **不一致**时，才调整
`build_result` 的 `num_turns` 计算，并同步更新单测
`claude_json.rs::result_success_shape`（`assert_eq!(r["num_turns"], 2)`）
与 `build_turn_result_matches_emitter`。若一致，跳过本节，只做 §2。

### 4. E2E 断言对齐

`e2e/tests/40-claude-json-stream.yaml` 的 `multiturn: a second result event
proves the follow-up turn ran` case 当前断言「≥2 个 result 事件」。确认这是
稳定语义后保留；若 §1 给出更精确的 per-query num_turns 值，可补一个断言
「第二个 result 的 `num_turns` 反映 follow-up query 的 step 数」。**不要**
恢复之前错误的 `num_turns:3` 累计断言。

## Acceptance

- `cargo fmt --all` clean。
- `cargo clippy --all-targets --all-features -- -D warnings` clean。
- `cargo test --workspace` green（含 `claude_json` 单测）。
- `.dev/scripts/e2e-run.sh claude-json-stream` 12/12 green（0.14.2）。
- `claude_json.rs` doc-comment 明确记录 Client 模式 + `num_turns` 语义。
- journal 记录 §1 核对结论（per-query vs cumulative）与依据链接。

## Out of scope (defer)

- 重构 stream 模型 / 改 `result` 发送时机（无必要，已对齐）。
- `recursive loop` 的 session 持久化 —— 见 Goal 327。
- TUI `/loop` slash command —— 见 Goal 323 与后续。
- HTTP SSE 路径的 result 语义（已对齐，不动）。

## Notes for the agent

- 先读 `crates/recursive-cli/src/cli/claude_json.rs`（`build_result`、
  `build_turn_result`、`num_turns` 计算）与 `main.rs:2009-2066`（多 turn
  循环 + `finish_without_result`）。
- 先读 `.dev/journal/manual-20260710-argusai-0142-followup.md` 了解 E2E
  case 已做的临时修复。
- 核对 Claude 语义用 `WebFetch`
  `https://code.claude.com/docs/en/agent-sdk/python`（RunResult /
  `num_turns` 字段说明）+ 搜索 `ClaudeSDKClient` 多 query 行为。
- 若仅做文档（§2），改动量 < 20 行，无运行时风险。
- **DO NOT** 改 `result` 事件数量或时机。**DO NOT** 改 `main.rs` 多 turn
  循环结构。
- 提交时不要加 Co-authored-by。
