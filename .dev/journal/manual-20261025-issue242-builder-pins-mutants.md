# 2026-10-25 — #242 85bac8fb builder.rs scope 变异幸存 2 条收敛（skill_source_sha256_pins 三钉）

**[10:2x] 交付行（wt-b2 席）**：run 40144987139（85bac8fb = #231 落地，builder.rs
files scope）2 MISSED 均在**邻函数** `skill_source_sha256_pins`（:682-691，#74 预存，
非 #231 改动；`file_config_or_report` 本体零 MISSED）：`-> Vec<String>` 整体替为
`vec![String::new()]` / `vec!["xyzzy".into()]`。根因：私有 fn 从未被直接测，6 条调用方
测试要么 URL 未设早返回、要么恰 1 URL（1 幻影 pin == 1 URL 计数门静默）、要么 2 URL +
1 真 pin（mutant 同 1 → 同一 Err）——无测试区分「空」与「1 幻影 pin」、无测试断言 pin
内容。处置：3 测试新增全钉死（2 单元：unset/空白/分隔符→空、` aaaa , ,bbbb,`→
`["aaaa","bbbb"]` trim/去空/保序；1 调用方层：2 plain-http URL + SHA256 unset → 真码撞
https 策略 / mutant 撞计数门「has 1 pin(s) for 2 URL(s)」=幻影 pin 现实爆炸半径），零
exclude_re、零等价论证、**产品逻辑零改动**（+71 全 `mod tests`）。负控：手工植入两
mutant 本体各 `0 passed; 3 failed`（6/6 红格），cmp 逐字节还原×2。三门无管道全绿
（fmt --check 0 / test --workspace EXIT=0 5293 passed / 0 failed / 12 ignored(#224 预存
windows 项) / clippy --workspace --all-targets --all-features -D warnings 0）。证明 `docs/proofs/242-85bac8fb-builder-sha256-pins-survivors.md`
+ README 索引。终判 = 落地后 push→main Mutation run（待填）。未 push、未动 main。

## Date

2026-10-25（wt-b2 席，基线 aa8a5cb7 = origin/main，开工 fetch+reset 树净、`.dev/` 无
#242 引用无双开）

## Goal

#242（P2）：85bac8fb 在 mutants 落地门检出幸存者，按 MISSED 清单强化测试使每条被杀。
主控预判「变异常在 file_config_or_report 的 Err 变体组句分叉/兜底臂」——**实际清单
推翻预判**：`gh run view 40144987139 --log | grep MISSED` 只有 2 条，全在
`skill_source_sha256_pins`（files scope 把同文件预存函数扫进范围）。MISSED 清单是
地面真值，按清单处置。

## Files touched

- `crates/recursive-cli/src/cli/builder.rs`：`mod tests` +71 行（3 测试，紧贴
  `skills_from_http_sources_pin_count_must_match_url_count` 之后）；产品代码零改动。
- `docs/proofs/242-85bac8fb-builder-sha256-pins-survivors.md`：新建（README 模板六字段）。
- `docs/proofs/README.md`：索引 +1 行。
- 本 journal。

## Tests added

1. `skills_from_http_sources_unpinned_multi_url_clears_pin_count_gate`（:1096）——
   2 URL 未 pin 必须过计数门到 https 策略；`err.contains("https://")` +
   `!err.contains("pin")`。`#[test]` 无运行时（https 门在 `HttpSkillSource::new`
   校验路径触发，先例 `plain_http_url_errors`）。
2. `skill_source_sha256_pins_unset_or_separators_only_is_empty`（:1121）——
   None / `"   "` / `" , ,"` 三臂 scoped（EnvGuard 非可重入先例）。
3. `skill_source_sha256_pins_parses_positionally_trims_and_drops_empties`（:1149）。

## Notes

1. **files scope 的连带责任**：触及某文件的 commit 会让该文件全部预存函数进变异范围；
   邻函数的历史欠账会记到本 commit 的 run 头上。处置口径不变（按清单杀），但报告里
   要写明归属，免得把「#231 代码有漏」误读进登记。
2. **为什么 1-URL 测试天然盲**：mutant 返回长度 1 的 Vec，而 `pins.len() != urls.len()`
   在 1 URL 下恰好不触发——任何只用单 URL 的调用方测试都杀不了「幻影 pin」族。多 URL +
   未 pin 是这族 mutant 的唯一调用方层观测点，且无需网络（第一 URL 用 plain-http 让
   https 门先于 fetch 报错）。
3. **负控快照与 fmt 顺序**：负控 cmp 基准取于 fmt 之前；fmt 后只改本席测试一处
   `EnvGuard::set(&[(..)])` 折行，产品函数 :682-691 与基线逐字节同（`git diff` 71 行
   全在 `mod tests`）。
4. 三门按席位记忆 zsh 口径：无管道、各门 `;` 隔开、`.log`+`.exit` 落盘、无 glob。
   test/clippy 两门后台串行（争 target 锁）。clippy 跑 `--workspace` 超集。
5. 未加 `exclude_re`：两条都真可杀，不属等价类；#228 批 2 的「正则转义 + `--list`
   自查」教训本单无触发面。
6. GitNexus MCP 本会话不可用（工具表无 `gitnexus_*`）；零产品符号改动，blast radius
   手工核：`skill_source_sha256_pins` 唯一调用方 `skills_from_http_sources`（:604）。
7. 台账文件不在仓内及邻近路径（`.dev/`、`docs/`、`../`），按 #231 先例把 `[HH:MM]`
   交付行写在本 journal 首段 + 终报，由主控转录。
