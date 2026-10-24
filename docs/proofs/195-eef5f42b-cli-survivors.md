# #195 — eef5f42b cli 幸存者 18 条：triage 与处置全账

> **转录**（2026-10-24 回填；证据原址：#195 评论（triage 2026-10-21、关单
> 2026-10-22）+ commit message；本文为台账转录，未复跑任何变异，不改写数字）。

## 基线

- **基线 sha**：eef5f42b（幸存者所在提交；triage 复核基线 c6add743）
- **源 CI run**：https://github.com/jeffkit/recursive/actions/runs/38101765315
  （192 mutants：18 MISSED / 170 caught / 4 unviable，全部落在
  `crates/recursive-cli/src/main.rs`）
- triage 复核口径：18 条涉及的函数在 `eef5f42b..c6add743` 之间无改动
  （仅行号右移 5–6 行），清单语义有效。

## Triage 结论（2026-10-21，A 席只读复核）

**真 13（P1 6 / P2 5 / P3 2）/ 等价 5 / 存疑 0 / 现有产品缺陷 0。**
口径：真 = 变异改变可观测行为而现有测试杀不掉（门禁漏网的编辑）；
等价 = 所有输入下产物相同、测试不可区分。

### 18 条逐条判定

| # | 基线位置（当前） | 变异 | 判定 | 场景 / 理由 |
|---|---|---|---|---|
| 1 | 903:24（:908） | 删 `!`（`if !src_skills.is_empty()`） | **真 P2** | `recursive http` 启动：服务级 SkillSource 返回非空时不再 `append`，远端技能静默丢失；空时反而多打一行 "loaded 0 skill(s)"。 |
| 2 | 921:16（:926） | 删 `!`（`if !skills.is_empty()`） | **真 P2** | 有技能时不再用"目录+远端"合并表替换 `LoadSkill`，远端技能不可加载；零技能时反而注册一个空 Skill 工具。 |
| 3 | 922:17（:927） | `&&` → `\|\|` | **真 P2（安全）** | `allow_tools` 非空且 `Skill` 已被 #65 过滤时，变异会复活 Skill → allow-list 绕过。`main.rs:926-927` 注释明确写了"allow-list 必须是最后一句"。 |
| 4 | 922:59（:927） | `\|\|` → `&&` | **真 P3** | `allow_tools` 为空且 Skill 不在表里时不再注册 Skill；或 `allow_tools` 非空但 Skill 在表里时跳过合并注册 → 远端技能不可用。 |
| 5 | 1780:5（:1780） | `probe_llm` → `Ok("")` | **真 P2** | `doctor --probe` 在死 key/网关不可达时仍打印 `✓ LLM reachable` 并 exit 0。该函数是唯一能发现 revoked key 的检查，当时无 `--probe` 测试。 |
| 6 | 1780:5（:1780） | `probe_llm` → `Ok("xyzzy")` | **真 P2** | 同上，且伪造 detail。 |
| 7 | 1781:9（:1786） | 删 `retry_max` | **真 P3** | probe 不再强制单发，继承 `config.retry_max`（默认 8）与 600s 网络窗 → 故障时 `doctor --probe` 从单发变成分钟级重试。 |
| 8 | 1782:9（:1787） | 删 `retry_initial_backoff_secs` | 等价 | `retry_max=0` 时 `RetryPolicy::backoff_for_budgeted/backoff_for` 在读取 backoff 之前就返回 `None`（`src/llm/pricing.rs:122` / `:148`），字段不可观测。 |
| 9 | 1783:9（:1788） | 删 `retry_max_backoff_secs` | 等价 | 同上。 |
| 10 | 2406:22（:2407） | `<` → `<=`（`tail_entries`） | 等价 | `n == len` 时 `start = 0`，`entries[0..]` 与 `entries` 内容完全相同（只有 clone vs move 的差别，测试不可观测）。 |
| 11 | 2417:22（:2421） | `<` → `<=`（`tail_messages`） | 等价 | 同上；两个分支本来就都是 `to_vec()`。 |
| 12 | 3340:20（:3343） | guard `!raw.trim().is_empty()` → `true` | 等价 | 空白串仍走 `parse("")` → `Err` → `Some(300)`，与 `_ => Some(300)` 相同。 |
| 13 | 3797:5（fn :3795） | `switch_weixin_session` → `None` | **真 P1（weixin）** | 单 runtime 串行服务多用户；切换失效后 runtime 保留上一个用户的 transcript，下一用户消息续进他人会话 → **跨用户串话/隐私泄露**。 |
| 14 | 3797:5（fn :3795） | 同上 → `Some("")` | **真 P1（weixin）** | 同上，且 caller 缓存空 id、`switched=true`，后续更不会再尝试切换。 |
| 15 | 3797:5（fn :3795） | 同上 → `Some("xyzzy")` | **真 P1（weixin）** | 同上，缓存伪 id；runtime 实际未切换。 |
| 16 | 3844:5（fn :3850） | `start_fresh_weixin_session` → `None` | **真 P1（weixin）** | 首次用户与 `/r` 都走此函数；失效后保留旧 transcript，`/r` 静默无效，同样跨用户串话。 |
| 17 | 3844:5（fn :3850） | 同上 → `Some("")` | **真 P1（weixin）** | 同上，并缓存空绑定。 |
| 18 | 3844:5（fn :3850） | 同上 → `Some("xyzzy")` | **真 P1（weixin）** | 同上，并缓存伪绑定。 |

