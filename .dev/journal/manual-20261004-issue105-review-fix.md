# Manual journal — 2026-10-04 — issue #105 评审阻断项修复

## Date
2026-10-04

## Goal
修复 run `pipeline-105-1004102032` 第二轮评审（`review-failure.log`）列出的两条阻断项，
让保留的半成品（`wip-pipeline-105-1003234724`）下一轮能过评审。
评审其余 8 条为 non-blocking，未在本轮处理。

## Files touched
- `crates/recursive-cli/src/main.rs`
  - `run_weixin_headless_daemon`：改为「是否切换过」+ 绑定值两个变量
    （`switched: bool` + `active_session: Option<String>`），条件式改为
    `!switched || req.session_id != active_session`；切换成功时
    `active_session = Some(bound); switched = true`，切换失败时
    `switched = false`——下一条消息重试，而不是把上一个用户的会话当成已就绪。
    （初稿用三态 `Option<Option<String>>` 表达同一语义，落地时改成两个变量以避免
    `clippy::option_option`；本段最初按初稿写，rev3 评审指出后已照实际代码更正。）
  - `switch_weixin_session`：`SessionWriter::open_existing` 失败路径由「保留旧 sink」
    改为 `replace_event_sink(Arc::new(recursive::NullSink))`，兑现注释里
    「degrade to in-memory」的承诺（旧 sink 留着会把本轮对话追加到上一个会话的
    `transcript.jsonl`）。
  - `start_fresh_weixin_session`：在 `create_bound_session` 之前先 detach 旧 sink，
    创建失败提前返回时不再遗留上一个会话的 `SessionPersistenceSink`。

## Tests added
无。三个函数都在 bin crate（`crates/recursive-cli/src/main.rs`）内且处于
`#[cfg(feature = "weixin")]` 之下，集成测试无法 import，单测需要先把这段逻辑
搬到可测模块——本轮按评审给出的最小修法处理，未做搬迁。

## Notes / traps hit
- 评审指出的根因是 `None == None`：`active_session` 初值 `None` 与未绑定用户的
  `req.session_id == None` 相等，`switch_weixin_session`（除 `/c` 外唯一调用
  `create_bound_session` 的地方）因此从不执行——首次对话既不绑定也不落盘。
- 拆出「是否切换过」后必须同时决定「切换失败缓存什么」：把失败也记成 `switched =
  true` 会让创建失败变成永久不再重试，因此失败路径把 `switched` 置回 `false`，
  保留重试语义。
- 记录偏差：本 journal 初版按设计初稿描述实现（三态 `Option<Option<String>>` +
  `.map(Some)`），与提交后的实际写法不符；issue #105 rev3 评审把这条列为 stale
  doc，现已按代码更正。
