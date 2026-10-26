# 2026-10-27 — #245 批次(B2 席):recursive-cli 尾段 output.rs:13-145(28 条)

- **Date:** 2026-10-27,交付行 [05:1x]
- **Goal:** #245 — run 40256596759 MISSED 清偿,recursive-cli 尾段(剩余 136 条按行号降序的最后 ~30 条,与 B1 批次 4「正取中段」天然错开)
- **Files touched:** `crates/recursive-cli/src/cli/output.rs`(+247,单 hunk 全在 mod tests 尾部)、`.cargo/mutants.toml`(+15,1 条带名全串锚等价豁免)
- **Tests added:** 4(2 print_usage 子进程 stderr 钉 + print_finish_note 子进程钉 + exit_for_finish 纯断言钉)

## [05:1x] 交付行(wt-b2 席)

**结果一句话:#245 cli 尾段 28 条清偿 = 27 杀 + 1 上下文等价豁免(带名全串锚),产品逻辑零改动;三门无管道全绿(fmt EXIT=0 / clippy --workspace --all-targets --all-features -D warnings EXIT=0 / test --workspace EXIT=0:80 suites 5405 passed 0 failed);负控实证 28 循环(27 红 rc=101 + m20_43_ge 绿 rc=0 走全包 13 测试二进制等价证明),逐条字节还原断言全 True。**

**取段**:剩余 cli = control.rs 78 + output.rs 58(批次 1-3 已清 claude_json/init/workspace/会话族/main 首段/otel);按行号降序尾段 = output.rs 低行号区,函数边界切齐:**output.rs:13-145 共 28 条** = print_usage 22 + print_finish_note 3 + exit_for_finish 3。与批次 3 台账「下批主力」建议一致。

