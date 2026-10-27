# 2026-10-27 — #245 批次 5:recursive-cli output.rs 其余 29 条(整文件收口)

- **Date:** 2026-10-27,交付行 [08:40](本机本地时钟;主控注入单写 13:5x 为另一口径)
- **Goal:** #245 批次 5 — run 40256596759 CLI 段,取 B1 批次 4 台账指定分段:`cli/output.rs` 流族之外的全部 **29 条**(`print_usage` 22 + `print_finish_note` 3 + `exit_for_finish` 3 + `JsonEventTask::finish_without_result` 1);与批次 4 的 29 条合计 58 = output.rs 全部 MISSED,**整文件收口**
- **Files touched:** `crates/recursive-cli/src/cli/output.rs`(+197,全部在 `mod tests`:3 处既有测试各加 1 条精确串断言 + 新增 10 个测试 fn + 1 helper + 1 const)、`.cargo/mutants.toml`(+24:1 真等价豁免)
- **Tests added:** 10 新 fn(进程内 2 + 自派生父测 4 + 子进程用例 4)+ 3 处既有测试加固;suites 不变

## [08:40] 交付行

**结果一句话**:29 条中 **28 条钉死 + 1 条真等价豁免**(带本地判死证据),产品逻辑零改动(`output.rs` 生产半区与 HEAD md5 一致),三门无管道全绿,负控 29/29 如预期。

**清单取法**:`gh run view 40256596759 --log | grep MISSED` 落盘后剥 ANSI,取 `output.rs` 全部 58 条,剔除 `in stream_events(_repl|_json)?$` / `in drain_events$` 及三条流族函数体替换(`:270/:388/:434`,批次 4 已钉),余 **29 条**与 B1 台账逐函数吻合。

**exit_for_finish 3 条删臂为何幸存(根因)**:既有 `exit_for_finish_{stuck,transcript_limit,permission_denial_limit}_errors` 全是 `contains` 断言;删臂后落入 `_ => bail!("agent finished with unknown reason: {finish}")`,而 `FinishReason` 的 Display 恰为 `stuck:Read:3` / `transcript_limit:100000/80000` / `permission_denial_limit`——三组子串("stuck"/"Read"/"3"、"transcript"/"100000"/"80000"、"permission")**全部命中**,contains 断言对删臂盲。钉法:三处各加一条 `assert_eq!(msg, <完整消息>)`(保留原 contains 行作文档),删臂即红。

**finish_without_result 1 条**(进程内):`JsonEventTask::Legacy(tokio::spawn(stream_events_json(rx)))` 持 tx 不放,`timeout(100ms, finish_without_result())` 必须仍 pending(体替换 `()` 立即返回即红),drop(tx) 后 5s 内必完——同模块批次 4 `drain_events_stays_pending…` 先例,无打印不受捕获影响。

**print_usage 22 + print_finish_note 3 = 纯 `eprintln!` 打印机,钉法转向(本批最重要的新钉法)**:B1 批次 4 实证本工具链 libtest 捕获是进程级(worker 线程输出亦被捕获,fd 级 dup2 看到空管道),进程内不可观测;B1 用真二进制 + stub 走集成钉,但本批需要的输入组合(`total_tokens==0`、`cache_hit==0 && total>0`、**`latency==0 && steps>0`**、`latency>0 && steps==0`)真跑 run 要么定时非确定(本地 stub 的 llm_ms 0/1ms 抖动)要么不可达(有 latency 必有 step)。改走**自派生子进程**:父测用 `std::env::current_exe()` 重新拉起**本测试二进制**,参数 `--exact <child> --nocapture`,子测是普通 `#[test]` fn(全量跑时无害地跑一遍,输出被捕获)直接以**任意确定参数**调 `print_usage`/`print_finish_note`;`--nocapture` 下 harness 行走 stdout、`eprintln!` 直落 fd 2,父测 `Command::output()` 读 stderr 做**整行精确比对**。全 std,无 unix-ism,无新 dev-dep;子进程 `.env("RECURSIVE_HOME", tempdir)` 钉住 `pricing_for` 的 providers.d 查找面(模型名 `mutants-245-unpriced-model` 在 bundled 目录无价,保证无 "cost:" 行;providers.rs 加载路径零 eprintln 已核)。

