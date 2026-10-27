# 2026-10-28 — #245 批次 8 合并落位(B1 席):recursive-cli control.rs 前半(:1043→:1159,B1)+ 收口段后半(:1190→:1302,B2)同文件真合并

**[17:2x] 合并交付行(wt-b1 席)**:主控注入「批次 4 前段落位合并」(轮值重编号,按台账口径 = **批次 8 前半**;原验收门作废,合并任务回本席)。基线按简报明示 sha **f9a346e2**(B2 席批次 8 后半;注:GitHub origin/main 实仍在 dcc1e6b8 未含该提交,f9a346e2 是主 checkout 上未 push 的本地 main,照 sha 执行)→ `git cherry-pick 70db6db4`(本席前半原 commit,wt-b1 reflog 实得;与简报备选 8a2048a7..482260ac 内 70db6db4 同一对象)。冲突 = control.rs 3 块 + journal add/add:**control.rs 不按冲突块逐块取两**——两侧均为同一锚点(祖先 :2302 之后、`mod tests` 收尾 `}` 之前)的纯插入,git 沿伪公共行(`#[tokio::test]`/空行/`}`)把区域切成 3 块,逐块"两边都要"会交错切碎函数体;改为自 git 对象确定性重建 = 祖先 dcc1e6b8 :1-2302 + **B1 块** 70db6db4 :2303-2444(142 行)+ **B2 块** f9a346e2 :2303-2533(231 行)+ 祖先 :2303(收尾 `}`),共 2676 行;两块 `cmp` **逐字节原位**(B1 :2303-2444,B2 :2445-2675),括号 514/514 平衡,零冲突标记,落位序按原始行段(前半在前、收口段在后)。journal 两席原文**逐字节全保留**(下方两节,可 `git show <sha>:<file>` diff 对账)。`cargo fmt --all` **零改动**(两块各自本就 fmt 干净,缝口无归位需求)。**三门无管道全绿**(冻结终树,私有 target `/Users/kong/target-arm64-b1b8`,日志 `/tmp/b1-merge-gates/{fmt,clippy,test}.log|.exit` 独立取码;工具链 cargo/rustc 1.99.0 aarch64 + clippy 0.1.99 b940084d7 与 rustc 同源,rustfmt 1.9.0-stable 与两半批同尺):fmt --check EXIT=0 / clippy --workspace --all-targets --all-features -D warnings EXIT=0(真实复查变更的 recursive-cli 后 Finished)/ test --workspace EXIT=0 **81 suites 5450 passed 0 failed 12 ignored**(= B2 后半基线 5444 + 本席前半 6;**两侧 12/12 新测试按名 ok 在列**——B2 后段 6 条原样、B1 前段 6 条原样;`cli::control::tests` 43 ok = 批次 7 的 31 + 6 + 6 对账一致)。生产半区零改动(两侧原始 diff 均纯 `mod tests` 追加,重建仅重排插入序,祖先生产半区逐字节未动)。一个 commit 留 wt-b1,信息注明两侧来源;未 push、未动 main,待主控验收。

---

# 2026-10-28 — #245 批次 8(后半,B2 席):recursive-cli control.rs 收口段 :1190 → :1302(forward 9 + elicit 2 = 11 条)

- **Date:** 2026-10-28,B2 修复席;主控注入单称「批次 6(未清段后半)」,按仓内台账口径实为批次 7 预留的**批次 8 收口段后半**(主控批次号与台账错位,已在此对齐)
- **Goal:** run 40256596759 CLI 段收口:B1 并行取前半(`handle_stdin_line` 4 + `parse_permission_result` 6 + `discover_loaded_skills_for_workspace` 1,:1043→:1159),本席取**行号序后半 11 条** = `SdkHookForwarder::forward` 9(`:1190`×2 / `:1206-1211` 六臂 / `:1237` delete !)+ `ControlElicitationHandler::elicit` 2(`:1302`×2)
- **Files touched:** `crates/recursive-cli/src/cli/control.rs`(+231,全部在 `mod tests`:6 个新测试 fn + 1 泛型 helper + 2 构造 helper)
- **Tests added:** 6,`cargo test --workspace` 81 suites 5444 passed(= 批次 7 的 5438 + 6,suites 不变,对账一致)

