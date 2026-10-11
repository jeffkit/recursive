# manual-20261012-issue168-empty-assistant

- **Date**: 2026-10-12
- **Goal**: #168 — 中断落在 tool_call 增量阶段时落盘空 assistant 消息，
  污染 transcript，之后每一轮 Anthropic 请求都 400。
- **Baseline HEAD**: 6c724056（`depends-on: #157` 已在其上，`e7ff337a`）
- **Worktree**: `fix/168-empty-assistant-guard`
- **Commits**: `1fc0bfbb`（主体）+ 本文件所在的 review 返工 commit
  （splice 越界修复 + reasoning-only 取舍 + 不变量 #4 补测）

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

2. **共享判据（两个，职责不同）**：`src/message.rs`
   ```rust
   // 序列化 / 加载过滤用：只认“毒消息”这一形态
   pub fn is_empty_assistant(m: &Message) -> bool {
       m.role == Role::Assistant && m.content.trim().is_empty() && m.tool_calls.is_empty()
   }
   // finish_interrupted 用：中断要不要落盘（reasoning-only 也算产出）
   pub fn has_persistable_interrupt(m: &Message) -> bool {
       m.role == Role::Assistant
           && (!m.content.trim().is_empty()
               || m.reasoning_content.as_deref().is_some_and(|r| !r.trim().is_empty())
               || !m.tool_calls.is_empty())
   }
   ```
   两者刻意不同，理由见下节“reasoning-only 取舍”。都**不含**“带 tool_calls 的
   assistant”——删它会留下孤儿 `Role::Tool`，破不变量 #8。

3. **`finish_interrupted` 第二层**（`src/run_core.rs`）：push 前判
   `!has_persistable_interrupt(msg)`，命中则 `tracing::warn!` 并直接
   `make_cancelled_outcome(step, None, …)`——turn 仍然是 `Cancelled`（不变量 #7），
   只是不 push 那一条“什么都没有”的行。

4. **序列化兜底（丢弃 + 记日志）**
   - `src/llm/openai.rs::build_request` — 经 `is_droppable_empty_assistant`
     过滤并 `tracing::warn!(provider="openai", …)`。
   - `src/llm/anthropic.rs::serialize_messages_anthropic` — 同款过滤 +
     `tracing::warn!(provider="anthropic", …)`。
   - 规格原文建议“丢弃而不是报错，并 `tracing::warn!` 记一次”，本次补上：请求被
     静默改写会无从诊断，日志里必须可见。

5. **历史坏会话出口**（`src/session/reader.rs`）：`load_messages` 与
   `load_messages_with_orphan_results` 两条加载路径都过滤该形态，用户能重新打开
   被污染的会话。

**未改**：on-disk transcript 格式（仍写 jsonl；只是不再写空行）；
Goal 382 的部分落盘机制本身（“中断也有产出”的意图保留，仍有文本/推理的中断照常落盘）；
reasoning-only 中断的处理（见下）。

## reasoning-only 取舍：**(A) 按规格保留**（review 返工修正）

规格「不做」原文：*“不改 reasoning-only 中断的处理（那是另一种形态，若要一起收紧，
需在 PR 里单独说明并加断言）”*。

**第一版实现违反了这一条**：`finish_interrupted` 用的是 `is_empty_assistant`，
而它只看 content + tool_calls，于是 reasoning-only 中断
（`content=""`, `reasoning_content=Some`）被连带丢弃 —— 实测 transcript 从
`[(User,"hi"), (Assistant,"", Some("thinking hard"))]` 变成 `[(User,"hi")]`。
更糟的是 journal 声称“原样保留”，与实现不符（交付缺陷，本次已改正）。

**取舍：选 (A)，保留 reasoning-only 中断。** 理由：

1. 它是**真实模型产出**（DeepSeek thinking / Anthropic extended thinking 都先发
   thinking 增量再发文本），丢弃等于静默吞掉这一 turn 的工作；
2. 保留它**不毒化请求**：Anthropic 序列化器对“无文本、无 tool_calls”的消息本就
   只产出 `{"role":"assistant","content":[]}`（无空文本块），OpenAI 侧则按 #16
   用 `reasoning_content` + `content: null` 表达；
