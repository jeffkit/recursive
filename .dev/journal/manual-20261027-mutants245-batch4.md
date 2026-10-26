# 2026-10-27 — #245 批次 4:recursive-cli output.rs 流事件族(29 条)

- **Date:** 2026-10-27,交付行 [03:14](本机本地时钟)
- **Goal:** #245 批次 4 — run 40256596759 CLI 段,取 B2 批次 2 台账建议的分段:output.rs 流族 29 条(`stream_events` 23 + `stream_events_repl` 4 + `stream_events_json` 1 + `drain_events` 1);批次 5 留 output.rs 其余 29(print_usage/finish 族)
- **Files touched:** `crates/recursive-cli/tests/cli_output_streams.rs`(新,5 钉)、`crates/recursive-cli/src/cli/output.rs`(+33,仅 mod tests 的 drain 单测)、`.cargo/mutants.toml`(+36:1 真等价 + 4 结构性豁免)
- **Tests added:** 6(集成 5 + 进程内 1),+1 suite

## [03:14] 交付行

**结果一句话**:29 条中 **24 条钉死 + 5 条豁免**(1 真等价带本地判死证据 + 4 结构性不可达带逐条负控绿),产品逻辑零改动,三门无管道全绿。

**钉法转向(重要教训)**:先按"fd 级 dup2 重定向 + tokio worker 线程打印"做了进程内捕获钉——探针实证**当前工具链的 libtest 捕获是进程级的**:worker 线程的 `println!` 也被捕获转储、fd1 管道为空,进程内钉此路不通(与旧版 rustc 的线程局部捕获行为不同)。流族打印函数全部改走仓库既有的**子进程集成钉**:`tests/cli_output_streams.rs` 用按请求排队的裸 TCP stub provider(仿 `cli_signal_surfaces`,按 `/chat/completions` 路径分发,其余路径回空 catalog——run 启动期有非 completion 请求会抢脚本槽)驱动真二进制。

**五钉覆盖**:`run_human_stream_pins_…`(一次 run 四步:非空/空白 assistant、ToolCall、800/801 字节与多字节跨界 Read)钉 `:270` 体替换、`:283` 守卫 ×3、`:286/:294` 臂、`:300` 阈值 ×3、`:302:31 </==`、`:302:35/38`、`:303 ×2`、`:311/:314`(空白行负断言杀 with true;800 整字节全量 + 恰 2 个 truncated 标记杀 `==/>=`;`x`×799+`日` 杀边界回退族,`/=` 死循环由 SIGKILL 看门狗+缺行判死);`run_with_subagent…`(agent 工具单 worker)钉 `:274` WorkerEvent 臂(`[worker helper] …` 归流行);`resume_stream…` 钉 `:317` Compacted 臂(见下);`repl_pipe…`(stdin 管道驱动 repl)钉 `:359/:368/:371`(裸文本、空白负断言、stderr `↳` 提示);`repl_json…` 钉 `:388`;`drain_events_stays_pending…`(进程内,无打印不受捕获影响)钉 `:434`(tx 存活时 100ms 超时必须仍 pending,体替换立即返回即红)。

**`:317` Compacted 臂 = 批次内最深的可达性挖掘**:单轮 `run` 的压缩**在黑盒上不可达**——`safe_split_point` 遇 asst+tool_calls 消息逐级回退,整轮都是带调用 assistant → older 切片恒退化 → `plan()` 拒绝;microcompactor(>12 条 tool 消息才动)与 `maybe_trim_transcript`(硬预算修剪)都不会产生 LLM 摘要压缩。能触发压缩又走 `stream_events` 的唯一面是 **resume**(多轮历史含 plain assistant 收尾):手工种子会话(仿 cli_resume_surfaces,无 hash 走 pre-g151 警告路径)预置两轮 plain 收尾,resume 后 5 连读 400 字节落入 `[阈值, 硬限)` 窗口。另两个坑:stub 的 usage 必须报 0——非零 `last_prompt_tokens` 让 `should_compact` 走 token 阈值分支、字符预算永不生效;`RECURSIVE_COMPACT_THRESHOLD=4800` + `--max-transcript-chars 6500` 是实测窗口(基线 ≈3.1k 字符)。

