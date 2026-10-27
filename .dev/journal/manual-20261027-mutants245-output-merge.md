# 2026-10-27 — #245 output.rs 双批真合并修复(wt-b1 席)

**[20:4x] 交付行(wt-b1 席)** 主控紧急单「output.rs 结构损坏修复」完成:坏件(4c997f61 的 1239 行,EOF 括号深度 +1)重建为 `2aa039f7 基底 + fa4ac7b2 尾块` 的 1241 行干净版;两侧测试 fn 全保留(40 fn 零重零缺),三门绿(bin 267/267 + clippy -D warnings + fmt),产品逻辑零改动,commit 落 wt-b1 等验收。**简报两处指认勘误,见 Notes 1/2 —— 落地前请主控过目。**

## Goal

修复 main 上 output.rs 双批(fa4ac7b2 B2 尾批 + 「70db6db4 B1 前段」)被朴素并集拼接造成的 `mod tests` 括号失衡(CI Format 红:unclosed delimiter at :1239)。

## Files touched

- `crates/recursive-cli/src/cli/output.rs`(重建,1241 行)
- `.dev/journal/manual-20261027-mutants245-output-merge.md`(本文件)

## Tests added

无新增测试(纯合并修复);保留两侧全部测试:基底 2aa039f7 的 34 fn + fa4ac7b2 尾块 6 fn(2 helper + 4 test)= 40 fn。

## Notes

1. **勘误一:70db6db4 未触碰 output.rs**(该 commit 只有 control.rs +142 与 journal)。简报的「你的前段 10 条(流事件族)从 `git show 70db6db4:…output.rs` 提取」无的放矢;主控看到的 261 行差量实为 `9ad785a8→70db6db4` 的跨线文件差 = main 已有的批次4(439bb6e6 流事件族)+ 批次5(2aa039f7 其余)内容,而非独立前段。
2. **勘误二:fa4ac7b2 不能直接作基底。** 其父是 9ad785a8(批次2,763 行),不含 main 已落地的批次4/5 共 12 个测试 fn——按简报字面「fa4ac7b2 版本 + 前段」会静默删掉 main 已有的 12 fn,违背「两侧测试 fn 全保留」。实际基底 = `2aa039f7`(批次5 整文件收口版,994 行,= 尾段落地前 main 的 output.rs 字节态),叠加 fa4ac7b2 相对 9ad785a8 的唯一增量(单尾 hunk `@@ -762,0 +763,247`:output_pin_child_case / run_output_pin_child 两 helper + print_usage×2 / print_finish_note / exit_for_finish 四 test)。
3. **坏件法证**:main 与 wt-b1 同在 4c997f61;坏件 1239 行 = fa4ac7b2(26 fn)+ 批次4/5 增量(12 fn)的并集,fn 集合与重建版完全一致(零缺失零多余)——纯尾部拼接结构损坏,无内容损失也无重复。
4. **重建与审计**(批次8 落位合并同法,自 git 对象重建、不做冲突块 "take both"):基底 1–993 行 + fa4ac7b2 763–1009 行字节原样 + 基底 994 行 `}`;四重审计全过 = 前缀/块字节 cmp 一致、括号与圆括号余额 0、fn 集合 base∪b2new 精确相等、`cargo fmt --all -- --check` 首跑即绿。vs 2aa039f7 呈单一尾部 hunk。
5. **三门**(三门无管道,日志 /tmp/gate_*.log):`cargo test -p recursive-cli --bin recursive` 267 passed / 0 failed(51s);`cargo clippy -p recursive-cli --all-targets --all-features -- -D warnings` 绿(2m20s);fmt 如上。简报的 `--lib` 门对 recursive-cli 无效——纯 bin 包无 lib target(error: no library targets found),bin 单测走 `--bin recursive`。
6. **helper 异名并存**:fa4ac7b2 的 pin child 协议(output_pin_child_case / run_output_pin_child)与批次5 的 usage_child_* / child_stderr 异名同义,并存保留、互不影响(简报的同名规则不适用;不为此引入删除/改名风险)。
7. **产品逻辑零改动**:2aa039f7 / fa4ac7b2 / 重建版三者的生产半区(`mod tests` 之前)逐字节 cmp 一致。
8. 批次4「流事件族 29 条」中的条数为变异体口径;「前段 10 条」与任何实际增量对不上,应为简报对差量的目测口径,特此留痕。
