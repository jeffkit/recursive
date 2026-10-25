# 2026-10-25 — B1 合并批次:run 40195789550(main.rs)+ run 40178848836(pricing.rs)9 条 MISSED 全杀(0 排除)

**[22:5x] 交付行(wt-b1 席)**:`crates/recursive-cli/src/main.rs` + `src/llm/pricing.rs`
(仅测试域,产品逻辑零改动;`.cargo/mutants.toml` 未动——本批 0 真等价)。
两 run 的 MISSED **以日志为准修正主控预判**:9 条全都**不在 WARN 臂本体**,而是
files-scope 整文件扫描把 89db17c9 / c3ce9316 所触文件里既有的弱覆盖位点连带
浮出(与 #243 writer.rs 批次同形态)。**A scope(cli-mutants,225 mutants:
3 missed/217 caught/5 unviable)3 条**:`main.rs:837:13 ||→&&`、`:842:12 delete !`
——`async_main` 内 #150/#226 的 `block_hidden` 接线(bc3ec53e),决定是否调
`hardening.warn_if_exposed(&scrub)`;`:2638:40 +→-`——`http_security_posture_line`
rules 求和 `allow + deny + interactive` 的首个 `+`。**B scope(agent-mutants)6 条**:
`pricing.rs:149:52 *→/`(`RetryPolicy::backoff_for_budgeted` 窗口内退避,#203)、
`:329:40 >` 三兄弟 `==/</>=`(`context_window_tokens_for_model_effective` 的
`context_window > 0` 守卫)、`:355:64`/`:373:64 /→*`(`default_compact_threshold_
chars/tokens` 的 `20_000.min(ctx / 4)`)。处置 **9 杀**:A-3 既有钉
`http_security_posture_line_summarizes_permissions_rules_mode_and_interactive`
补一条 `deny: ["Edit"]`(原 deny 为空,`allow − deny + interactive` 与原式同值
1−0+2=3)→ 期望「permissions=4 rules」(:5422);A-1/A-2 新源码钉
`exposure_warn_is_gated_by_block_hidden_or_of_kernel_and_reexec`(:5605,两断言
:5613「block_hidden must be `prctl applied OR (clean re-exec | nothing to
protect)`」/ :5617「the exposure WARN must fire when the block is NOT hidden」,
空白归一 `squash_ws` 抗 rustfmt reflow)——谓词内联在进程入口 `async_main`
(该块会 `exec`),进程内不可执行,沿 89db17c9 源码钉口径;**顺手加固既有
`http_arm_fixes_headless_semantics`**:原 `rsplit_once("mod tests {")` 以最后一次
出现为界、而该字面量就在测试里,任何后加含 `mod tests {` 文本的测试都会把它
的 needle 卷进「生产半区」变恒绿(89db17c9 自匹配陷阱第二形态)→ 抽共享
`production_source()`(:5562):按真实模块头 `"\n#[cfg(test)]\nmod tests {\n"`
`split_once`(字面量里 `\n` 是转义序列,匹配不到自身)+ 剥 `\r`(Windows CI
CRLF 检出;本地 perl 全文件 5903 行 CRLF 模拟两钉绿)。B-1 新钉
`retry_policy_network_window_backoff_follows_the_doubling_schedule`(:570;既有
窗口测试全部只断 `is_some()` 从不断值——地板内 budgeted==count-only 逐 attempt
相等、attempt 2=4s、地板外 attempt 3=8s 而 count-only 已 None、max 5s 下 attempt
3/40 皆钳 5s);B-2/3/4 新钉 `context_window_effective_positive_override_wins_
but_zero_falls_through`(:937;`PinnedRecursiveHome` 钉 RECURSIVE_HOME 持
env_lock #222、同时屏蔽 remote cache 与开发机 providers.d;写
`providers.d/openai.toml` 把 gpt-4o window 覆盖为 0 + 新模型
user-wide-window-model=777000;fixture 守卫断言 merge 真把 gpt-4o 置 0;wide →
777_000 杀 `<`/`==`,gpt-4o → bundled 128_000 永不报 0 杀 `>=`/`==`);B-5/6 新钉
`compact_threshold_small_window_caps_the_summary_reserve_at_a_quarter`(:1044;
既有阈值测试全在 ≥128K 窗口、`/4` 从不生效,bundled 最小窗口 moonshot-v1-8k=
8000 作 fixture → chars 19_200 / tokens 4_800,mutant `*4` 让 reserve=20_000、
`saturating_sub` 归零)。**负控实证 10 条全红、每条 cp+cmp 逐字节还原**(终态行
号):A1 `||→&&` → :5607;A2 删 `!` → :5615;A3 `+→-` → :5426「permissions=2
rules」≠4;REG 删 :951 `with_headless(true)` → headless 钉 :5583(删行态)/5584
终态红,证重构未削弱;B1 → :582「attempt 1 … left=Some(500ms) right=Some(2s)」;
B2 `==`/B3 `<` → :970 left=128000 right=777000;B4 `>=` → :980 left=0
right=128000;B5 → :1051 0≠19200;B6 → :1053 0≠4800。**三门无管道全绿**:
`cargo fmt --all -- --check` EXIT=0 / `cargo clippy --workspace --all-targets
--all-features -- -D warnings` EXIT=0 / `cargo test --workspace` EXIT=0
74 suites 5322 passed / 0 failed / 12 ignored(基线 5318 + 4:pricing 3 + exposure
1;posture/headless 为改版)。中途 clippy 一次红 `needless_update`(deny 补齐后
`..Default::default()` 多余)删之后对终态重跑全部 A 负控。ubuntu-only 自查:新
测试零 `cfg`(#224,非 #228 盲区);env 仅经 PinnedRecursiveHome 持锁(#222);
B-1/5/6 纯函数无 env/fs。hardening.rs 禁区只读未动;mcp.rs 未动(#209);本地
mutants 未跑(§2b);GitNexus 不可用 grep 代:零生产符号改动,爆炸半径=测试域。
两 commit 按 scope:A(cli)main.rs = b605e5cd;B(agent)pricing.rs + 本 journal。未 push、
未动 main,待主控验收。备份 /tmp/b3-backups/{main.rs,pricing.rs}.final,负控脚本
/tmp/b3-negctl-{A,B}.sh,日志 /tmp/b3-*.log。

## Date

2026-10-25(B1 席,主控轮值注入 22:0x,墙钟 2.5h)

## Goal

主控合并批次:两处 scope 的未清 MISSED 合并钉死。
- A. 89db17c9 scope(Goal 429 main.rs/http):run 40195789550
- B. c3ce9316 scope(#239/#240 compact/pricing):run 40178848836

要求:产品逻辑零改动;真等价走负控实证 + exclude_re(正则转义 + `--list` 自检);
发现产品真缺陷停下报告;#222/#224/禁区;三门无管道;本地 mutants 勿跑(§2b);
ubuntu-only 自查。

## 清单修正(run 日志为准)

主控简报写「posture 行/interactive-without-hooks WARN 的算符与条件族」「compact
usage WARN 臂/pricing poison 臂的算符族」。`gh run view <id> --log | grep MISSED`
实际:

| # | 位点 | 函数 | 来历 | 是否 WARN 臂 |
|---|------|------|------|------------|
| A-1 | `main.rs:837:13 ||→&&` | `async_main` | bc3ec53e #226(block_hidden) | 否——#150 曝光 WARN 的**触发条件**接线 |
| A-2 | `main.rs:842:12 delete !` | `async_main` | 同上 | 同上 |
| A-3 | `main.rs:2638:40 +→-` | `http_security_posture_line` | 388a77c0 S3 | 否——姿态行 rules 计数 |
| B-1 | `pricing.rs:149:52 *→/` | `RetryPolicy::backoff_for_budgeted` | #203 | 否 |
| B-2/3/4 | `pricing.rs:329:40 > → ==/</>=` | `context_window_tokens_for_model_effective` | — | 否 |
| B-5/6 | `pricing.rs:355:64`/`:373:64 /→*` | `default_compact_threshold_chars`/`_tokens` | — | 否 |

#239 compact WARN 臂与 #240 poison 臂的钉本身未被点名(run 日志 compact/mod.rs
零幸存)。两 run 都是 files-scope 邻座欠账:触碰文件 → 整文件重扫 → 既有弱覆盖
浮出。处置方式不变:行为钉补齐;9/9 皆非等价,0 排除。

## Files touched

- `crates/recursive-cli/src/main.rs`(仅 `mod tests`,+66/−9 行左右)
  - 改 `http_security_posture_line_summarizes_permissions_rules_mode_and_interactive`:
    加 `deny: vec!["Edit".into()]`,needle `permissions=3 rules` → `4 rules`;doc 注明
    三类求和;删多余 `..Default::default()`(clippy needless_update)。
  - 新 helper `production_source() -> String`(:5562)+ `squash_ws`(:5573)。
  - 改 `http_arm_fixes_headless_semantics` 用 `production_source()`(原
    `rsplit_once("mod tests {")` 删除)。
  - 新 `exposure_warn_is_gated_by_block_hidden_or_of_kernel_and_reexec`(:5605)。
- `src/llm/pricing.rs`(仅 `mod tests`,+131 行)
  - 新 `retry_policy_network_window_backoff_follows_the_doubling_schedule`(:570)
  - 新 `context_window_effective_positive_override_wins_but_zero_falls_through`(:937)
  - 新 `compact_threshold_small_window_caps_the_summary_reserve_at_a_quarter`(:1044)
- 本 journal。
- **未动**:`.cargo/mutants.toml`(0 排除)、`src/hardening.rs`(禁区,只读)、
  `src/tools/mcp.rs`(#209)、任何生产行。

## Tests added / changed(4 新 + 2 改)

| 测试 | 杀 | 要点 |
|------|----|------|
| `exposure_warn_is_gated_by_block_hidden_or_of_kernel_and_reexec`(新) | A-1、A-2 | 源码钉,生产半区空白归一后断 `||` 形式与 `if !block_hidden { warn_if_exposed }` |
| `http_security_posture_line_summarizes_…`(改) | A-3 | deny 非空 → 4 rules;`+→-` 得 2 |
| `http_arm_fixes_headless_semantics`(改) | — | 切分方式加固,REG 负控仍红 |
| `retry_policy_network_window_backoff_follows_the_doubling_schedule`(新) | B-1 | 断值而非 is_some |
| `context_window_effective_positive_override_wins_but_zero_falls_through`(新) | B-2/3/4 | providers.d 0/777000 两臂 |
| `compact_threshold_small_window_caps_the_summary_reserve_at_a_quarter`(新) | B-5/6 | 8K 模型 /4 守卫生效 |

## Notes

1. **A-1/A-2 为什么是源码钉而不是行为钉。** `block_hidden` 谓词内联在
   `async_main`:`harden_process()` → `CredentialHandoff::capture()` →
   `scrub_process_env` → `reexec_clean_env`(macOS 首启会 `exec` 替换进程镜像)→
   `block_hidden` → `warn_if_exposed`。单测无法执行这段而不触发 exec/平台分叉。
   `warn_if_exposed` 自身行为已在 `src/hardening.rs` 钉住(`warn_if_exposed_*`
   两测试)。源码钉守的是接线的两个算符,精神同 89db17c9 的
   `http_arm_fixes_headless_semantics`(#231 口径「接线行不在进程内可测面」)。
2. **语义分析(存照,非等价断言)。** `delete !`:在 `HardenOutcome::Failed`
   (prctl 被 seccomp 拒)/ `Unsupported` + `ReExecOutcome::Failed` / Windows
   `NoExecveSemantics` 路径上,原版 WARN、mutant 静音——正是 #150 要消灭的
   fail-silent,**非等价**。`||→&&`:仅在恰一操作数为真时分叉;Linux prctl 成功
   (is_applied=true,reexec=CoveredByKernelMechanism)mutant 多调一次
   `warn_if_exposed`,但其首行 `is_applied() → return`;macOS 干净子进程
   (AlreadyCleanEnv)/ NotNeeded 时 mutant 多调一次,`exposed = removed + retained
   == 0 → return`——**除非** 子进程 scrub 的 `retained` 非空(HTTP 路由器白名单
   变量在 re-exec 子进程里是否回到 env)。这一面属 hardening.rs 内部,本席只读,
   不下等价结论;源码钉让问题 moot。
3. **建议(不在本单,需主控拍板,产品侧改动)。** 抽纯函数
   `fn exposure_block_hidden(hardening: &HardenOutcome, reexec: &ReExecOutcome) -> bool`
   (放 hardening.rs 或 main.rs 顶层),`async_main` 只调它;真值表测试 6 行即可把
   A-1/A-2 变成行为钉,源码钉可退役。#150/#226 域,涉禁区,留给原作者席。
4. **既有源码钉的切分陷阱(第二形态)。** 89db17c9 记录的是「needle 命中测试自身
   源码」;这次发现的是「切分标记本身在测试里,后加测试会移动边界」。
   `production_source()` 用真实模块头(含真实换行)切分,字面量里的 `\n` 是两个
   字符匹配不到;`split_once` 取首次出现(`mod tests {` 全文件仅一处,:4235)。
   加 `\r` 剥离:仓库无 `.gitattributes`,ci.yml 有 windows-latest(非阻塞但是
   「windows 专属 bug 的唯一探测器」),autocrlf 检出会让含 `\n` 的标记失配 →
   `expect` panic 假红。本地模拟:`perl -pe 's/\n/\r\n/'` 全文件 CRLF(5903 行)跑
   两钉绿,cmp 还原。
5. **B-2/3/4 fixture 设计。** 用既有 provider id `openai` 覆盖 gpt-4o window=0,
   对应 :324-328 注释的真实场景(providers.d 半成品条目);另加新模型名测正向
   覆盖。fixture 守卫 `find_preset_effective("openai")` 断 gpt-4o 已置 0,防 merge
   语义改变后测试空转。期望值用 `context_window_tokens_for_model("gpt-4o")`
   (bundled)而非字面 128_000,bundled 变更不误红。
6. **B-5/6 fixture 选型。** bundled 最小窗口 `moonshot-v1-8k`=8000(其次 32000/
   32768/65536 一批);8K 下 mutant 把阈值归零最直观,fixture 断言窗口值使目录
   变动时失败自解释。既有 `compact_threshold_tokens_is_less_than_context_window`
   对 8K 模型 mutant 下 0<8000 仍绿,不构成杀伤,故需精确值钉。
7. **三门过程。** 首次 clippy 红 `needless_update`(posture 钉四字段齐备后
   `..Default::default()` 多余)→ 删行 → 终态重跑 A 四负控 + 三门,全绿。fmt
   首跑红一次(三参 `assert!` 超宽)`cargo fmt --all` 后绿——按 memory 约定先
   fmt 再钉跑,少一轮门。
8. **爆炸半径(grep 代 GitNexus,工具不可用)。** `git diff -U0 | grep ^@@`:
   main.rs 全部 hunk ≥ :5399(tests 模块 :4235 起),pricing.rs 全部 hunk ≥ :563
   (tests 模块 :396 起)。零生产符号改动。
9. **ubuntu-only 自查。** 新测试零 `#[cfg]`(#224;非 #228 cfg 盲区,ubuntu
   mutants job 全可见);env 仅经 `PinnedRecursiveHome`(持 `env_lock`,#222);
   `tempfile` 已是 dev-dep;B-1/B-5/6 纯函数。
10. **负控脚本可复跑**:`/tmp/b3-negctl-A.sh`(4 条)/`/tmp/b3-negctl-B.sh`(6 条),
    每条 cp 备份 → sed 单行替换 → 定向 cargo test → cp 还原 → cmp。