3. 规格明确要求不夹带这种行为变更。

实现方式：`finish_interrupted` 改用 `has_persistable_interrupt`（content ∨
reasoning ∨ tool_calls）。序列化/加载过滤**不变**（仍是 `is_empty_assistant`），
即：reasoning-only 消息会被落盘，仍会在序列化时被丢弃（对 Anthropic 而言就是
`content: []`），两者语义一致且都不产生空文本块。

钉住该行为的断言：`run_core::tests::interrupted_reasoning_only_assistant_is_still_pushed`
（content 为空、reasoning 为 `Some("thinking hard")` → assistant 行仍在、reasoning 仍在）。

## review 返工：`load_messages_with_orphan_results` splice 越界（本人引入）

**根因**：过滤历史空行时，插入点仍按**未过滤**的 `TranscriptEntry` 列表算
（`orphan_result_insert_at(&entries)`），却往**已过滤**的 `messages` 上
`messages.splice(insert_at..insert_at, …)`。真实入口是
`crates/recursive-cli/src/cli/resume.rs` 的 `resume --orphans=skip|redo`
（非空 answers）。复现输出：

```
panicked at core/src/slice/index.rs:1009:51:
range end index 3 out of range for slice of length 2
```

即使索引碰巧不越界，位置也是错的：过滤让锚点前的行数变少，用原索引插入会把合成
tool_result 落到**更靠后的消息之后**，不变量 #8 被静默破坏（下面的第三个用例正是
这种“不 panic 但顺序错”的形态）。

**修复**：不在 reader 里自己算索引，直接复用 `src/session/orphan.rs` 已有的
`pub fn splice_orphan_results(messages, answers)` —— 它在**过滤后的 message 序列**
上调用 `orphan_result_insert_at_messages` 并在同一序列上 splice，索引与目标序列
天然一致；`reader.rs` 里重复的 entry 版 `orphan_result_insert_at`（此时已无生产
调用）连同其单测一并删除，索引算术由 `orphan.rs` 的
`orphan_insert_index_anchors_on_the_last_assistant_with_calls` 继续钉住：

```rust
let messages = entries.into_iter().map(entry_to_message)
    .filter(|m| !crate::message::is_empty_assistant(m)).collect();
Ok(super::orphan::splice_orphan_results(messages, answers))
```

**第一版测试为什么没抓到**（同类“断言不在真实路径上生效”问题）：
原用例调的是 `load_messages_with_orphan_results(&dir, &[])`，走 `answers.is_empty()`
的提前 return，永远到不了 splice。返工后：

- `legacy_empty_assistant_is_filtered_on_load` 改为用**非空 answers** 再调一次；
- 新增 `legacy_empty_assistant_plus_orphan_tool_call_splices_at_the_filtered_index`
  —— 复现验收者 falsifier 的精确形态（空 assistant 行 + 末尾孤儿 tool call +
  非空 answers）；
- 新增 `legacy_empty_assistant_plus_orphan_answer_lands_before_later_turns`
  —— 索引“碰巧不越界”的形态（`user, <空>, assistant(tool), tool, user`），断言
  合成结果落在**已有结果批之后、后续 user turn 之前**（index 3），把位置正确性
  钉死，而不只是“不 panic”。
- 三例都用 `assert_pairing` 校验 #8（每个 tool 结果都有配对的 assistant call、
  每个 call 都有结果）。

## 测试

- `src/message.rs`（不变量 #4：新增 pub fn 在同文件有单测）
  - `is_empty_assistant_matches_empty_and_whitespace_only`、
    `..._rejects_non_empty_content`、`..._never_matches_a_tool_call_carrier`、
    `..._rejects_every_other_role`、`..._ignores_reasoning`
  - `has_persistable_interrupt_keeps_text_and_reasoning_only`、
    `..._drops_only_nothing`（空/纯空白 reasoning 不算）、
    `..._rejects_non_assistant_roles`
