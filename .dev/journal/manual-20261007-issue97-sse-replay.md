# Manual journal — issue #97 SSE 可重放：Last-Event-ID / `?since=` + lagged 不再静默丢帧

- **Date**: 2026-10-07
- **Goal**: 场景 gap 单 #97（P1）——`GET /sessions/:id/events` 原本是纯 live：
  慢消费者触发 `broadcast` 的 `Lagged` 时 `Err(_) => None` 直接丢帧；1h 硬超时
  或弱网重连又从"订阅时刻"重新开始。断连窗口内的 PartialMessage/ToolCall/
  ToolResult/Done 全部永久丢失，客户端流式状态机无法恢复。
- **Baseline**: worktree HEAD（本工作树基于含 #134 的 main；见下方 Notes 的两处
  基线编译修复）。

## What landed

### `src/http/event_log.rs`（新模块）—— 每会话有界重放环

- `SessionEventLog`：`Mutex<VecDeque<StoredFrame>>`，`SESSION_EVENT_LOG_CAPACITY = 512`。
  每帧在 push 时分配一个**单调递增的内部 position**（与线上 `id:` 解耦）——
  position 连续，所以"环里缺没缺帧"可以精确判定，而不必猜测 `seq` 是否连续
  （无 SSE 映射的事件同样消耗 `seq`，靠 `seq` 差推断必然误报）。
- `push(frame)` 满则 `pop_front`；`replay(cursor)` 把客户端游标解析成重放集合
  （`None` = 从现在开始，新订阅者**不**被灌历史，保持既有语义）；`drain_from(next)`
  给 live pump 用。游标解析：先按 `id` 精确匹配，失败再按 id 尾部的序列号匹配
  （让 `?since=7` 这种裸序号也成立，且落到该事件组最后一帧之后）。
- `SessionReplay.next_pos` 是**含下界**（"下一条该发的帧的 position"），不是写指针：
  pump 用它取 `frames ≥ next`。这一点必须对——用"写指针 + `>`"会在"订阅后新推入
  的第一帧"上差一位（漏帧或重发），`frames_published_after_connect_arrive_exactly_once`
  就是钉它的回归测试。
- 游标所指帧已被淘汰时：返回环内全部帧 + `truncated = true`（语义上"你落到窗外了"）。
  live 侧的 gap 判定同样精确：`oldest_retained > next`（position 连续且环只留后缀）。

### `src/http/handlers.rs` —— SSE 端点与 forwarder

- `send_session_message` 的 forwarder 改成**先 `log.push(frame)` 再 `broadcast_tx.send(frame)`**，
  保证被 channel 唤醒的订阅者一定能在环里找到该帧。
- `session_events`：
  - 新增 `?since=`（`SessionEventsQuery`）与 `Last-Event-ID` 头（`?since=` 优先）；
  - **先 `subscribe()` 再快照环**，订阅后到达的帧进 `rx`，两者不会漏；
  - 初始重放（命中即发；`truncated` 则先发 `gap` 帧），随后进入 live pump；
  - pump 里**不再消费 broadcast 的 payload**：channel 只当作"有新帧"的唤醒信号，
    真正要发什么一律从环里 `drain_after(pos)` 取。于是 `Lagged` 不再是丢帧，
    而是"醒来后把环里落下的帧补发"，必要时补一个 `gap` 帧；
  - `gap` 帧不带 `id:`（它是本连接的一次性提示，不占会话时间线，不会推进客户端的
    `Last-Event-ID`）。
  - 1h 上限保留（连接仍然周期性重建），但重连现在用 `Last-Event-ID` 无损续传。
- `SseEvent::Gap { resume_id: Option<String> }`（`src/http/mod.rs`）：游标早于
  保留窗口时显式告知，客户端应走 `GET /sessions/:id` 对账——**不再静默**。
- `SessionState.event_log: Arc<SessionEventLog>`：环挂在会话上（跨轮次重放需要），
  随会话删除/驱逐一起释放，不新增全局清理路径。
- OpenAPI：`/sessions/{id}/events` 补 `since` 参数与可续传说明。

## Tests added

- `src/http/event_log.rs` 单测 7 条：容量淘汰、无游标=从当前开始、按 id 续传、
  裸序号续传、被淘汰游标→全量+gap、`drain_from` 只在真跳帧时标 gap、
  未知游标但从未淘汰→不误报 gap。
- `tests/issue97_sse_replay.rs`（新集成测试，7 条，走真实 router + SSE body）：
  `?since=` 重放、`Last-Event-ID` 重放、无游标不重放历史、空 header 视为无游标、
  游标越窗先发 `event: gap`（并带 `resume_id`）后再跟保留帧、订阅后 live 推帧
  恰好投递一次、真实一轮对话的帧确实进了重放环且能从首帧续传。

## Gates

```
cargo test --workspace            # 62 个 test target 全 ok（含新增 10 条）
cargo clippy --all-targets --all-features -- -D warnings   # clean
cargo fmt --all -- --check        # clean
```

未跑 Docker/host e2e（`08-http-api` 的 SSE 用例连接后才发消息，无游标路径行为
不变，仅投递路径改为"唤醒后从环取"）；e2e 由 flow 的 gate 负责。

## Notes

- **两处基线编译错误**（本工作树 HEAD 就已存在、与本 issue 无关，但不修无法编译
  任何 target；修复均为最小的一行/一参数）：
  1. `src/preset.rs`：`MINIMAL` 预设的 `ToolProfile` 缺 `run_code`（#134 加了字段，
     只补了 `standard`）→ 补 `run_code: false`（与 `standard` 同约定，`origin/fix/134-minimal-run-code`
     分支在修同一处）。
  2. `src/http/mod.rs`：测试 `test_state(...)` 调用少传 `session_mirror_root`（#121
     加的参数）→ 补 `None`。
- 设计取舍：线上 `id:` 仍保持 #117 的 `<ts_ms>-<turn>-<seq>`（及其 `:progress`
  后缀），**不改线格式**；断点续传的位次放在环内部的 position 上。好处是 gap
  判定精确、progress 帧有自己的位次不会与源帧撞车、且 #117 的 id 契约与测试不动。
- 内存：环上限 512 帧/会话（流式 turn 里 token delta 也是帧），覆盖"秒级~分钟级
  断连"这一主场景；更久只能拿到 `gap`——这正是它存在的意义（有界且可见）。