**打法**:exit_for_finish 三条纯函数,字节精确错误串断言即杀(臂删落到 `#[non_exhaustive]` wildcard 的 "agent finished with unknown reason" 文案必异)。print_usage/print_finish_note 走 stderr——**进程内不可捕获**:libtest `thread::add_spawn_hook` 让一切 std 新线程继承测试线程的 output-capture sink,eprintln 根本到不了 fd 2(library/test/src/lib.rs 实证;fd dup2、scope 新线程、raw pthread 三路全废,pthread 版 SIGSEGV 并诱发下述机器事故)。终版用**自派生子进程模式**(批次 1a CARGO_BIN_EXE 子进程钉同族):测试二进制 `current_exe()` 以 `--exact --nocapture` + `OUTPUT_PIN_CHILD=<case>` 重启自身,子进程同名测试直调真函数后 return,`--nocapture` 使 sink 不装、eprintln 直达真实 stderr,父进程捕获断言。跨平台零 cfg、零 unsafe、`env_clear`+显式 env(#222)、contains 断言 CRLF 中性(#224)。**methodology 附产:零修改 kill :35:42 `>=` 靠 avg 除零 panic 使子进程非零退出(`assert!(ok)` 红),非 stderr 内容。**

**杀伤明细(27)**:T1 usage-full 精确三行(`tokens: prompt=100 completion=50 total=150` / `cache: hit=8 miss=2 (80.0% hit rate)` / `llm latency: total=1000ms avg=250ms over 4 steps`)尽杀 :19 ±×、:21 两组 ×÷%、:36 ×%、:35 `<`/`==` 族 + 行存在性杀 :13/:18 `<`/`==`、:35:29 `<`/`==`、:35:42 `<`/`==`;T2 四 case(all-zero 杀 :13 `>=`;no-cache-hits 杀 :18 `>=`/`==` 且钉 cache 门独立性;zero-latency 杀 :35:29 `>=` 与 :35:33 `&&`→`||`;zero-steps 杀 :35:42 `>=`);T3 三 case(transcript-limit/cancelled 精确文案杀 :45 体替换与 :46/:52 臂删,other-silent 反断言);T4 三串杀 :134/:140/:145,附带 Ok 侧(NoMoreToolCalls/Cancelled/ProviderStop("end_turn"))与 Budget/WallClock/ProviderStop("rate_limited") 精确串契约钉。

**1 条上下文等价豁免(.cargo/mutants.toml)**:`:20:43 replace > with >= in print_usage`——:18 门保证 `cache_hit_tokens > 0`,故 `total_cache = hit + miss ≥ 1 > 0` 在一切可达状态成立,`>`≡`>=`;负控实证绿(全 recursive-cli 包 13 测试二进制全过)。**自检抓坑一次**:初版裸行号锚 `…output\\.rs:20:43:` 把同 line:col 的 `==`/`<` 两条**可杀同胞**(T1 下 0.0%≠80.0%,negctl 双红)一并扫出名单——收窄为带名全串锚 `crates/recursive-cli/src/cli/output\\.rs:20:43: replace > with >= in print_usage`;`--list -p recursive-cli` 对拍:无豁免 96 条含 3 条 20:43,有豁免 95 条含 2 条(==/< 保留在门禁视野)。

**负控实证 28 循环**(驱动 /tmp/b2-backups/245b3/negctl.py,逐条日志 /tmp/b2-negctl-245b3/,NEGCTL_EXIT=0,total=28 mismatched=0):27 条 rc=101 红,1 条(m20_43_ge)rc=0 绿;每循环还原后 filecmp 逐字节一致,零 MISMATCH。

## 机器事故与绕过(主控需知,B1 席同受累)

本地 03:20 pthread 版测试 SIGSEGV 后,**全机所有新 exec 的 x86_64 二进制**(rustup 全家 rustc/cargo/rustup、`cc -arch x86_64` 新产物)挂 U 态不可杀;arm64 原生与已驻留进程正常、文件读取 5GB/s 正常、无 quarantine、Rosetta 缓存翻译正常——定位为 **Rosetta 全新翻译(oahd)路径楔死**,等 1.5h 未自愈。**B1 席 `cargo test --workspace` 两个进程同楔(78002/83653),其门禁同被阻塞**,建议主控知悉/重启该机。绕过:`curl static.rust-lang.org/dist/rust-1.98.1-aarch64-apple-darwin.tar.xz` 自装 arm64 原生同版本工具链至 /tmp(PATH 前置,不动 rustup/全局),target/ 双 host 三版共存;cargo-mutants 27.1.0 无 aarch64 发行版,用该工具链 `cargo install --root /tmp/b2-cargo-mutants --locked --version 27.1.0 cargo-mutants` 自编译。**注意:本批三门与负控全部在 arm64 原生工具链(1.98.1,与 stable 同版本)下完成**;CI(ubuntu x86_64)仍是权威口径。

**红线自证**:`git diff -U0` 单 hunk `@@ -762,0 +763,247 @@` 全在 mod tests;kernel/runtime/run_core/agent/tools/http_call/a2a/mcp/hardening/agui_session/main.rs/interrupt.rs/resume.rs 零触碰(git status 仅 output.rs + mutants.toml + journal);#222 子进程 env_clear+显式 env、单测零 env;#224 零 cfg、零 include_str;ubuntu-only 自查:std Command/current_exe/退出码/stderr 管道,无 unix 专属 API(初版 libc/pthread 方案已整体废弃)。本地 mutants 未跑运行(§2b,仅 `--list`);GitNexus 不可用 grep 代:三函数调用方 resume.rs:1029/1035/1072、main.rs:3245/3655/3661/3849/3855,本批零产品改动。

## 剩余条数与下批分段(run 40256596759 口径,减已清偿)

- **recursive-cli 余 108 = control.rs 78 + output.rs 高行号族 30**(stream_events 23 + stream_events_repl 4 + stream_events_json 1 + drain_events 1 + JsonEventTask::finish_without_result 1,270-509;断言模式与批次 1 claude_json 同族可复用);残留 1(main.rs:1169 skills_base,拍板项 B 不计)。
- **agent 余 68**(批次 1 台账建议维持);**tui 54** 走 tui-mutants 通道。
- 备份:/tmp/b2-backups/245b3/(negctl.py、output.rs.orig/.with-tests、wip-tests.patch、mutants.toml.orig/.with-entry、三门 gate-*.log、list-*.log、arm64 工具链 /tmp/b2-rust-aarch64、arm64 cargo-mutants /tmp/b2-cargo-mutants);negctl 循环日志 /tmp/b2-negctl-245b3/。未 push、未动 main,commit 留 wt-b2 待主控验收。
