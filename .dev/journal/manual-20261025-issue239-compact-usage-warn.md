# 2026-10-25 — #239 compact usage 缺省静默按 0 token 记账（两处）→ WARN 可观测

**[16:2x] 交付行（wt-b1 席）**：`src/compact/mod.rs` 两处
`usage.unwrap_or_default()`——`try_structured_compact`(:315，structured
路径) 与 `summarize` 的 free-text 回退臂(原 :707，现 :720)——provider
响应缺 usage 时静默按全零 token 记账，压缩消耗从预算（BudgetExceeded
判定）与成本账面双向消失无痕；:311 的 `info!` 只覆盖 structured→
free-text 回退、不覆盖 usage 缺失。修法=两臂改 `match` 显式 `None` 臂
`tracing::warn!`（target `recursive::compact`；`step` + `path=
"structured"`/`"free-text"` 字段；语义句「compaction usage missing;
accounted as 0 tokens (degradation)」），**记账语义零改动**（仍记 0 ——
#42/#193 族 usage 口径归一不在本单，只加观测）；Compactor 无会话身份
字段（运行时租户透明 ruling：kernel 侧不见 session identity），故携
`step` 不携 session_id。1 测试新增 `test_compact_contract_missing_usage_
warns`(:990)：正控 `Completion { usage: Some(15) }` 不告且取值保留在前；
structured 臂注入=`MockProvider` structured 队列硬编码 `usage: None`
（mock 现成形态，零 mock 改动）；free-text 臂注入=`Completion { usage:
None }` + 空 structured 队列（走 :311 回退）；断言语义句 + `path="…"`
+ `step=N` 字段 + 返回 usage==default；平台中性无 cfg 按 #224 口径，
不触 env 故无 #222 EnvGuard。负控两臂**各单独**还原 `unwrap_or_default()`
→ 钉各在对应臂断言红（structured「missing structured usage must be
announced」终态 :1035 / free-text「missing free-text usage must be
announced」终态 :1063；负控 panic 报 :1018/:1042 为还原态行号，差 −13）、
正控绿，`cmp` 逐字节还原 ×2。登记页 #239 ❌→✅。三门无管道全绿（fmt --check EXIT=0 / clippy
--workspace --all-targets --all-features -D warnings EXIT=0 / test
--workspace EXIT=0 5304 passed/0 failed/12 ignored，74 suites）。src/compact
禁区按本单授权解除、仅动两臂与测试区。未 push、未动 main。等主控验收。

## Date

2026-10-25（wt-b1 席，基线 ddc92e43 = origin/main 含 #238；主控轮值注入
16:5x，两联 #239+#240 之一）

## Goal

#239（P3，fix(observability)，Goal 428 S4 扫描立单）：compact 两路径
provider 缺 usage 的 `unwrap_or_default()` 静默零记账 → 缺 usage 臂
WARN（会话/阶段字段 + 语义句）+ 回归钉。owner ruling「任何降级必须
可观测」。#42/#193 族邻接——口径归一勿做，只加可观测。

## Files touched

- `src/compact/mod.rs`（产品：:315 structured 臂、:720 free-text 臂两处
  `unwrap_or_default()` → `match` + `None` 臂 WARN，各 +13 行；测试：
  `test_compact_contract_missing_usage_warns` 1 新增，紧邻 Goal 414
  contract 钉群之后）
- `docs/architecture/degradation-registry.md`（#239 行 ❌→✅，新行号 /
  观测方式 / 回归钉列）

## Tests added

- `compact::tests::test_compact_contract_missing_usage_warns`
  （`#[tracing_test::traced_test]` + `#[tokio::test]`，同文件 :990）：
  1. 正控：free-text 路径 `usage: Some(15)` → `total_tokens == 15`
     且 `!logs_contain(SENTENCE)`（置于注入之前，避免缓冲区串扰）；
  2. structured 臂：`MockProvider::new(vec![]).with_structured_responses(
     [Ok(json!{summary,kept_facts,next_steps})])` → mock 返回
     `StructuredCompletion { usage: None }`，渲染含「[compacted: structured
     at step 7]」证实确走该臂；断言 usage==default + 语义句 +
     `path="structured"` + `step=7`；
  3. free-text 臂：空 structured 队列（mock 默认 Err → :311 info! 回退）
     + `Completion { usage: None }`，渲染不含「structured at step」证实
     确走该臂；断言 usage==default + `path="free-text"` + `step=9`。
