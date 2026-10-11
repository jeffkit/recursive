# manual-20261012-issue168-empty-assistant

- **Date**: 2026-10-12
- **Goal**: #168 — 中断落在 tool_call 增量阶段时落盘空 assistant 消息，
  污染 transcript，之后每一轮 Anthropic 请求都 400。
- **Baseline HEAD**: 6c724056（`depends-on: #157` 已在其上，`e7ff337a`）
- **Worktree**: `fix/168-empty-assistant-guard`（本地提交，未 push）

## 根因

`Goal 382` 的“中断也有产出”部分落盘机制里，半成品判据把**只有 tool_call 增量、
没有任何文本**也算 `have_partial`：

- `src/llm/openai.rs`（原 851-866）：`|| !tool_call_builders.is_empty()`
- `src/llm/anthropic.rs`（原 555-569）：`|| !acc.tool_calls.is_empty()`

随后构造 partial `Completion` 时又**清空 tool_calls**（`HashMap::new()` /
`acc.tool_calls.clear()`，防孤儿），`finish_reason="interrupted"`。于是
`run_core::finish_interrupted` push 出 `Message::assistant("")`：内容空、无
tool_calls。该消息经 `SessionPersistenceSink`（`MessageAppended` 事件）逐条落盘
进 `transcript.jsonl`。

序列化侧当时没有任何兜底：Anthropic 适配器会发
`{"role":"assistant","content":""}`，而空文本块是硬 400
（`text content blocks must be non-empty`）；`filter_leading_assistant` 只裁**开头**
的 assistant；#16 的空 content 守卫只在 `tool_calls` 非空时把 content 换成
`null`——恰恰不覆盖“空且无 tool_calls”这一形态。消息一旦落盘就永久留在
transcript 里，用户后续每一轮都失败，且无法重开会话。

## 改动（双层：生产端收紧 + 序列化/加载兜底）

1. **收紧半成品判据**（不再产出空 partial）
   - `src/llm/openai.rs`：新增 `OpenAiProvider::has_persistable_partial(content,
     reasoning_content)`，判据只剩“有文本或有推理”；两处中断分支（cancel / 读错误）
     改调它，tool_call 增量不再算 partial。纯 tool_call 中断回到
     `Err(Error::Cancelled)` / `Err(Error::Llm)`——“turn 失败但不污染 transcript”。
   - `src/llm/anthropic.rs`：对称的 `AnthropicProvider::has_persistable_partial(acc)`。
   - 两者都标 `#[cfg_attr(test, mutants::skip)]`：`reasoning_content` 那一项在
     “读到一半就返回”的语义下**行为等价于 true**（reasoning 非空说明此前一定
     已经 accumulate 过，而那时早就返回了 partial），删/翻它不产生可观测差异。
     注释里写明了这条推理。

2. **共享判据**：`src/message.rs` 新增公开函数
   ```rust
   pub fn is_empty_assistant(m: &Message) -> bool {
       m.role == Role::Assistant && m.content.trim().is_empty() && m.tool_calls.is_empty()
   }
   ```
   四个守卫点共用同一份定义，避免各自漂移。刻意**不含** `tool_calls` 非空的
   assistant——删它会留下孤儿 `Role::Tool`，破不变量 #8。

3. **`finish_interrupted` 第二层**（`src/run_core.rs`）：push 前判
   `is_empty_assistant(msg)`，命中则 `tracing::warn!` 并直接
   `make_cancelled_outcome(step, None, …)`——turn 仍然是 `Cancelled`（不变量 #7，
   调用方照常落盘/退出码），只是不 push 那一条空行。用来兜住“别的 provider /
   未来的 partial 路径 / 内存里重放的 transcript”。

4. **序列化兜底（丢弃，不报错）**
   - `src/llm/openai.rs::build_request` — 组 messages 前过滤掉空 assistant。
   - `src/llm/anthropic.rs::serialize_messages_anthropic` — 同款过滤。
   - 都是“已经落盘的历史坏消息必须能读出去”的最后一层；两侧都做，因为 OpenAI 侧
     `content:""` 自身通常被接受、严格转译网关才会拒。

5. **历史坏会话出口**（`src/session/reader.rs`）：`load_messages` 与
   `load_messages_with_orphan_results` 两条加载路径都过滤该形态，用户能重新打开
   被污染的会话。

**未改**：on-disk transcript 格式（仍写 jsonl；只是不再写空行）；
Goal 382 的部分落盘机制本身（“中断也有产出”的意图保留，仍有文本的中断照常落盘）；
reasoning-only 中断的处理（原样保留，见 `parse_sse_stream_returns_partial_when_only_reasoning_accumulated`）。

## 测试

新增/改动（红→绿顺序见下）：

- `src/llm/openai.rs`
  - `parse_sse_stream_errors_when_only_tool_call_started`（原
    `parse_sse_stream_returns_partial_when_only_tool_call_started` 改写）——tool_call
    增量后断流必须 `Err`，且错误文本含 `SSE stream read error`。
  - `serializer_skips_empty_assistant`（规格用例）——空 assistant 不进 OpenAI 请求体。
  - `serializer_keeps_interrupted_assistant_with_text`——反向对照：有文本的中断保留；
    `tool_calls` + 空 content 仍按 #16 发 `content: null`。
- `src/llm/anthropic.rs`
  - `parse_sse_stream_errors_when_only_tool_call_started`（同上改写）。
  - `serializer_skips_empty_assistant`（规格用例）——`build_request` 出的
    `messages` 里既没有空 assistant，也没有空文本块。
  - `serializer_keeps_interrupted_assistant_with_text`——反向对照（含 tool_use 保留）。
