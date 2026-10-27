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
