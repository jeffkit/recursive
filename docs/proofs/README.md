# docs/proofs/ — 变异判死证明库

镜像约定 §3（`docs/landing-and-acceptance.md`，owner 2026-10-18 拍板）：
**变异判死证明随修复提交进仓**。本目录收录每个「变异全杀死/幸存者收敛」
声称的证明台账：基线、源 run、逐条处置、终判。

## 命名

`<issue>-<slug>.md`（§3 原文）。现有文件：

- [`195-eef5f42b-cli-survivors.md`](195-eef5f42b-cli-survivors.md) — #195 eef5f42b 幸存者 18 条 triage 与三单处置（#210/#211/#212 + #223 批 2/3）
- [`223-mutation-batches-1-3.md`](223-mutation-batches-1-3.md) — #223 三批全账（e197bbce / 955a2fa9 / e8b74633）与终判 run 40000505454
- [`242-85bac8fb-builder-sha256-pins-survivors.md`](242-85bac8fb-builder-sha256-pins-survivors.md) — #242 85bac8fb builder.rs files-scope 幸存 2 条（邻函数 `skill_source_sha256_pins`，run 40144987139）三钉收敛 + 负控 6/6 红

## 一份证明的最小字段

1. **基线 sha** — 证明对应的 main/worktree 基点；
2. **源 CI run URL** — 检出幸存者的 Mutation run（mutants.yml push→main 落地门）；
3. **幸存者清单与逐条处置** — 每条标注处置类型：
   `钉死-测试名` | `等价-论证（出处）` | `exclude_re-条目` | `门口径-闭环 commit`；
4. **负控实证记录**（如适用）— 手工变异 + 正控立红，证明「不可杀」非「忘了杀」；
5. **终判 run URL** — 宣称零 MISSED 的那次 Mutation run；
6. **复核口径**。

## 复核口径（两条并存，不矛盾）

- 协议 §3「幸存者必须独立进程 recheck」——**独立进程 = CI runner**：
  mutants.yml 落地门的 Mutation run 是终裁（如 #223 的 run 40000505454）。
- 协议 §2b「本地 mutants 复验禁用（占盘 ~39G / 1h 仅 25 条，两次压盘至
  11G 临界；#223 批 1 实证后 2026-10-22 定约）」——本地只做**手工变异
  负控**（单点临时变异 + 定向断言，分钟级；先例：#223 批 3 推翻「tail 两条
  可杀」预设），不做全量复验。

## 诚实边界

- 回填文件是**转录**：证据原址为 issue 评论与 commit message；文内显式
  标注回填日期，不改写任何数字，无「本次复跑」。验收者按字段溯源原址。
- 后续批次（如 #228 hardening.rs 34 条真幸存）的证明文件按本 README 模板
  落到本目录，作为第一个前向消费者。