- `src/run_core.rs`
  - `interrupted_tool_call_delta_persists_no_empty_assistant`（规格用例）——
    `finish_reason="interrupted"` + 空 content + 空 tool_calls 的 Completion 过
    `run_inner` 后，`outcome.messages` 里不存在“空 content 且无 tool_calls”的
    assistant，且 finish_reason 仍是 `Cancelled`。
  - `interrupted_assistant_with_text_is_still_pushed`——有文本的中断照常 push。
  - `is_empty_assistant_matches_only_empty_assistant_messages`——判据边界
    （空白算空；user/system/tool 不算；带 tool_calls 的 assistant 不算，#8）。
- `src/session/reader.rs`
  - `legacy_empty_assistant_is_filtered_on_load`（规格用例）——磁盘上真实存在该空行
    （测试先断言 `load_transcript` 里有 4 条），`load_messages` 后为 3 条，且
    `tool_calls` 的 assistant 与 `Role::Tool` 结果都还在。
- 回归：`compaction_keeps_tool_calls_paired_with_results`（不变量 #8）随
  `cargo test --test invariants` 通过。

## 验证

- 红：6 个新/改用例在未打补丁的 HEAD 上失败，逐条输出见 commit body / PR 描述。
- 负向对照：逐个 hunk 回退（保留测试）→ 对应用例失败 → 还原 → 与回退前快照
  sha256 逐字节比对一致。覆盖 `openai` 两个 call site（cancel / read-error，
  分开验证）、`anthropic` 判据、`finish_interrupted` 守卫、两个序列化器、
  加载路径三个过滤点。唯一无法单测证明的是 openai **cancel 分支**的
  tool_call-only 路径（没有驱动 mid-cancel 的既有测试），已用与 read-error 分支
  对称的代码审查覆盖。
- 门禁：`cargo test -p recursive-agent --lib -- llm`（282 passed）、`-- run_core`
  （81）、`-- session::`（194）、`--lib`（3307 passed）、`--test invariants`
  （55 passed，含 #8 配对）、`--test integration`（20）、
  `--test deferred_tool_loading`（3）、
  `cargo clippy -p recursive-agent --lib -- -D warnings`（clean）、
  `cargo fmt --all -- --check`（clean）。

## Spec 漂移与说明

1. 规格说“#16 的空 content 守卫（`openai.rs:1240`）”——行号已漂到 1249 附近，
   行为描述正确（仅 `tool_calls` 非空时生效）。
2. 规格建议“历史坏消息过滤与 #161 的 helper 放同一处更自然”——本仓 #161 是
   `PermissionHook`（`src/tools/registry.rs`），不是会话加载 helper，该指引在
   本基线不成立。判据放在 `src/message.rs`（`Message` 自身的性质，四个守卫点
   共用），加载侧在 `SessionReader`。
3. 规格的 `legacy_empty_assistant_is_filtered_on_load` 还要求“后续请求可正常发出”。
   本用例用加载结果直接过序列化器间接覆盖（同一 `is_empty_assistant` 判据），
   没有起 HTTP 服务器——语料里“mock 流先发 tool_call 增量再断流”的端到端形态
   同样没有起服务器，而是用 `MockProvider` 直接注入 buggy 形态的 `Completion`
   （provider 行为另有 `parse_sse_stream_errors_when_only_tool_call_started` 真
   HTTP 测试）。
4. 规格未要求，但实测**必须**放弃的一处：把 `filter_leading_assistant` 也收窄成
   `is_empty_assistant` 会让 `filter_leading_assistant_removes_empty_assistant`
   与 `..._removes_multiple_empty_assistants` 失败——那两个既有测试钉的是“开头的
   **非空** assistant 也要裁掉”的独立行为。故保留其原判据（它对空形态同样生效），
   真正的 #168 守卫放在 `serialize_messages_anthropic` 里（能覆盖中段，且是
   leading-only 那趟够不到的位置）。

## Notes / 残余不确定性

- **另一条同类通道（未改，需决策）**：`run_core::handle_no_tool_calls`
  （`src/run_core.rs:835-853`，push 在 845-847）在“LLM 正常返回、`finish_reason` 为
  `stop`/`end_turn`、但 content 为空且无 tool_calls”时同样 push
  `Message::assistant("")` 并落盘。
  这不是中断路径，规格未覆盖，且修它要顺带决定“空回复该记成什么 finish_reason”
  （`NoMoreToolCalls` 还是新语义），属于超范围，故未动。实际风险有限：两侧序列化器
  都会丢弃它，只有 Anthropic 客户端自己消化空文本块（本仓序列化器已不发空块）。
  若认为要一并堵住，第二层守卫是现成的：同一个 `is_empty_assistant`。
- **未加 writer 层守卫**：`SessionPersistenceSink` / `SessionWriter::append_with_audit`
  不判空，写的是事件流里的任何 `MessageAppended`。加在那里最“终极”，但会让
  `SessionWriter`（测试/工具也直接用）不能如实写出调用方给的内容，改动面更大；
  本次按规格只做“生产端 + 序列化 + 加载”三层。
- **`#[mutants::skip]` 的必要性**：`has_persistable_partial` 的 `reasoning_content`
  项在“返回即退出”的语义下不可达可观测差异；别的变异（判据整体替换、常量 true/false）
  本可被新用例杀掉，故 skip 只覆盖这一项不可判定的等价性，不掩盖行为变异。
  该仓 `mutants` 是 dev-dependency 且 `#[cfg_attr(test, …)]`，生产二进制零影响。
- 真实 Anthropic 端点未打（无凭据）；结论建立在“空文本块被拒”这一已文档化约束与
  本仓序列化路径上，mock 断言的是过去会 400 的那个确切 wire shape。
- 全程未 push、未建 PR/gist、未评论 issue；仅本地提交。