- 负控实证（临时还原、钉红、逐字节还原）：
  - 轮 1 仅还原 structured 臂 → `panicked at src/compact/mod.rs:1018:9:
    missing structured usage must be announced, not silently zeroed`，
    EXIT=101；`cp` 快照回 + `cmp` 通过；
  - 轮 2 仅还原 free-text 臂 → `panicked at src/compact/mod.rs:1042:9:
    missing free-text usage must be announced, not silently zeroed`，
    EXIT=101；`cp` 快照回 + `cmp` 通过。
  两轮正控段均绿（panic 点都在注入段之后），证明钉对两臂**各自独立**敏感。
  （panic 行号为还原态：还原一臂少 13 行；终态 `assert!` 在 :1035/:1063。）

## Notes

- **三门无管道**（日志 /tmp/b1-239-{fmt,clippy,test}.log）：见提交信息
  尾部数字；三者均 `> log 2>&1; echo EXIT=$?`，无 `| tail`/`| grep`。
  首轮 fmt --check 红（新测试两处 `assert_eq!` 三参超宽需换行，纯空白），
  `cargo fmt --all` 后停掉首轮 test 跑、三门整体重跑于终态树。
- **爆炸半径（GitNexus MCP 本席不可用，grep 代替，同 #231 注 6）**：
  `try_structured_compact` 唯一调用方 `summarize`(:706)；`summarize`
  唯一生产调用方 `compact` 的 PTL 重试环(:794)。两处改动只新增一条
  log 事件，`Option<(String, TokenUsage)>` / `Result<(String, TokenUsage)>`
  返回值与控制流零变——`None` 臂返回 `TokenUsage::default()` 与原
  `unwrap_or_default()` 等值。风险 LOW。
- **字段取舍**：issue 建议「session/turn 字段」。`Compactor` 结构体无
  session id（kernel/runtime 按运行时租户透明 ruling 不见 identity），
  `compact()` 的 `#[tracing::instrument]` span 已携 `step`；本 WARN 显式
  携 `step` + `path` 两字段，`path` 区分两臂便于定位（free-text 臂只在
  structured 失败后才到，两臂同一会话同一 step 可能各响一次）。
- **噪音评估**：每次压缩最多 1 条（两臂互斥——structured 成功则不走
  free-text；structured 缺 usage 但后续 JSON 校验失败回退时可能两条，
  属应得响度：两次 provider 调用的 token 都丢了）。provider 持续不报
  usage（如某些 OpenAI 兼容网关）会每次压缩响一次——这正是本洞的目标
  信号，未做去重/限频。
- **正控覆盖面注记**：structured 臂的「usage 存在 → 不告」正控不可用
  `MockProvider` 构造（其 `complete_structured` 硬编码 `usage: None`，
  见 src/llm/mock.rs:181），改 mock 超出本单口径；正控落在 free-text
  臂，两臂 `match` 形态同构，`Some(usage) => usage` 分支由 mutants
  终判覆盖（替 `Some` 臂为 default → 正控 `total_tokens == 15` 红）。
- **#42/#193 邻接（未动）**：usage 数值面口径（normalised split、
  cache 字段折算）与本单正交——本单只在 `Option` 为 `None` 时发声，
  `Some` 内的数值不做任何判断或改写。
- mutants 前瞻（本地未跑，按单规 §2b）：本单新面已有钉——删任一 warn →
  对应臂断言红（负控已实证）；`None` 臂返回值替为非 default → 两处
  `assert_eq!(usage, TokenUsage::default())` 红；`path` 字串互换 →
  `path="structured"`/`path="free-text"` 断言红。