## 交付行(本批取段 + 剩余)

**结果一句话**:取段 11 条 → **11 条全部钉死、零豁免**,负控 11/11 判死;recursive-cli 台账口径余 **11 = B1 前半半区**(若 B1 同批交付则为 0)+ INERT 残留 1(`main.rs:1169 skills_base`,随批次 2 拍板项 B 处置,不计工作量);产品逻辑零改动(生产半区 md5 `41500a98…` 与开工基线逐字节一致)。

**钉法(全进程内,复用批次 6/7 helpers)**:
- **`forward` 体替换 2 条**:`:1190 → None` 由 `sdk_hook_forwarder_surfaces_the_host_callback_result` 杀(camelCase 直键 `preToolCall` 注册 → `answer_single_waiter` 式回包 `{"action":"block","systemMessage":…}` → 必须 `Some(Skip)` 且逐字段);`:1190 → Some(Default::default())` 由 `sdk_hook_forwarder_without_registered_callbacks_returns_none` 杀(空注册表 → 必须 `None` 落本地 hooks,`Some(Continue-default)` 即红)。
- **六臂删除 6 条**(`delete match arm "preToolCall"/…/"sessionEnd"`):单测 `sdk_hook_forwarder_resolves_pascalcase_aliases_for_all_wired_events` 循环六事件,各只按 **PascalCase 别名**注册(`PreToolUse`/`PostToolUse`/`PermissionRequest`/`UserPromptSubmit`/`SessionStart`/`SessionEnd`)——直键必 miss、逼走别名臂;臂删除即落 `_ => None` → 该迭代 parked 超时红。回包用 `{"decision":"deny","reason":…}` 形(顺带钉 `hook_result_from_host` 的 decision 半形,Error 逐字断言)。
- **`:1237 delete !`**:`sdk_hook_forwarder_keeps_asking_after_a_continue_answer` 双回调 `["cb_first","cb_second"]`,首包 `continue` 后必须继续问第二个(其 `block` 裁决胜出)——变异把早退反转成「见 Continue 即退」,首包后即返 `Some(continue)`,第二次 parked 超时红。单回调测试对该变异天然盲(早退与否可观察值相同),双回调是唯一杀点。
- **`elicit` 体替换 2 条**:`→ None` 由 `elicitation_handler_surfaces_the_host_answer` 杀(真 `control_response` 回包必须 `Some` 且逐字段);`→ Some(Default::default())` 由 `elicitation_handler_returns_none_when_the_waiter_is_dropped` 杀——从 `bridge.pending` **取走 waiter 令 oneshot 关闭**,`ask_host` 的 `rx.await.ok()` 必须得 `None`,不得伪造默认答复(变异即不注册 waiter → parked 超时红)。
- 新 helper `answered<T>`:批次 6 `answer_single_waiter` 硬编码 `Option<Value>`,而 `forward` 产出 `Option<HookResult>` —— 泛型化同一「parked→回包→join→pending 清空」契约。`SdkHookForwarder`/`ElicitationHandler` 以 `as _` 匿名引入测试半区。

**负控 11/11**(`/tmp/b2-backups/245b8/negctl.py` + `negctl.log`,行号级手工变异:体替换=签名后插 return、删臂=行删除、delete != 行内 `!matches!`→`matches!`;指定杀手过滤,基线先证绿):**11/11 KILLED,compile_err=0**,六臂与 delete ! 均为单杀手精确判死,体替换四条另有连坐红(预期内),每次还原 `filecmp` 字节断言 True。

**⚠ 新故障模式(本批实证,建议主控收录 AGENTS.md known-failure-modes)**:**cargo 按 mtime 判新鲜度——`shutil.copy2` 还原变异会把 ORIG 的旧 mtime 一起还原,cargo 视源码「未变」而复用最后一个变异的脏二进制**。症状:源码逐字节干净、测试却持续红(本批:elicit 两条「never registered」假红,单线程仍红,恰为负控最后一次变异 elicit→Some(Default) 的行为)。负控脚本与任何「手工变异+还原」流程必须在还原后 `os.utime(SRC, None)` 强制 bump(修复后 touch 即 6/6 复绿)。批次 7 的 filecmp 字节断言防的是字节漂移,防不了这个。