triage 重要修正：默认 feature 门会**继续生成** weixin 6 条（cargo-mutants
对 cfg 掉的函数照样生成变异、测试不编译该函数 → 全绿 → 记 missed），
不能按 12 条派单，也不得以 exclude 掩盖 P1 串话面。

## 逐条处置（落地落点）

### 真 13 条 → 三子单（全 CLOSED）

| 子单 | triage 编号 | 落地 commit | 终验 |
|---|---|---|---|
| #210（P1）weixin 单 runtime 会话切换 | 13–18 | **6a0e8a10** | weixin 6 条 cargo mutants 实跑全 CAUGHT（`--features weixin` 夹具 + `CLI_MUTANTS_FEATURES` 门入口，未 exclude） |
| #211（P2）HTTP skills 注册守卫 + doctor --probe | 1–3、5–6 | **fdaaf666** | 真值表 + 穿透断言；probe mock provider 断 `✗`/exit 1 与单发不重试 |
| #212（P3）922:59 与 1781 retry_max | 4、7 | **e244483c** | 对应条目零幸存 |

### 等价 5 条 → #223 批 2 / 批 3 处置

- **1782 / 1783**（probe_config retry 字段）：#223 批 2（**955a2fa9**）
  **撤下等价论证、改哨兵基值显式断言钉死**——批 2 账原话「3 条 probe_config
  retry 字段哨兵基值显式断言（#195 等价论证撤下）」。
- **2406 / 2417 / 3340**：#223 批 3（**e8b74633**）维持真等价定性，
  以**负控实证**确认不可杀（手工三变异 → tail 单元 ×2 + plan_approval +
  `sessions show --tail` 集成 ×2 全绿 EXIT=0；正控：守卫换 `>` 立红
  EXIT=101，断言面非空洞），随后 `.cargo/mutants.toml` `exclude_re`
  按打印名收口（render_empty_state 先例；否决 `#[mutants::skip]` 防过度 skip）。

## 终判

- #210/#211/#212 零幸存达成后 #195 关单（Policy-ack: jeffkit 2026-10-22）。
- 等价收口与全账收敛的终判 = #223 终判 run
  **[40000505454](https://github.com/jeffkit/recursive/actions/runs/40000505454)**
  （零 MISSED，success），全账见
  [`223-mutation-batches-1-3.md`](223-mutation-batches-1-3.md)。
