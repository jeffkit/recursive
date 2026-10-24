# #223 — 变异幸存者收敛三批全账（e197bbce / 955a2fa9 / e8b74633）

> **转录**（2026-10-24 回填；证据原址：#223 各批次主控验收评论与关单评论 +
> commit message；本文为台账转录，未复跑任何变异，不改写数字）。

自 6a0e8a10（#210）起 main 落地变异幸存全部收敛；终判 =
**[run 40000505454](https://github.com/jeffkit/recursive/actions/runs/40000505454)**
（e8b74633，exclude_re 生效后首个 main.rs scope run）：**Mutation 零
MISSED，success**。

## 批次 1 — e197bbce（27 条钉死）

- **源 run**：[39854090906](https://github.com/jeffkit/recursive/actions/runs/39854090906)
  @ 9a38a91 —— `src/eval.rs` scope：136 mutants = 27 MISSED / 92 caught / 17 unviable。
- **落地**：e197bbce（cherry-pick 自 B1 wt-b1 4f8acb7e；基线 b0cd7a76）。
- **处置**：27 条逐条钉死 = 9 测试新增 + 1 invariant 去空转，**产品逻辑零
  改动**（+435/-4 全在 `mod tests`），零等价类豁免、未加 exclude。
  根因两类：① 既有测试空转（`tasks()` 公开访问器无人走；`preset_carries_run_code`
  断言全在允许方向；`summarize` 三算符在 cache=0/turns=1/tools=0 上外延相等）；
  ② 纯函数零覆盖补数值/盘面断言（`seed_*`×4 tempdir 逐字节、`Mode::as_str`
  字面量、`estimate_*` 双钉、`summarize_by_category` 逐格、`mechanism_pairs`
  三 preset 联合否证 `||`、`render_markdown` provenance 串/免责注/零分母）。
- **独立复跑三门**（主控，wt-b1 原地全量重建）：`cargo test --workspace`
  EXIT=0（5192 passed / 0 failed / 12 ignored，全为 #224 预存 windows 项）；
  clippy -D warnings EXIT=0；fmt --check EXIT=0。
- **本地 cargo-mutants 复验中止事实（协议 §2b 的由来）**：12:44–13:53 完成
  25/136（caught 22 / unviable 3 / **missed 0**）后主控终止——file scope 全
  候选 4 worker scratch 实测占盘 ~39G、1h 仅 25 条、两次压盘至 11G 临界；
  改由 CI Mutation 落地门（push e197bbce 触发）终裁。此口径入舰队协议 §2b
  （本地复验禁用、CI 终裁）。

## 批次 2 — 955a2fa9（23 条处置全账：10 钉 + 8 门口径 + 3 等价维持）

- **源 run**：[39922642466](https://github.com/jeffkit/recursive/actions/runs/39922642466)
  @ 85a17c05（基线含门口径 ecd1b471）。
- **落地**：955a2fa9（cherry-pick 自 wt-b2 2c6b0bc3）。
- **10 条钉死**（独立复跑三门：workspace 5227 passed / 0 failed；clippy 0；
  fmt 0）：
  - 6 条 WARN/scrub 路径加 **tracing 捕获层断言**（report→() /
    warn_if_exposed→() 断言 WARN 事件本体 + 结构化字段
    mechanism/errno/os/count/variables；:129 `+`→`-` 由 count=3 求和钉；
    :130 `==`→`!=` 双静默钉；:305/:311 delete-! 三趟 DEBUG 双向钉）
    ——「降级必须可观测」的观测本体自此有回归钉；
  - 3 条 probe_config retry 字段**哨兵基值显式断言**（#195 等价论证撤下）；
  - 1 条 `async_main:797` delete-! 新增真实二进制集成测
    （`cli_init_keeps_env.rs`，pinned home + 自定义 preset key_env 探针钉
    「Init 豁免」契约；手工变异验证删 `!` 必红）。
- **8 条结构性 → 门口径闭环**：`is_deferred_consumer`×2（agent 门无
  vector-memory/e2b-sandbox，体不编译恒等价）+ weixin×6（cli 门 FEATURES=""）。
  **主控拍板落地 ecd1b471**：`agent-mutants.sh` FEATURES +vector-memory,
  e2b-sandbox（成本≈0）/ `cli-mutants.sh` FEATURES +weixin（tui 门先例）；
  weixin 6 条由 6a0e8a10 既有夹具钉死，门补 feature 后本 push 的 Mutation
  run 全量可判。
- **3 条等价维持**（tail×2 / plan_approval，6abe69c0 论证照旧——后经批 3
  负控实证收口）。

## 批次 3（收尾）— e8b74633（3 条真等价类，exclude_re 收口）

- **源 run**：[39943859391](https://github.com/jeffkit/recursive/actions/runs/39943859391)；
  落地 d4bcb120（cherry-pick）→ main 为 e8b74633。
- **B2 以负控实证推翻派单前提**：「tail 两条可杀」经核伪——
  tail_entries/tail_messages 守卫仅在 n==len 分叉，`<=` 变异后 slice 臂
  start==0 与 `_` 臂同返回全量向量，**不存在外延可分的输入数**。
  负控：手工施加三变异 → tail 单元 ×2 + plan_approval + `sessions show
  --tail` 集成 ×2 **全绿 EXIT=0**（全 crate 断言面等价）；正控：守卫换
  `>` **立红 EXIT=101**（断言面非空洞）。plan_approval 守卫：空白串必
  parse Err 同落 Some(DEFAULT_SECS)（任务书情形②，主控已批准）。
- **处置选型**：`.cargo/mutants.toml` `exclude_re` 按打印名排除 3 条
  （e0599 日志记录逐字对齐，render_empty_state 先例）；**否决
  `#[mutants::skip]`**——item 级上不了 match arm，整函数级会把已被 CI
  杀掉的 `Ok(0)=>None` / `Err=>Some(DEFAULT)` / delete-! 一起隐藏 = 过度
  skip。测试加固仍有：tail==len 边界行内容级 pin（boundary_removed helper）。
- **独立复跑三门**：workspace 5238 passed / 0 failed；clippy 0；fmt 0；
  产品逻辑零改动（main.rs +26 行中 20 为注释/文档性）。

## 收敛与沉淀

- #223 关单（Policy-ack: jeffkit 2026-10-22 链）；三期全账如上。
- 路过教训已固化：协议 §2b（本地 mutants 复验禁用、CI 终裁）；
  cfg(linux) 盲区自查项（080c1e4e）；门口径与批次验收一致原则（ecd1b471）。
- 后续新 push 若再出 MISSED，#223 按惯例重开或在新批次评论接续
  （mutants.yml「File keeper issue on survivors」步骤口径不变；
  #228 hardening.rs 34 条真幸存即按此轨道另派批次）。