**踩坑与空洞守卫(写进 helper,必读)**:首版用 `concat!(module_path!(), "::child")` 拼测试名——bin 包里 `module_path!()` 是 `recursive::cli::output::tests`(**带 crate 根段**),而 libtest 测试名是 `cli::output::tests::…`,`--exact` 匹配 **0 条**("running 0 tests"、exit 0、stderr 空)→ 三条正向父测红,但**零输出用例的父测"假绿"**(期望空、得到空)。修法两条:① `module_path!().split_once("::")` 剥根段再拼;② helper `child_stderr` **硬断言子进程 stdout 含 `"1 passed; 0 failed"`**——名字错(0 passed)或子进程在变异下 panic(1 failed)都必须响亮失败,杜绝"没跑=没打印"的空洞。`--list` 复核 13 个新名字全在 `cli::output::tests::` 下。

**四个子用例与各自判死面**:
- `usage_child_full_positive`(prompt=60/completion=40/total=100,**hit=30/miss=70**,latency=1000ms,steps=4)→ 父测期望恰 3 行 `tokens: prompt=60 completion=40 total=100` / `cache: hit=30 miss=70 (30.0% hit rate)` / `llm latency: total=1000ms avg=250ms over 4 steps`。hit<miss 使 `:19 +→-` 在 debug 下溢 panic(release 回绕得 0.0% 同样红,profile 无关);`+→*` 1.4%;`:20 ==/<` 0.0%;`:21:48 /→%` 3000.0%、`/→*` 300000.0%;`:21:70 *→+` 100.3%、`*→/` 0.0%;`:18 ==/<`、`:35:29 ==/<`、`:35:42 ==/<` 各少一行;`:36 /→%` avg=0ms、`/→*` avg=4000ms(steps=4 而非 1,否则 `*` 等价)。**16 条**。
- `usage_child_all_zero`(全 0,latency=0,steps=5)→ 期望 stderr 空:杀 `:13:27 >=`(打出 total=0 行)与 `:35:29 >=`(`0>=0 && 5>0` 打出 total=0ms 行)。**2 条**。
- `usage_child_tokens_without_cache_or_latency`(total=15,hit=miss=0,latency=0)→ 期望恰 1 行 tokens:杀 `:18:35 >=`(多出 `cache: hit=0 miss=0 (0.0% hit rate)`)。**1 条**。
- `finish_note_child_transcript_limit_then_cancelled` → 期望恰 2 行 `note: stopped because transcript reached 123456 chars (limit 100000)` / `shutdown: agent stopped at next step boundary after signal`:杀 `:45:5` 体替换(0 行)、`:46:9`/`:52:9` 删臂(各少一行)。**3 条**。
- 进程内 `print_usage_zero_steps_never_divides`(latency=100,steps=**0**):原码守卫 false 直接返回;`:35:33 &&→||` 与 `:35:42 >→>=` 都让守卫为真 → `100 / 0` **整数除零 panic**,钉的是"返回"而非打印,不受捕获影响。**2 条**(真 run 不可达的组合,恰是这两条只能在进程内杀的原因)。

**1 条豁免(mutants.toml,`--list` 自检 91→90 恰 1 条,diff 即该条)**:`:20:43 > → >=`(`let hit_rate = if total_cache > 0`)**真等价**——守卫在 `if usage.cache_hit_tokens > 0` 之内,`total_cache = cache_hit + cache_miss`(u32),可达输入上 `total_cache >= cache_hit > 0`,原式与 `>= 0`(无符号恒真)逐输入同值,`else { 0.0 }` 是防御性死代码;debug 下加法溢出先 panic 于比较之前,release 回绕需 provider 在报 cache hit 的同时报 ~2^32 cache miss,无 fixture 可构造。同 B1 `:302:31` 类别。行列锚 `output\\.rs:20:43:` 保住四个**可杀** `>→>=` 同胞(`:13:27`/`:18:35`/`:35:29`/`:35:42`,全部已钉,`--list` 实证仍在册)。

