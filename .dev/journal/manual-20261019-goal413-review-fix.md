# Goal 413 — 评审返工：e2e 真的压缩 + AG-UI 消费端不再重播被折叠历史

- **Date**: 2026-10-19
- **Goal**: 处理独立评审 NEEDS_FIX（goal 413 canonical rebuild 一轮）
- **Files touched**:
  - `src/compact/rebuild.rs`（docs + 新回归测试；cut 语义见下；round 3 起也是
    g157 声明处理的唯一共享位置 `rebuild_declared_seed`）
  - `src/http/agui.rs`（`load_thread_seed` 整文件交给 canonical rebuild；新测试）
  - `src/session/reader.rs`（`read_transcript` 共享解析；`load_messages*` 走
    `rebuild_declared_seed`；round 3 新增 `load_seed_for_existing_runtime`）
  - `crates/recursive-tui/src/{app/render.rs,backend.rs}`（仅
    `TranscriptEntry` 新字段的两行初始化 + resume 调用点修正）
  - `crates/recursive-cli/src/main.rs`（微信 session 切换调用点修正）
  - `e2e/tests/22-compaction.yaml` + `e2e/fixtures/22-compaction.json`（6 轮 + 重启冷加载断言）

## 评审两条的处置

### 1. e2e 用例不可能通过 → 重写场景（已实测）

评审指出的 blocker 成立：`keep_recent_n + 2 == 10` 消息的门槛让两轮（5 条消息）
永远不压缩，`marker_rows=1` 必然红。改成 6 轮：

- turn 6 开头 transcript = `[system, u1,a1 … u5,a5, u6]`（12 条），split=4，
  `[u1,a1,u2]` 可摘要 → intra-turn 压缩触发；
- fixture 增补 turn 3–6，并把「摘要调用」（`Conversation to summarize:`，
  aimock 首匹配优先，必须排第一）单独占一条；
- 断言改为**重启服务后**再 GET：强制走 cold load（writer → TranscriptEntry →
  `entry_to_message` → `rebuild_seed`），证明 marker bit 真的落盘并回读，
  而不是只在内存里。

本地用 npx aimock + debug 二进制按 yaml 的 shell 步骤实跑验证：
`turn1..6_role=assistant`、`marker_rows=1 message_rows=13`；两轮的旧场景
`marker_rows=0`（复现评审结论）。容器内用 `printf '%s'` 而不是 `echo` 传 JSON
（dash 的 echo 会解释 `\n`，jq 直接解析失败）。

### 2. `rebuild_seed` 以 boundary 为切点 → 改在「声明」处切，并给出反例

评审建议在 `rebuild_seed` 里无条件从 marker 行开始切片。实测该做法会**丢历史**：

- intra-turn 压缩作用在 kernel 的 copy-on-write 副本上，wrapper 只能
  *追加* summary（`AgentRuntime::mark_transcript_rewritten` 有明确说明），
  落盘快照形如 `[…history…, SUMMARY, tail]`；
- 无条件切 marker 会把 marker 之前（也就是 preserved tail 与更早历史）全部丢掉。
  本地实测：重启后 cold load 只剩 `[system, summary]`（13 → 2 条），
  即「压缩一次 = 下次 resume 丢一段对话」。

因此切点归「持久化声明」：g157 `compact_boundary` 行才是“它之前的行已被折叠”
的声明（`SessionReader::load_transcript` 一直这么切），`load_thread_seed` 现在
用同一条声明切片（新增 `is_compact_boundary_line`），再交给 canonical rebuild。
三个消费端由此得到同一 seed，AG-UI 不再「历史 + 其摘要」重播；`rebuild_seed`
本身不做 marker 切片，模块文档写清原因，并新增回归测试
`rebuild_does_not_slice_at_a_marker_that_trails_the_history` 钉住。

## 评审 minor

- `src/compact/rebuild.rs` 已进入 index（一轮 `-N`，二轮改为完整 `git add`）。
- 边界的 marker 回填由 canonical rebuild 负责（round 3 起为
  `rebuild_declared_seed`），合成 bit 时打 `tracing::warn!`
  （owner「任何降级必须可观测」）。

## Tests

- `src/compact/rebuild.rs`: `rebuild_does_not_slice_at_a_marker_that_trails_the_history`
  （替换掉上一轮基于无条件切片的用例），其余 framing 测试保留；
  round 3 新增 `rebuild_declared_seed_*` 三条。
- `src/http/agui.rs`: `load_thread_seed_cuts_at_the_persisted_boundary` —— 原始
  JSONL（含 pre-boundary 行 + `compact_boundary` 行 + 带 marker 的 summary 行）
  → seed == `[summary, turn2, answer2]`；round 3 新增评审 BLOCKER 探针
  `load_thread_seed_marks_a_legacy_unmarked_summary_after_the_boundary`
  （边界后首行是无 marker 的 System summary → seed ==
  `[summary, turn2, answer2]`，summary 被回填 marker）。
- `src/session/reader.rs`: round 3 新增
  `load_seed_for_existing_runtime_keeps_{the_runtime_prompt,a_compaction_summary_after_the_prompt}`。
- `tests/compact_contract.rs`: round 3 新增
  `test_compact_contract_rebuild_declared_boundary_backfills_a_legacy_summary`。
- e2e `22-compaction`：6 轮 + 重启后 `marker_rows=[1]`。

## 二轮评审（round 2）