**三门无管道全绿**(冻结终树复跑,日志 `/tmp/b2-backups/245b8/gates/{fmt,clippy,test}.log|.exit` 独立取码):fmt --check EXIT=0 / clippy **--workspace** --all-targets --all-features -D warnings EXIT=0 / test --workspace EXIT=0 **81 suites 5444 passed 0 failed**。

**红线自证**:`git status` 仅 `control.rs`;`git diff` 单文件 +231 全在 `mod tests`(生产半区 md5 `41500a98…` 与开工基线一致);真缺陷零发现。#222:无进程内 `set_var`(仅 `std::env::temp_dir()` 读取);#224:无 include_str!/源码自匹配钉,断言全 sort-free 精确 eq/matches!,无 libc/pty/signal,5s timeout 只上界「必须完成」之事;ubuntu-only 自查:纯 tokio/std/tempfile 件,windows dead_code 无新面(无 cfg 门代码)。本地 mutants 未跑(§2b,仅手工变异负控)。GitNexus MCP 不可用,grep 代 blast radius:被钉两结构仅 `main.rs:3429/3435`、`resume.rs:832/837` 构造安装,零改动;改动仅测试。

## 剩余条数(cli 收口状态)

- **本批取段**:control.rs `:1190` → `:1302` 行号序 11 条(forward 9 + elicit 2),**零豁免**。
- **剩余 11 = control.rs `:1043` → `:1159`(B1 前半半区)**:`handle_stdin_line` 4(`:1043`/`:1046`/`:1060`×2)+ `parse_permission_result` 6(`:1138`×3/`:1141`×3)+ `discover_loaded_skills_for_workspace` 1(`:1159`)——stdin 线/permission 半区按主控分单归 B1 并行;批次 7 已预留 helpers(`eventually_parked_request_ids`/`answer_the_parked_waiter`)可直用。B1 交付后 recursive-cli CI 口径应归零(另余 INERT 残留 1 随拍板)。
- agent 68 / tui 54:维持批次 2 建议(tui 走 tui-mutants 通道由 tui 席认领)。
- 备份:`/tmp/b2-backups/245b8/`(`negctl.py`/`negctl.log`/`negctl-run.log`、`control.rs.orig|.nowork|.final`、`wip-tests.patch`/`final-tests.patch`、`gates/` 三门 log+exit、`targeted-first.log`)。未 push、未动 main,commit 留 wt-b2 待主控验收。

---

# 2026-10-28 — #245 批次 8 前半(B1 席):recursive-cli control.rs :1043 → :1159 共 11 条

- **Date:** 2026-10-28,交付行 [16:2x](本机本地时钟)
- **Goal:** #245 — run 40256596759 MISSED 清偿。主控注入单称「批次 5」(轮值重编号),按既有分段序列即**批次 8 前半**:批次 1-7 后 recursive-cli 未清段仅剩 control.rs 22(批次 7 台账预留段)+ `main.rs:1169` INERT(批次 2 已做待发版豁免,不计入)。本批按主控指令取**前半 11 条**,后半 11 条留给并发 B2 席错开。
- **Files touched:** `crates/recursive-cli/src/cli/control.rs`(+142,全部在 `mod tests`:6 个新测试 fn + 1 个 helper)
- **Tests added:** 6(`cargo test -p recursive-cli --bin recursive control::` 37 passed = 31 + 6,对账一致)

## [16:2x] 交付行(wt-b1 席)

**结果一句话**:取段 **11 条**(stdin 4 + parse 6 + discover 1)→ **11/11 全钉死、零豁免**;CLI 未清段仅剩 control.rs 后半 11 条(→ B2)与 `main.rs:1169` INERT;产品逻辑零改动(生产半区 md5 `41500a98…` 与开工基线一致),负控 11/11 判死,三门无管道全绿(私有 target 目录;clippy 初红系工具链缺 clippy 组件落到异构 0.1.98 所致,已定位修复并以匹配 0.1.99 真跑过,详见「环境事故」)。

