# Manual — 2026-10-24 — Goal 424 (#197 DESIGN 文档落 docs/)

## Date
2026-10-24

## Goal
Goal 424 = #197 第三交付物:上下文与压缩契约 DESIGN 文档落 `docs/architecture/compaction.md`(#186 收口最后一件)。执行路线 B1(纯 docs),S3 主控拍板 (a) 文档内追认「判例族即负控」。

## Files touched
- `docs/architecture/compaction.md`(新增,S1:四契约各一节 + 契约×回归×负控映射表 + 实例×落点 commit 映射表 + S3(a) 判例族即负控成文表 + Honest boundaries)
- `docs/architecture/index.md`(S2:Other Concepts 注册一行)
- `docs/architecture/persistence.md`(S2:压缩段 canonical rebuild 处加契约文档链接,2 行)
- `docs/architecture/invariants.md`(S2:Invariant #8 Impact 行加 compaction.md 指针,既有段落零改写)
- `docs/architecture/sessions.md`(S2:Related Concepts 加一行)
- `docs/architecture/agui.md`(S2:See also 加一行)

## Tests added
无(纯 docs 交付)。两表所有坐标核校后落:9 个 commit(7ad601bf/c5457d21/f20b31cb/fa0a1332/236defd3/60ff94c8/f72f6b11/e88131b9/d43e8d15)逐一 `git log` 实证;测试行号(mod.rs:834/876/943、runtime/tests.rs:2474/2531、run_core.rs:3570、cold_load.rs:644/657、agui.rs:2650)与代码坐标(kernel.rs:428-436、runtime.rs:151/553-574/620-637、mod.rs:372/433/562-573)逐处读源核实。

## Notes
- **发现文档漂移一处(未顺手改,升主控定夺)**:`docs/architecture/invariants.md:77` 与 `AGENTS.md` 引用的回归测试名 `compaction_keeps_tool_calls_paired_with_results` 在树中已不存在;现实测试为 `tests/invariants/tool_call_pairing.rs::compaction_preserves_tool_call_pairing`(:169,另有 `safe_split_point_never_splits_on_tool` :211、`session_resume_preserves_tool_call_pairing` :242)。tool_call_pairing.rs:11 注释自述「extends the existing …」即旧测名已被该文件取代。属 docs 漂移非代码漂移,本单 scope 不含重写既有段落,故只记录。
- **发现 index.md 未注册 persistence.md**(pre-existing,与本单无关):index.md 「Other Concepts」现仅列 Skills/Sessions/AG-UI/Invariants(+ 本单新增的 Compaction)。本单规格只要求注册新文档,未顺手补。
- 判例族→框架规则映射按 rebuild.rs module doc 四规则 + declaration 规则逐一成文,规则④(空 assistant)红灯在 `rebuild.rs` 单测 `rebuild_empty_assistant_rows_are_filtered`(+ 写侧 #196 守卫),不在 compact_contract.rs——文档如实标注。
- 三门:`cargo fmt --all -- --check` ✓;`cargo test --workspace` ✓(全 result 行 ok、0 failed);`cargo clippy --all-targets --all-features -- -D warnings` 见 commit 前终验。
- S4 未动:不关单、不动标签、不 push。
