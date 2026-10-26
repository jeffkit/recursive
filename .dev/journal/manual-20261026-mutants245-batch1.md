# 2026-10-26 — #245 批次 1:full 全仓首审首批(agent eval 前 10 + cli claude_json 31)

- **Date:** 2026-10-26,交付行 [23:10]
- **Goal:** #245 批次 1 — run 40256596759(full --all 首审:cli 222 / agent 79 / tui 54)首批清偿
- **Files touched:** `src/bin/recursive-eval.rs`(+135,仅 mod tests)、`tests/eval_cli.rs`(新,子进程钉)、`crates/recursive-cli/src/cli/claude_json.rs`(+309,仅 mod tests)、`.cargo/mutants.toml`(+43,7 条等价豁免)
- **Tests added:** 16(4 bin 单测 + 2 集成 + 10 claude_json 钉)

## [23:10] 交付行

**清单口径修正(最重要)**:任务书写「recursive-agent 10 条」,实测 run 40256596759 的 agent 段 **MISSED 79 条**(cli 222/tui 54 与任务口径一致)——79 = `src/bin/recursive-eval.rs` 44 + agui_session 5 + audit_log 14 + checkpoint 7 + compact/mod 9;「10」疑为主控拉日志时 agent job(5h30m)尚在途的中途快照。本批按指示取**时间序前 10 条**(全在 recursive-eval.rs,第 11 条 `:128 *=` 同族被同一测试顺带钉死,实杀 11),其余 68 条见下方分段建议。

**agent 11 条杀伤(eval bin,产品零改动)**:6 条 parse 变异用 4 组不同 flag 偏移的解析钉(`parse_run_options_happy_path_parses_every_flag`):`:112 <`→`<=`(i 越界 panic)/`>`(循环不进、mode 落 Replay)、`:114 +`→`-`(value 闭包下溢 panic)/`*`(读到 flag 本身报 unknown mode)、`:124 +=`(—mode 臂)`-=`(@0 下溢 panic)/`*=`(@4 时跳过 ["--report","r.md"] 对,report 断言红)、`:128 +=`(—presets 臂)同两法;`:57 dispatch→Ok(())` 用 current_thread runtime 单测(unknown command 必须 Err);`:46 main→Default::default()`(ExitCode::SUCCESS=0)与 `:73 list→()`(stdout 空)进程内不可观测,按仓内 `mock_mcp_server` 先例用 `env!("CARGO_BIN_EXE_recursive-eval")` 子进程集成测试钉(exit code 2 + stderr「unknown command」;list 输出含 "tasks ("/"presets:"/"single-append-row")。附 2 条契约钉(bad-value/repeat clamp)。

**cli claude_json 31 条 = 24 杀 + 7 真等价豁免**:24 条由 10 个新钉覆盖(resolve 未知格式 `||`、AssistantText/Usage/Latency/Compacted/HookStarted/HookProgress/HookFinished 七臂删除、include_partial 守卫、build_result 四个 usage fallback `>0` 守卫三兄弟 ×4 站点、`llm_latency_ms>0`、`steps>0`(连带 `num_turns` fallback)、println_json 源码钉)。**println_json**(进程 stdout 不可捕获)按 #429/#89db17c9 源码钉口径:`include_str!` 生产半区(首 `#[cfg(test)]` 前)内锚定 `Ok(line) => println!("{line}"),` 与 Err 臂,空白归一抗 reflow。