**清单取法(全量对账)**:`/tmp/b7-mutants-full.log`(run 40256596759 MISSED 落盘)递归 CLI 共 **222** 条 = claude_json 31(批1)+ 会话族 resume 5 / interrupt 3 / workspace 1(批2)+ main.rs 首段 3(批2,:963/:1168/:1170)+ otel 14(批2)+ init 28(批3)+ output 58(批4+5)+ control 78(批6 前段 30+附赠 1、批 7 后段 25、**本批前半 11**、余 11→B2)。本批 11 条 = CI 清单 control.rs 行号序 `:1043 / :1046 / :1060×2 / :1138×3 / :1141×3 / :1159`,与批次 7 台账预留「批次 8」前半逐条吻合;**B2 后半 11 条 = `:1190×2 / :1206-:1211 六臂 / :1237 / :1302×2`**(forward 9 + elicit 2)。

**各家族钉法(全部进程内 + tempdir,无子进程钉)**:
- **handle_stdin_line 4 条**(`:1043` 删 control_response 臂 / `:1046` 删 control_cancel_request 臂 / `:1060` guard→true / guard→false):spawn 真 `ask_can_use_tool` 落 waiter(`parked_request_ids` 纯 yield 轮询够用——注册在首次 poll 内完成,无需 200ms tick),再直接调 `handle_stdin_line` 喂真 wire 帧。control_response 帧断言 ask 返回 `Some({"behavior":"allow"})` 且 `pending` 清空(删臂→waiter 永不 resolve→5s timeout 红);cancel 帧断言返回 `{"behavior":"deny","message":"cancelled by host","interrupt":true}` 逐字段全等(cancel_pending 语义,删臂→timeout 红);user 门用新 helper `session_with_accept_flag` 双相——flag=false 喂 `{"type":"user","message":{"content":"hello stdin"}}` 断言 FIFO 空(guard→true 红)、flag=true 断言 FIFO 恰一条(guard→false 红)。单钉双相杀双 mutant。
- **parse_permission_result 6 条**(`:1138:14` g1→true/false、`:1138:62` `==→!=`、`:1141:14` g2→true/false、`:1141:62` `==→!=`):三个输入的全 variant 断言构成杀手矩阵——`{"allowed":true}`→`Allow`(杀 g1→false、`==→!=`:变异体落 `_` 臂 "unrecognised…");`{"allowed":false}`→`Deny("denied by host")` 逐字(杀 g2→false、`==→!=`);`{"allowed":false,"reason":"nope"}`→`Deny("nope")`(g2 措辞走 reason 缝);`{}`→`Deny` 且前缀 `unrecognised permission response: `(杀 g1→true、g2→true:分别变异成 `Allow` / `Deny("denied by host")`)。`PermissionDecision` 带 `PartialEq`,直接 `assert_eq!`。
- **discover_loaded_skills_for_workspace 1 条**(`:1159` 体替换 `vec![]`):tempdir 植 `.recursive/skills/alpha/SKILL.md` + `.claude/skills/beta/SKILL.md`(discover_skills 无 frontmatter 时以目录名为 name),断言返回按 **name contains** 双命中——断 contains 而非全等,是因函数还会拼 `$HOME/.recursive|.claude/skills`,开发机/CI 的真实 HOME 不污染判死(体替换→空表→必红)。

**负控 11 条**(`/tmp/b8-negctl/negctl.py` + `negctl.log`,行级手工变异按 cargo-mutants 语义:删臂=区间删除、guard=行内替换、`==→!=`=列位替换、体替换=签名后插 `return vec![];`;每条只跑其指定杀手单测,**判死甄别 = `test result: FAILED`,编译错单独分类不计红**;还原 filecmp 字节断言 11/11 True):终局 **11/11 KILLED,compile_err=0**。首跑 0/11 全「COMPILE_ERR」为**负控脚本自身判据错**——cargo test 失败尾部本就打 `error: test failed`,被 `error:` 前缀误吞;改判据 `could not compile`/`error[E` 后重跑即 11/11,非钉盲。