- `src/llm/openai.rs`
  - `parse_sse_stream_errors_when_only_tool_call_started`（改写）——tool_call 增量后
    断流必须 `Err`，错误文本含 `SSE stream read error`。
  - `serializer_skips_empty_assistant`（规格用例，`#[tracing_test::traced_test]`）
    ——空 assistant 不进请求体，且日志出现 drop 警告。
  - `serializer_keeps_interrupted_assistant_with_text`——反向对照：有文本的中断保留；
    `tool_calls` + 空 content 仍按 #16 发 `content: null`。
- `src/llm/anthropic.rs`
  - `parse_sse_stream_errors_when_only_tool_call_started`（同上改写）。
  - `serializer_skips_empty_assistant`（规格用例 + 日志断言）——无空 assistant、
    无空文本块。
  - `serializer_keeps_interrupted_assistant_with_text`——反向对照（含 tool_use 保留）。
- `src/run_core.rs`
  - `interrupted_tool_call_delta_persists_no_empty_assistant`（规格用例）——
    `interrupted` + 空 content + 空 tool_calls 过 `run_inner` 后无空 assistant 行，
    finish_reason 仍 `Cancelled`。
  - `interrupted_assistant_with_text_is_still_pushed`——有文本照常 push。
  - `interrupted_reasoning_only_assistant_is_still_pushed`——reasoning-only 照常 push
    （见上节取舍）。
- `src/session/reader.rs`
  - `legacy_empty_assistant_is_filtered_on_load`（规格用例，含非空 answers 分支）
  - `legacy_empty_assistant_plus_orphan_tool_call_splices_at_the_filtered_index`
  - `legacy_empty_assistant_plus_orphan_answer_lands_before_later_turns`
- 回归：`compaction_keeps_tool_calls_paired_with_results`（不变量 #8）随
  `cargo test --test invariants` 通过。

## 验证

**红（第一版）**：6 个规格用例在未打补丁的 HEAD 上失败（空 assistant 进请求体 /
tool-call-only 中断返回空 partial / transcript 里出现空 assistant 行）。

**红（返工）**：splice 修复前，两个新用例
`panicked … range end index 3 out of range for slice of length 2` /
`… index 4 … length 3`。

**负向对照**（逐 hunk 回退 → 对应用例转红 → 还原 → sha256 逐字节一致）：

| hunk | 位置 | 回退后结果 |
|---|---|---|
| A1a | openai **cancel** 分支判据 | **无测试转红**（见残余不确定性） |
| A1b | openai read-error 分支判据 | FAILED `panicked src/llm/openai.rs:3176` |
| A2 | anthropic 判据 | FAILED `panicked src/llm/anthropic.rs:3408` |
| B | `finish_interrupted` 守卫 | FAILED `panicked src/run_core.rs:3421` |
| C1 | openai 序列化过滤 | FAILED `panicked src/llm/openai.rs:3199` |
| C2 | anthropic 序列化过滤 | FAILED `panicked src/llm/anthropic.rs:3427` |
| D | 加载路径过滤（两处） | FAILED（3 个 reader 用例） |
| E | splice：改回用**未过滤** entries 算索引 | FAILED：2 个 panic（`range end index 3 … length 2` / `index 4 … length 3`）+ 位置断言 FAILED（`left: User / right: Tool`，合成 `Tool("repaired")` 落到 `User("next question")` 之后） |
| F | reasoning 豁免（改回只看 content+tool_calls） | FAILED `panicked src/run_core.rs:3514`，transcript 只剩 `[(User,"hi")]` |

**门禁**（返工后重跑）：`--lib llm`、`-- run_core`、`-- session::`、全量 `--lib`、
`--test invariants`、`--test integration`、`--test deferred_tool_loading`、
`cargo clippy -p recursive-agent --all-targets -- -D warnings`、
`cargo fmt --all -- --check` 全绿（见对应 commit body）。

## Spec 漂移与说明

1. 规格说“#16 的空 content 守卫（`openai.rs:1240`）”——行号已漂到 1249 附近，
   行为描述正确（仅 `tool_calls` 非空时生效）。