**负控 29 条**(`/tmp/b5-negctl/negctl.py` + `negctl.log`,每条 apply → **仅跑指定那一条钉** → `filecmp` 字节还原断言):**28 红**(exit 101,每条 "0 passed; 1 failed" 恰由预期钉判死,`compile_err=False` ×29 → 全是真测试失败而非编译失败)+ **1 绿**(`:20:43 >=` 下 `cli::output::tests` 30/30 过)。中途一处脚本自检:首个 `cargo mutants --list` 从 crate 目录带相对 `-f` 得 0 条——从工作区根 `-p recursive-cli -f crates/…/output.rs` 才是正确口径(91 基线与 B1 吻合)。

**三门无管道全绿**(日志 `/tmp/b5-gates/{fmt,clippy,test}.log|.exit`,各门 `;` 隔开独立取码):fmt --check EXIT=0 / clippy --workspace --all-targets --all-features -D warnings EXIT=0 / test --workspace EXIT=0,**81 suites 5417 passed 0 failed 12 ignored**(= 批次 4 的 81/5407 + 10 fn,对账一致)。

**红线自证**:`git status` 仅 2 路径;`output.rs` 以首个 `#[cfg(test)]` 切生产半区,HEAD 与工作树 **md5 同为 db0e48ed…**,`git diff -U0` 四个 hunk 全在 `mod tests`(:619/:643/:650/:796 之后);`mutants.toml` 仅追加;产品逻辑零改动,真缺陷零发现。#222:无进程内 `set_var`,子进程 env 仅显式 `RECURSIVE_HOME`=tempdir(走子进程 env 参数,B1 同口径),无定时/负载敏感断言(`finish_without_result` 的 100ms 窗口只断言"未完成",负载只会让它更不完成);#224:纯 std 跨平台(`current_exe`/`Command`/`--exact`/`--nocapture` 三平台通用),断言走 `str::lines()`(CRLF 中性)与 `trim().is_empty()`,不新建文件故无 `#![cfg(unix)]` 之需;ubuntu-only 自查:无 libc/pty/signal,无平台差异面。本地 mutants 未跑(§2b,仅 `--list` dry-run ×2);GitNexus MCP 不可用,grep 核对 blast radius:四个被钉函数的调用点(main.rs run/repl/resume 路径)零改动,被改的只有测试。

## 剩余条数与下批分段(cli 余 78 + 残留 1;agent 68 / tui 54 不变)

- **前席遗留(勿双计)**:本席上一化身 04:48 曾交付 `fa4ac7b2`(同段但只 28 条,未含 `:509 finish_without_result`;同为自派生子进程钉法),已被本单开工基线重置(`reset --hard origin/main`)变成**悬空 commit**(`git branch -a --contains` 为空,其台账 `manual-20261027-mutants245-b2-cli-tail.md` 随之不在树上;备份 /tmp/b2-backups/245b3/)。主控 13:5x 注入时 B1 批次 4 台账仍列 29 条待清,说明前次未被验收。**本 commit 取代之**,验收只认本条。
- **本批取段**:`output.rs` 其余 29(整文件收口,output.rs 58/58 清偿:批次 4 流族 29 + 本批 29)。
- **cli 余 78 = `control.rs` 78**(整个 CLI 段只剩这一个文件;核对:107 − 29 = 78 ✓)。
  - **批次 6/7/8:control.rs 78 拆三批 ≈26**——ControlBridge / permission 请求回路 / stdin 行协议 / hook forwarder 四段按行号归并(B2 批次 2 台账原建议,B1 批次 4 维持,本席维持)。control.rs 若也是 `println!`/`eprintln!` 直打面,**本批的自派生子进程钉法可直接复用**(`child_stderr` helper 在 output.rs tests 内,搬到 control.rs tests 或抽到 `test_env.rs` 共享皆可——建议后者,避免两份 module_path 剥根逻辑)。
- **残留 1**(`main.rs:1169 skills_base` INERT):随 B2 拍板项 B 处置,不计入。
- agent 68 / tui 54:维持 B2 建议。
- 备份:`/tmp/b2-backups/245b5/`(`wip-tests+toml.patch`、`output.rs.orig`/`.pinned`、`mutants.toml.orig`、`list-before/after.txt`、probe 日志、CI run log、negctl 副本)、`/tmp/b5-negctl/`(`negctl.py`/`negctl.log`/`output.rs.pinned`)、`/tmp/b5-gates/`(三门 log/exit)。未 push、未动 main,commit 留 wt-b2 待主控验收。