**7 条等价豁免(`.cargo/mutants.toml`,#223/#243 口径)**:①`:357` 大 drop 臂(TurnFinished|PartialToken|PartialReasoning|… 共 19 变体)与 ②`:381` WorkerEvent 臂——删除后落入 `#[non_exhaustive]` 捕获臂 `_ => Vec::new()`,行为逐字节同(PartialToken 只在 include_partial=false 时到达该臂,捕获臂同样丢弃);③-⑦ 五个 `FinishReason::{Cancelled,Stuck,TranscriptLimit,PermissionDenialLimit,ProviderStop}` 臂——各臂 error 字面量与 `src/agent/types.rs` Display **逐字节一致**(cancelled / stuck:{c}:{r} / transcript_limit:{chars}/{limit} / permission_denial_limit / provider_stop:{reason}),删除后 `_` 臂 `finish.to_string()` 同值,subtype/is_error 亦同。**Regex 手术(#228 教训当场复现)**:首版裸子串 pattern(`delete match arm FinishReason::Cancelled`)经 `--list` 自检发现会连带豁免 **output.rs 的 6 条同族可杀 mutant**(stream_events/stream_events_repl/print_finish_note/exit_for_finish 的 delete-arm)——已收紧:①②用 `\\{\\.\\.\\}` 区分裸臂与 output.rs 的 `{ref …}` 臂,③-⑦锚定 ` in ClaudeJsonEmitter::build_result` 尾缀。自检三件套全过:CI 31 条为 ground truth,7 pattern 各恰命中 1 条 claude_json 且 0 条其他文件;豁免后 `cargo mutants --list -p recursive-cli --features weixin` 恰少 7 条;24 条可杀名全数仍在单上。

**负控实证 22 条**:agent 6 红(:112×2/:114×2/:124-=/:128-=)+ 1 挂死实证(:124 `*=` 命名测试 90s 不完成 → cargo-mutants timeout=Caught)+ 3 红(dispatch/main/list 体替换,python 全函数体替换后集成/单测红)。cli 15 红(resolve `||`;七臂删除;usage/latency 比较 `>=`/`<`/`==`/`>=`;:434 `==`;:237 guard→true;:442 `>=`;println_json 体替换)+ **7 绿等价证明**(七豁免 mutant 手工应用 → claude_json 套件 18/18 保持绿 → 还原)。每次还原后 `cmp` 逐字节一致,工作树终态只含本批 4 文件。

**三门无管道全绿**:fmt --check EXIT=0 / clippy --workspace --all-targets --all-features -D warnings EXIT=0 / test --workspace 76 suites 5382 passed 0 failed(= 430 基线 75/5346 + 431 批 +20 + 本批 +16 测试、+1 suite 即 tests/eval_cli.rs)。

**红线自证**:kernel.rs/runtime.rs/run_core.rs/agent/(除只读 types.rs)、tools/、http_call.rs、a2a.rs、mcp.rs(#209)、hardening.rs 禁区、agui_session.rs(#152)零改动(git status 仅 4 文件);产品逻辑零改动(两 commit 全部 hunk 在 mod tests / 新测试文件 / mutants.toml 豁免段);#222 全部新测试零 env(子进程两路径不读 config);#224 零 cfg、include_str 锚点无 `\n` 依赖(CRLF 中性)、无 Path::join 平台分段;ubuntu-only 自查:新测试仅 std::process::Command + 退出码,无 unix 专属 API、无 ignore。本地 mutants 未跑(§2b);GitNexus 不可用,grep 代:println_json 调用方(main.rs 等)不受影响、dispatch/list 无其他调用方。

## 剩余条数与建议分段(共余 68+191+54 = 313)

- **agent 余 68**:B 批建议 `src/bin/recursive-eval.rs` 余 33 条(:164 split_list 族/:172/:189/:199/:276/:281-294 cmd_report 族/:314-315 write_file)——同文件同测试模块,预计一批;C 批 audit_log 14(:168/:268/:273/:283/:300/:301/:406/:432/:495/:615)+ agui_session 5(#152 禁区只读注意:变异钉需走集成测试,勿改产品);D 批 checkpoint 7(:196-200 ShadowRepo 族/:856 stderr_tail)+ compact/mod 9(:431/:527/:530/:534/:540)。
- **cli 余 191**:E 批建议 `cli/output.rs` 58(与 claude_json 同族 stream 事件,本批钉可复用断言模式);F 批 `cli/control.rs` 78(最大单文件,建议按 ControlBridge/permission/stdin-line/hook-forwarder 四段拆两批);G 批 `cli/init.rs` 28 + `cli/interrupt.rs` 3;H 批 otel.rs 14(main.rs 4 条建议与 otel 同批——main 体替换需子进程钉,本批 eval_cli 模式可复制)。
- **tui 54**:走 tui-mutants 通道(#228/#243 同款),建议独立批次由 tui 席认领。
- 备份:/tmp/b2-backups/goal245-batch1/(终态 4 文件、negctl 驱动脚本与挂死日志、--list 自检输出)。未 push、未动 main,待主控验收。