**5 条豁免(mutants.toml,--list 自检 96→91 恰好 5 条,可杀同胞全在册)**:
- `:302:31 > → >=` **真等价**:usize 的 `end >= 0` 恒真,守卫退化为 `!is_char_boundary(end)`,与原式逐输入同值(`is_char_boundary(0)` 恒真,循环在 end==0 前必停);负控绿(全套件 6/6 过)+ 字节还原。
- **4 条结构性不可达**(`stream_events` 的 PlanProposed/PlanConfirmed/PlanRejected 三臂 + `stream_events_repl` 的 PlanProposed 臂):`run`/`resume`/`repl` 全部以 `interactive=false` 建 runtime,builder 不注册 enter/exit_plan_mode(builder.rs:2098 既有测试锚),plan 三事件无法进入这两台打印机;TUI/HTTP 宿主自渲染事件流。逐条删臂 → `cli_output_streams` 全绿 → 字节还原。
- 正则自检:等价条目带 `output\.rs:302:31:` 行列锚(`:300:47` 同名算子同胞不误伤,`--list` 实证);plan 族条目 `… in stream_events(_repl)?$` 尾锚,不吞可杀臂。

**负控 29 条**:24 红(每条恰由预期钉判死,exit 101)+ 5 绿,逐条 apply→run→字节还原(`filecmp` 断言),脚本/日志 `/tmp/b4-negctl/`。中途修了两个负控脚本自身缺陷:274 的测试过滤器错配(WorkerEvent 臂的钉在 subagent 钉不在人类流钉)、`arm_to_empty` 对 `PlanConfirmed =>`(单元变体无 `}` 收尾)拼出语法错误造成假红——均已修正重跑,最终账面 24/24 红 + 5/5 绿。

**三门无管道全绿**(日志 /tmp/b4-gates/):fmt --check EXIT=0 / clippy --workspace --all-targets --all-features -D warnings EXIT=0 / test --workspace EXIT=0,81 suites 5407 passed 0 failed 12 ignored(本批 +1 suite +6 钉)。

**红线自证**:`output.rs` 生产半区零改动(diff 全部 hunk 在 `mod tests`,+33 行);新增文件仅 `tests/cli_output_streams.rs`;`mutants.toml` 仅追加;#222 无 env 注入(子进程 `env_clear` + 显式 env;`RECURSIVE_COMPACT_THRESHOLD` 走子进程 env 参数);#224 新文件 `#![cfg(unix)]`、断言 contains/行匹配 CRLF 中性、路径 `Path::join`;ubuntu-only 自查:裸 TCP stub/SIGTERM/SIGKILL 均 POSIX,`libc::kill` 沿用 cli_signal_surfaces 先例;真缺陷零发现;本地 mutants 未跑(§2b,仅 `--list` dry-run);GitNexus 不可用,grep 手工核对 blast radius(output.rs 的 `stream_events*` 调用点:main.rs run/repl/resume,均未改动)。

## 剩余条数与下批分段(cli 余 107 + 残留 1;agent 68 / tui 54 不变)

- **cli 余 107 = output.rs 29(print_usage 22 + print_finish_note 3 + exit_for_finish 3 + JsonEventTask::finish_without_result 1)+ control.rs 78**。
  - **批次 5:output.rs 其余 29**(整文件收口;注意 print_usage/print_finish_note 是 eprintln 直打,捕获进程级教训同样适用——子进程钉或复用本批 stub;`exit_for_finish` 已有进程内单测先例可扩)。
  - **批次 6/7/8:control.rs 78 拆三批 ≈26**(ControlBridge/permission 回路/stdin 行协议/hook forwarder 四段按行号归并,B2 台账原建议不变)。
- **残留 1**(`main.rs:1169 skills_base` INERT):随 B2 拍板项 B 处置,不计入。
- agent 68 / tui 54:维持 B2 建议。
- 备份:/tmp/b4-negctl/(negctl.py、output.rs.orig、negctl.log)、/tmp/b4-gates/(三门日志)。未 push、未动 main,commit 留 wt-b1 待主控验收。
