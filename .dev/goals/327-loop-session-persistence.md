# Goal 327 — `recursive loop` session 持久化 (P1)

**Roadmap**: Claude Code JSON 对齐 follow-up — loop 一等公民化（主菜）。
与 Goal 326（stream-json 语义文档化）配套；与 Goal 323（TUI loop driver）
共享「loop = turn 的事件驱动编排器，turn 属于同一 session」模型。

**Design principle check**:
- Implemented as: `crates/recursive-cli/src/main.rs::run_loop` 接入
  `SessionWriter` + `SessionPersistenceSink`（复用 `run_once` 在
  `main.rs:1853-1870` 的装配模式），把每个 `runtime.run()` 的
  `AgentEvent` 落进同一个 `transcript.jsonl`。loop 编排逻辑留在
  `src/runtime.rs::run_loop`（runtime 层，不绑定 CLI 进程）以便 TUI
  `/loop`（Goal 323）复用。
- ❌ Does NOT branch inside `src/run_core.rs::RunCore::run_inner`
  (invariant #1)。
- ❌ Does NOT 改 `src/runtime.rs::run_loop` 的编排语义（wakeup/睡眠/
  next_goal 逻辑不动），只让 CLI 层把 session sink 喂进去。
- ❌ Does NOT 给 loop 加 per-turn stdout 流式输出（见 Out of scope）；
  loop 仍以结束时的 summary 为 stdout 产物，**仅加落盘**。
- ❌ Does NOT 动 TUI、HTTP、`run_event_loop`。

## Why

`recursive loop` 当前是「进程内多 turn 执行器」：`runtime.run_loop()`
（`src/runtime.rs:935-958`）在同一个 runtime / 同一份内存 transcript 上
反复调 `runtime.run(next_goal)`，turn 间上下文连续。但它**没接
`SessionWriter`**——对比 `run_once` 在 `main.rs:1854-1858` 组了
`CompositeSink([ChannelSink, SessionPersistenceSink(sw)])`，`run_loop`
（`main.rs:1741-1757`）直接 `AgentRuntimeBuilder::new()...build()`，不传
`event_sink`。所以 transcript 只在内存里有，跑完即丢：**没有
`transcript.jsonl`**，不能 `resume`、不能 `episodic_recall`、E2E 不能用
`recursive-session:` 断言（`10-loop-mode.yaml` 现在只能断言文件产出）。

按「loop 只是 turn 的事件驱动编排器，turn 属于同一 session 的多轮对话」
模型，loop 跑出的本就应是一个正常 session。本 goal 补上这条落盘链路，让
loop 成为一等公民。

## Scope (do exactly this, no more)

### 1. `crates/recursive-cli/src/main.rs::run_loop` — 接入 session 写入

在 builder 装配前，参照 `run_once`（`main.rs:1809-1870`）加 session
writer 装配：

1. `run_loop` 签名新增 `session: bool`（即 `!cli.no_session`）与
   `name: Option<String>` 两个参数。调用点 `main.rs:819` 同步传
   `!cli.no_session` 与 `cli.name.clone()`。
2. 当 `session == true`：
   - `SessionWriter::create_with_tools(&config.workspace, &goal, &config.model, &config.provider_type)`（与 `run_once:1810` 同构）。
   - 若 `name` 为 `Some`，`writer.set_name(name)`。
   - 失败时 `eprintln!("session: failed to create session writer: {e}")`
     并继续（不 bail，与 `run_once` 一致）。
3. **event_sink 只挂 `SessionPersistenceSink`，不挂 `ChannelSink`**：
   loop 没有 printer 在 drain channel，挂 ChannelSink 会撑爆无界 channel
   或死锁。即
   `let event_sink: Option<Arc<dyn EventSink>> = sw.as_ref().map(|w| Arc::new(SessionPersistenceSink::new(w.clone())) as Arc<dyn EventSink>);`
   然后 `if let Some(s) = event_sink { builder = builder.event_sink(s); }`
   （builder 支持 `.event_sink`，见 `cli/builder.rs:482`）。
4. `runtime.run_loop(...)` 结束后，用
   `cli::output::finalize_session_writer(sw, status)` 收尾，`status` 由
   最后一个 outcome 的 `finish_reason` 决定（见 §2）。
5. `--no-session` 时全程不建 writer，行为与今天一致（无落盘）。

### 2. session status 映射

最后一个 `RuntimeOutcome` 的 `finish_reason` → `SessionStatus`：

- `NoMoreToolCalls` → `SessionStatus::Completed`
- `Cancelled` → `SessionStatus::Interrupted`
- 其余（`BudgetExceeded` / `Stuck` / `TranscriptLimit` / `ProviderStop` /
  `PermissionDenialLimit`）→ `SessionStatus::Interrupted`

（与 `run_once`/resume 对 error finish 的处理一致；不新增 `Failed` 状态。）
在 `.meta.json` 现有字段基础上**不强制加新字段**；若方便可加
`turn_count = outcomes.len()`，但非必须——以 `transcript.jsonl` 的
message_count 为准。

### 3. `src/runtime.rs::run_loop` — 不改编排，只确认 sink 透传

`runtime.run_loop` 内部调 `self.run(next_goal)`，`run` 会通过 runtime
已设的 `event_sink` 派发 `AgentEvent`。因此只要 CLI 层在 build 时设了
`SessionPersistenceSink`，每个 turn 的消息会自动 append 进同一 session
目录。**`runtime.rs::run_loop` 本身不改一行**；如发现 `run` 没把 sink
传到 kernel，那是单独 bug，停下报告，不要在本 goal 里扩大范围。

### 4. E2E — `e2e/tests/10-loop-mode.yaml` 加 session 断言

setup 里给 loop run 加 `unset RECURSIVE_SESSIONS_DIR` +
`RECURSIVE_HOME=/tmp/rh-loop`（按 `CLAUE.md` 的 session 隔离规范），跑完
后 `find /tmp/rh-loop -name transcript.jsonl` 定位 session 目录，拷到
`/tmp/sessions-loop`，加 `recursive-session:` 断言：

- `transcript.jsonl` exists
- `minMessages` ≥ loop 实际产生的消息数（fixture 决定，给保守下界）
- `roles` 含 `assistant`
- `status` ∈ `["completed"]`（fixture 让 agent 不调 schedule_wakeup →
  单 turn `NoMoreToolCalls` → Completed）

teardown 清理 `/tmp/rh-loop /tmp/sessions-loop`。**不要**改 loop 的
`--workspace` 与 `loop-result.txt` 既有断言。

### 5. Tests

- `main.rs` 或 `cli/output.rs` 单测：`finalize_session_writer` 对 loop
  产出的 writer 正确按 finish_reason 映射 status（若映射逻辑抽成纯函数
  `finish_to_session_status(FinishReason) -> SessionStatus`，单测它即可，
  避免起进程）。
- 既有 `run_loop` runtime 单测保持 green。
- E2E `10-loop-mode` 加 session 断言后 green。

## Acceptance

- `cargo fmt --all` clean。
- `cargo clippy --all-targets --all-features -- -D warnings` clean。
- `cargo test --workspace` green。
- `recursive loop`（默认，无 `--no-session`）跑完后，
  `~/.recursive/sessions/<id>/transcript.jsonl` **存在**且含每个 turn 的
  assistant/user 消息；`.meta.json` `status` 反映最后 turn 的
  finish_reason。
- `recursive loop --no-session` 不产生 session 目录（行为不变）。
- `recursive resume` 能恢复一个 loop 跑出的 session（手动验证）。
- `.dev/scripts/e2e-run.sh loop-mode` green，且新 session 断言通过（0.14.2）。
- `recursive loop` 仍打印 `Loop completed: N turn(s)`（或 `--json`
  summary），stdout 行为不变。

## Out of scope (defer)

- **loop 的 per-turn stdout 流式输出**（stream-json per turn）：loop 仍
  只在结束时出 summary。若要让 loop 也逐 turn 流式，是单独目标，需设计
  「每 turn 一个 stream 段 + 一个 result」的输出契约（与 Goal 326 的
  Client 模式语义呼应）。本 goal 只加落盘。
- TUI `/loop` slash command —— Goal 323（P1 已含 SessionWriter 坑注释）。
  本 goal 把 loop 编排逻辑留在 runtime 层正是为它铺路，但不实现 TUI 侧。
- `run_event_loop`（后台任务触发）的 session 持久化 —— 结构类似，单独
  目标。
- 给 `.meta.json` 加 `turn_count` 字段为非必须，不做除非顺手。
- HTTP API 的 loop 触发。

## Notes for the agent

- 先读：
  - `crates/recursive-cli/src/main.rs::run_loop`（1741-1789，当前无 sink）
  - `crates/recursive-cli/src/main.rs::run_once`（1809-1870，要复制的
    装配模式）与 `run_once` 调用点（`main.rs:793-805`，看 `session`/
    `name` 怎么传进来）
  - `crates/recursive-cli/src/cli/output.rs::finalize_session_writer`
    （127-149）
  - `src/runtime.rs::run_loop`（935-958）确认 `run` 透传 event_sink
  - `e2e/tests/10-loop-mode.yaml`、`e2e/tests/00-smoke.yaml`（session
    隔离规范模板）
- **开工前跑** `gitnexus_impact({target: "run_loop", direction: "upstream"})`
  与 `gitnexus_impact({target: "SessionPersistenceSink"})`，确认 blast
  radius。HIGH/CRITICAL 先停下报告。
- **关键坑**：event_sink **只挂 `SessionPersistenceSink`，不挂
  `ChannelSink`**——loop 没有 drain channel 的 printer，挂了会死锁/撑爆。
- **关键坑**：`--no-session`（`RECURSIVE_NO_SESSION=1`）必须尊重，否则
  现有 headless/CI 用法会突然开始写盘。
- E2E 必须按 `CLAUDE.md` 的 session 路径隔离规范：`unset
  RECURSIVE_SESSIONS_DIR` + 独立 `RECURSIVE_HOME`，否则 session 落到
  `/workspace/sessions` 而 `find` 找不到（既有 `12/13/15` 套件已踩过此坑）。
- 触碰 `crates/recursive-tui/src/` 才需过 TUI gates；本 goal **不动 TUI**，
  无需 `tui-test-presence.sh` / `tui-mutants.sh`。
- `recursive loop` 的 `--json` summary 结构保持不变（数组 of
  `{finish, steps}`）。
- 提交时不要加 Co-authored-by。
- 改完写 `.dev/journal/manual-<date>-goal327-loop-session.md`。