**环境事故(如实留痕,非本批代码问题;根因已定位并修复)**:clippy 门初跑即红,依赖单元 `cpufeatures 0.3.0` / `ring 0.17.14` 报 `libc` 未链接(E0433/E0432),且**私有 target 目录同样复现**——确定性缺陷,非并发竞争。证据链:`cargo check -v` 的 cpufeatures rustc 命令行带 `--extern libc=…rmeta`,同 flags 的 `cargo clippy -v` **完全没有**该边(`--extern` 判零);`cargo check -p libc` / `-p recursive-agent --all-features --tests` / 最小工程隔离探针(cpufeatures 0.3.0 与 0.3.1)全 EXIT=0;test 门只编 cpufeatures 0.2.17(默认 feature 图根本不碰 0.3.0)。**根因:aarch64 工具链 `/Users/kong/rust-aarch64/toolchain` 未装 clippy 组件**(bin 仅 cargo/rustc/rustdoc),`cargo clippy` 沿 PATH 落到 `~/.cargo/bin` 的**异构 clippy 0.1.98(x86_64,rustc 1.98 时代)**;cargo-clippy 复 invocate 旧 cargo 做解析,旧 cargo 对 `target_vendor = "apple"` 新式 target-gated 依赖边求值丢失 → `--extern libc` 未传。**修复:PATH 前置 `/Users/kong/rust-aarch64/extract/rust-1.99.0-aarch64-apple-darwin/clippy-preview/bin`(clippy 0.1.99,哈希 b940084d7 与 rustc 1.99.0 严格同源)+ `DYLD_LIBRARY_PATH=/Users/kong/rust-aarch64/toolchain/lib`(clippy-preview 二进制依赖该处 librustc_driver dylib)**,clippy 重跑真 lint 45 秒 EXIT=0 零警告。附带发现:批次 4-7 台账的 clippy「绿」实为 ~6 秒缓存空跑(批次 7 日志 `/tmp/b7-gates/clippy.log` 仅 2 行且Checking 的是 wt-b2 路径),本批是近期首次真 clippy 全量过——工作区无陈欠 lint。另观察到位 x86_64 cargo test 循环(ps 实录 12+ 个连续 PID,25-45 秒一次,stable-x86_64 工具链)写共享 `target-arm64` 目录,与本红无因果但建议主控转相关席位纠正(工具链注入单遵行 + 共享 target 目录卫生)。

**三门无管道全绿(私有目录 `/Users/kong/target-arm64-b1b8`,冻结终树,日志 `/tmp/b8-gates/{fmt,clippy,test}.log|.exit` 独立取码)**:fmt --check EXIT=0 / clippy --workspace --all-targets --all-features -D warnings EXIT=0(匹配 0.1.99 真跑)/ test --workspace EXIT=0,**81 suites 5444 passed 0 failed**(= 批次 7 的 5438 + 6,对账一致)。

**红线自证**:`git status` 仅 `control.rs`;`git diff` 单段纯 `mod tests` 追加,生产半区 md5 `41500a98…` 与开工基线一致;真缺陷零发现。#222:无进程内 `set_var`(仅读 `HOME`/`std::env::temp_dir()`);#224:无 include_str!/源码自匹配钉,断言全 variant 全等/逐字/contains;无 libc/pty/signal,5s timeout 只上界「必须完成」之事;ubuntu-only 自查:tempfile/std::fs/serde_json/tokio 常规件,无 `cfg(unix)` 之需,`discover` 钉对 HOME 额外技能 contains 容忍,CI ubuntu 亦稳。本地 mutants 未跑(§2b)。GitNexus MCP 不可用,grep 核 blast radius:被钉 3 函数调用点(`stdin_read_loop` 喂线 / `StdioPermissionHook::check` / builder init+reload_skills)零改动,改动仅测试。

## 剩余条数与下批分段

- **本批取段**:control.rs `:1043` → `:1159` 行号序 11 条;**recursive-cli 余 = control.rs 后半 11(`:1190×2 / :1206-:1211 / :1237 / :1302×2`,B2 并发在办)+ `main.rs:1169` INERT 1(批次 2 待发版豁免,随 B2 拍板项 B 处置,不计入)**。
- CLI 段至此对账闭合(222 = 已清 199 + 本批 11 + B2 11 + INERT 1);agent 68 / tui 54 维持 B2 建议,不在本席范围。
- 备份:`/tmp/b8-negctl/`(`negctl.py`、`negctl.log`、`control.rs.pinned`)、`/tmp/b8-gates/`(三门 log/exit,私有目录)、`/tmp/b8-repro-af.log`、`/tmp/b8-libc.log`(环境事故取证)。未 push、未动 main,commit 留 wt-b1 待主控验收。