### 1. 回归：prompt 下压着「无 marker 的历史 summary」被整段丢弃（已修）

可复现的落盘形态：本 commit 之前的 `session_mirror::serialize_transcript` 恒写
`is_compaction_summary: false`，且 mirror 不写 `compact_boundary` 行，于是
`load_transcript` 的 backfill（`saw_boundary` 门控）也不触发。此类文件形如
`[prompt, SUMMARY, tail…]`（`Compactor::apply` 就是把 summary 插在前缀之后）。
旧的 `rebuild_seed` 把 leading System run 整段丢弃 → summary 与 prompt 一起消失，
折叠历史在 resume 时无痕丢失（#158 类 + 违反「降级必须可观测」）。

修法（按评审建议，且不破坏 #158 契约测试）：`rebuild_seed` 里
`marked.is_none() && prefix_len >= 2` 时只 skip 第一行（唯一的重建 prompt：
`RuntimeBuilder::build` 只 prepend 一条，`apply` 紧跟其后插 summary），
把第二行视为 legacy summary 保留、补 `is_compaction_summary`、`boundary = Some(1)`,
并 `tracing::warn!`（合成 marker 是降级，必须可观测）。marker 存在时仍整段丢前缀
（`test_compact_contract_rebuild_158_boundary_after_prefix` 继续成立）。
新增测试：`rebuild_recovers_a_legacy_unmarked_summary_under_the_prompt`、
`rebuild_legacy_recovery_keeps_reinjected_system_rows_after_the_summary`、
`rebuild_with_the_marker_present_drops_the_whole_prompt_run`、契约测试
`test_compact_contract_rebuild_158_legacy_unmarked_summary_survives`。

### 2. e2e 描述的压缩模型差一轮（已修）

不再写「第 6 轮才压缩」：按 `split_for_compaction` 的算术陈述——第 4 轮末 9 行
（< keep_recent_n + 2 = 10）不触发；第 5 轮末 11 行、split = 3、older slice
`[u1,a1]` 有 2 条会话行，门就开了，marker 落在 5→6 边界（cross-turn 或
intra-turn 取决于字符阈值检查点），两条路径都只持久化一个 marker。yaml 步骤注释
与 turn-5/6 fixture 注释同步改写。

### 3. e2e 断言过弱（已修）

`marker_rows=1` 会误中 `marker_rows=11/21`。输出改为 `marker_rows=[%s]`，
断言 `contains: "marker_rows=[1]"`（整行等价、无前缀歧义；`message_rows=` 仍打印
供诊断）。

### 4. `src/compact/rebuild.rs` 只是 intent-to-add（已修）

已 `git add src/compact/rebuild.rs`，index 里是完整文件（`A` 而非 ` A`）。

## 三轮评审（round 3）——BLOCKER + 两条 Secondary

### BLOCKER：AG-UI 重载丢掉 legacy summary（已修）

评审探针：append-only 线程文件 `[prompt,u1,a1,<boundary>,SUMMARY(unmarked),u2,a2]`
在 `load_thread_seed` 切掉声明前内容后，`rebuild_seed` 把唯一的 System 行当作
重建 prompt 丢掉 → seed 只剩 `[u2,a2]`，折叠历史静默消失。

修法（按评审建议收敛到单一共享位置）：`src/compact/rebuild.rs` 新增
`rebuild_declared_seed(stored, after_boundary)` —— 切最后一条 g157 声明；声明后
首行若是未标记的 `Role::System`，就地回填 `is_compaction_summary` 并 `warn!`；
随后交给 `rebuild_seed`。`SessionReader`（`read_transcript` 只负责解析）与
`load_thread_seed` 都调用它，不再各自切片/回填。`SessionReader::load_transcript`
保留 g157 的切分语义（展示路径），不再承担 marker 回填。

### Secondary 1：合成 marker 的日志级别（已修）

`debug!` → `warn!`，且随规则一起搬进 `rebuild_declared_seed`——同一事件只剩一个
位置、一个级别（与 `rebuild_seed` 的 legacy 分支一致）。

### Secondary 2：`load_messages` 被当「替换 transcript」用（已修）

审计两个调用点：TUI `ResumeSession`（`crates/recursive-tui/src/backend.rs`）与
CLI 微信 session 切换（`crates/recursive-cli/src/main.rs`）。二者都在已构建的
runtime 上 `set_transcript(seed)`，而 `load_messages` 返回的是 builder seed
（stored prompt 已丢，因 `RuntimeBuilder::build` 会重建自己的）。新增
`SessionReader::load_seed_for_existing_runtime(dir, runtime_transcript)`：把 runtime
自己的 leading 非 summary System prompt 重新接到 seed 前；若 runtime 无 prompt，
打 `warn!`（降级可观测）。两个调用点改走该入口。`cli/resume.rs` 仍走 builder
seed 路径，不受影响。

## Gates

- `cargo test --workspace --no-fail-fast` 全绿（65 组；lib 3456 条）。首轮并行跑时
  `mcp::tests::http_initialize_fails_fast_and_ignores_the_tool_budget` 以 919ms
  越过 900ms 阈值（负载所致的既有时间敏感 flake，源码无关；单跑 0.35s 通过）。
- `cargo clippy --all-targets --all-features -- -D warnings` clean。
- `cargo fmt --all -- --check` clean。
- `cargo test test_compact_contract` 11/11 通过。