2. 规格建议“历史坏消息过滤与 #161 的 helper 放同一处更自然”——本仓 #161 是
   `PermissionHook`（`src/tools/registry.rs`），不是会话加载 helper，该指引在
   本基线不成立。判据放在 `src/message.rs`，加载侧在 `SessionReader`。
3. 规格的 `legacy_empty_assistant_is_filtered_on_load` 还要求“后续请求可正常发出”。
   用例用加载结果直接过序列化器间接覆盖（同一 `is_empty_assistant` 判据），
   没有起 HTTP 服务器——“mock 流先发 tool_call 增量再断流”的端到端形态也没有起
   服务器，而是用 `MockProvider` 直接注入 buggy 形态的 `Completion`（provider 行为
   另有 `parse_sse_stream_errors_when_only_tool_call_started` 真 HTTP 测试）。
4. 规格未要求，但**实测必须放弃**的一处：把 `filter_leading_assistant` 收窄成
   `is_empty_assistant` 会让 `filter_leading_assistant_removes_empty_assistant`
   与 `..._removes_multiple_empty_assistants` 失败——那两个既有测试钉的是“开头的
   **非空** assistant 也要裁掉”的独立行为。故保留其原判据（它对空形态同样生效），
   真正的 #168 守卫放在 `serialize_messages_anthropic` 里（能覆盖中段，且是
   leading-only 那趟够不到的位置）。
5. 规格说“`finish_interrupted` 与两个序列化器都跳过空 content 且无 tool_calls 的
   assistant”——`finish_interrupted` 处收紧为“content ∨ reasoning ∨ tool_calls
   都没有才跳过”，这是规格「不做」条款的直接后果（见上文取舍），不是偏离。

## Notes / 残余不确定性

- **A1a 无对应测试**：openai **cancel 分支**（`CancellationToken` 在 tool_call 增量
  之后触发）没有既有测试驱动，回退该 hunk 不会让任何用例转红。它与 read-error 分支
  是同一谓词的对称调用，只做了代码审查级覆盖；若要把这个洞堵上，需要一个在 SSE
  增量之间取消 token 的测试（现有用例都不覆盖该时序）。
- **另一条同类通道（未改，需决策）**：`run_core::handle_no_tool_calls`
  （`src/run_core.rs:835-853`，push 在 845-847）在“LLM 正常返回、`finish_reason` 为
  `stop`/`end_turn`、但 content 为空且无 tool_calls”时同样 push
  `Message::assistant("")` 并落盘。这不是中断路径，规格未覆盖，且修它要顺带决定
  “空回复该记成什么 finish_reason”（`NoMoreToolCalls` 还是新语义），属于超范围，
  故未动。实际风险有限：两侧序列化器都会丢弃它。若要一并堵住，第二层守卫是现成的
  （`has_persistable_interrupt` / `is_empty_assistant`）。
- **未加 writer 层守卫**：`SessionPersistenceSink` / `SessionWriter::append_with_audit`
  不判空，写的是事件流里的任何 `MessageAppended`。加在那里最“终极”，但会让
  `SessionWriter`（测试/工具也直接用）不能如实写出调用方给的内容，改动面更大；
  本次按规格只做“生产端 + 序列化 + 加载”三层。
- **`#[mutants::skip]` 的必要性**：`has_persistable_partial` 的 `reasoning_content`
  项在“返回即退出”的语义下不可达可观测差异；别的变异（判据整体替换、常量 true/false）
  本可被新用例杀掉，故 skip 只覆盖这一项不可判定的等价性，不掩盖行为变异。
  `is_empty_assistant` / `has_persistable_interrupt` 同样按此标准标注，且各自都有
  同文件单测。该仓 `mutants` 是 dev-dependency 且 `#[cfg_attr(test, …)]`，生产
  二进制零影响。
- 真实 Anthropic 端点未打（无凭据）；结论建立在“空文本块被拒”这一已文档化约束与
  本仓序列化路径上，mock 断言的是过去会 400 的那个确切 wire shape。
- 全程未 push、未建 PR/gist、未评论 issue；仅本地提交。
