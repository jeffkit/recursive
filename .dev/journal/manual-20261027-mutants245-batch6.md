# 2026-10-27 — #245 批次 6:recursive-cli control.rs 前段 30 条(98 → 798)

- **Date:** 2026-10-27,交付行 [09:4x](本机本地时钟;主控注入单写 15:0x 为另一口径)
- **Goal:** #245 批次 6 — run 40256596759 CLI 段,cli 仅剩 `control.rs` 78 条,本批取**按行号前 30 条**(`:98 ask_hook_callback` → `:798:8 delete !`);`register_repo_root` 钉**顺带判死第 31 条** `:798:37 ==→!=`(负控红),实际清偿 31
- **Files touched:** `crates/recursive-cli/src/cli/control.rs`(+418,全部在 `mod tests`:1 处既有 `permission_mode_roundtrip` 加 `"strict"` 一行 + 14 个新测试 fn + 4 个 helper)、`.cargo/mutants.toml`(+22:1 条豁免覆盖 2 个真等价变异)
- **Tests added:** 14 新 fn(13 tokio/sync 单测 + 1 既有测试补一行),suites 数不变

## [09:4x] 交付行

**结果一句话**:取段 30 条 → **28 条钉死 + 2 条真等价豁免**(穷举 2^24 实证 + 负控绿),register_repo_root 单测同钉**附赠判死第 31 条**;清偿 31,control.rs 剩 **47**;产品逻辑零改动(生产半区与开工基线 md5 一致 `ccc30ce9…`),三门无管道全绿,负控 31/31 如预期。

**清单取法**:`gh run view 40256596759 --log | grep MISSED` 落盘剥 ANSI,recursive-cli 段共 222 条实有,与批次 1-5 台账逐文件对账(58 output + 31 claude_json + 28 init + 14 otel + 4 main + 5 resume + 3 interrupt + 1 workspace = 144 已清,加本批 31 = 175,核对 222 − 47 = 175 ✓),余 `control.rs` 78 条;本地 `--list -p recursive-cli -f …/control.rs` 基线 145 条与 CI 行号逐一吻合(生产半区无漂移),取行号序前 30。

**各家族钉法(全部进程内,无子进程钉——本段 30 条无一条落在 `println!` 直打面)**:
- **ask_* 三发射器 6 条**(`:98`/`:116`/`:129` ×2):批 4 已实证本工具链 libtest 捕获进程级,`println_locked` 的帧内容进程内不可观测;但体替换(`None`/`Some(Default)`)与原码的可观测差不在 stdout 而在 **waiter 注册**——钉法:spawn 发射器 → `parked_request_ids` 200 次 yield 轮询 `bridge.pending` 出 ≥1 个 waiter(变异体从不注册 → panic)→ 走真 `control_response` 线型 resolve → 断言返回值逐字节等于回执 → 断言 `pending` 清空。helper `answer_single_waiter` 共享。
- **cancel_pending / fail_all_pending 2 条**(`:167`/`:177`):ask_can_use_tool 造真 waiter → `cancel_pending(未知 id)` 必须不动别人(正控)→ `cancel_pending(真 id)` 后 5s 内拿到**恰** `{"behavior":"deny","message":"cancelled by host","interrupt":true}`(体替换 `()` 则 waiter 永不 resolve → timeout 红);fail_all 同型,双 waiter 各得恰含 `message` 的 deny,pending 清空。
- **session_id 2 条**(`:220`):`session_writer: None` → `"unknown"`;`SessionWriter::create_at(dir 名 = sess-control-245b6, …)`(显式目录,**零 env 依赖**,锁文件与 .meta.json 落 tempdir)→ id 恰为目录名。两变异(`"xyzzy"`/`""`)双双红。
- **error_response 1 条**(`:246`):与完整 envelope `{"type":"control_response","response":{subtype:"error",request_id,error}}` 逐字段 `assert_eq!`。
- **"strict" 1 条**(`:263`):既有 `permission_mode_roundtrip` 的轮转表**漏了 "strict"**(幸存根因)——补 `"strict"` 一行 + 新钉 `parse_permission_mode_knows_strict_and_rejects_unknown_names`(`Strict` 判型 + 大写/空串 None 双向)。删臂后 roundtrip 的 `.expect(s)` 即红。
- **workspace_diff 2 条**(`:680`):tempdir 里 hermetic git 仓(`git_in` helper:`-c user.name/email`、`GIT_CONFIG_NOSYSTEM`、`HOME=<tempdir>`、`safe.directory=*`、`LC_ALL=C`,沿 `src/checkpoint.rs::git_cmd` 口径)→ 干净树断言 `Ok("")` 正控 → 改文件后 diff 必含 `notes.txt` 与 `+second line 245b6`。`Ok("xyzzy")` 死于正控,`Ok(String::new())` 死于脏树断言。依赖 PATH 上有 git——既有先例 `cli_session_surfaces.rs`/`checkpoint` 测试同依赖,CI runner 自带。
- **control_read_file 9 条**(`:689`/`:696` ×2/`:705` ×3/`:712`/`:727`):11 字节文件走 utf-8/base64/`max_bytes`=len/=5 四态 + 4096 字节与 512KiB+1 文件钉**默认预算恰 512\*1024**(`*→/`=0 → 4096 全文断言红;`*→+`=1536 → 4096/524288 断言红;`>→>=` 死于 `max_bytes==len` 非截断正控;`>→<`/`>→==` 死于 `max_bytes<len` 截断前缀;`==→!=` ×2 分别死于 encoding 键的有无两侧)。`absPath` 按 `resolve_within` 的 caller-spelling 语义与 `ws.join(name).display()` 全等。
- **base64 :745 1 条**:`>→>=` 让 1 字节尾组打出 `TABLE[0]='A'` 而非 `'='`——既有 `base64_encode_smoke` 只有 2/3 字节例(幸存根因),新钉 `"h"→"aA=="`、`"abcd"→"YWJjZA=="`、`[ff,fe,fd,fc]→"//79/A=="`、`b""→""`。
- **seed_read_state 1 条**(`:760`):错误面先行(`read_file_state: None` → 恰 `"read_file_state not available"`,缺 path → `"missing path"`)再正路:`ReadFileState::get` 取回 `ReadRecord{is_partial:false, content:"seeded\n", timestamp>0}`。体替换 `Ok(())` 死于第一个 Err 断言。
- **register_repo_root 5 条**(`:785` ×2/`:790`/`:798:8` + 附赠 `:798:37`):错误面三条恰串(missing / relative → `"directory must be absolute"`(`!` 删除后 relative 反而落进 roots-None 分支,红)/ no roots)→ `new_shared_sandbox_roots()` 上注册绝对路径两次,断言返回 `Ok(display)` 且 roots **恰** `[(path, ReadWrite)]` 一条(idempotent)。`:798:8` 删 `!` → 首注册不 push → 空 → 红;`:798:37` `==→!=` → 二次注册 push 重复 → 两条 → 红(第 31 条,同钉附赠)。

**2 条豁免(mutants.toml,`--list` 自检 145→143 恰 2 条,diff 即该两条)**:`:742:28`/`:742:40` `|→^`(`(b0<<16) | (b1<<8) | b2`)**真等价**——三操作数皆 `u8 as u32`,各占位域 16-23 / 8-15 / 0-7 两两不交,不交位集上 `a|b ≡ a^b`;**穷举全部 2^24 字节三元组零差异**(两种列位独立验证,脚本输出在交付记录)+ 负控绿(base64 三钉 3/3 过)。正则 `control\\.rs:742:(28|40): replace \\| with \\^ in base64_encode`:行列锚定,同列位的可杀同胞 `|→&` ×2 实证仍在册(grep 计 2)。

**负控 31 条**(`/tmp/b6-negctl/negctl.py` + `negctl.log`,行号级手工变异:体替换按行界、算子按列位,备份取自钉成终态后的树):**29 红**(exit 101、`compile_err=False` ×31 → 全是真测试失败而非编译错;每条 killers 恰为预期那只钉,read_file 家族两钉并跑时恰一只或两只红与判死面预测一致)+ **2 绿**(`:742` 两条等价)。每次还原后 `filecmp` 字节断言,31/31 restored=True。

**三门无管道全绿**(日志 `/tmp/b6-gates/{fmt,clippy,test}.log|.exit`,各门独立取码):fmt --check EXIT=0 / clippy --workspace --all-targets --all-features -D warnings EXIT=0(0 warning)/ test --workspace EXIT=0,**81 suites 5431 passed 0 failed 12 ignored**(= 批次 5 的 81/5417 + 14 fn,对账一致)。

**红线自证**:`git status` 仅 2 路径(`control.rs` + `mutants.toml`);control.rs 以首个 `#[cfg(test)]` 切生产半区,开工基线与终态 **md5 同为 ccc30ce9…**,`git diff -U0` 三 hunk 全部 ≥ :1352(均在 mod tests);产品逻辑零改动,真缺陷零发现。#222:无进程内 `set_var`,git helper 的 `HOME`/`LC_ALL` 全走子进程 `Command::env`;workspace_diff 产品码继承环境属既有行为未触碰。#224:无 include_str!/源码自匹配钉(无 CRLF 面);断言全 `contains`/逐字段 eq(CRLF 中性);`relative/dir` 全平台为相对路径;无 libc/pty/signal/timing 敏感断言(5s timeout 只上界"立即完成"之事,200-yield 轮询在 current-thread runtime 下确定),无 `#![cfg(unix)]` 之需;ubuntu-only 自查:git/tempfile 皆 CI 标配,无平台差异面。本地 mutants 未跑(§2b,仅 `--list` dry-run ×2:toml 前 145 / 后 143);GitNexus MCP 不可用,grep 核 blast radius:被钉 11 个函数的全部调用点(`ControlSdkHookForwarder`/`ControlElicitationHandler`/`plan_dialog_loop`/`handle_stdin_line`/`stdin_control_loop`/initialize 等 host 臂)零改动,改动仅测试。

## 剩余条数与下批分段(cli 余 47;agent 68 / tui 54 不变)

- **本批取段**:control.rs 行号前 30(`:98` → `:798:8`),附赠 `:798:37`;**control.rs 余 47 = 78 − 31**。
- **批次 7(≈25)**:`file_suggestions` 3(:805 ×3)+ `:813`/`:815` 算子 ×2 + `walkdir_shallow` 10(:824 ×2、:834 ×4、:838 ×3、:839)+ `rewind_files` 1 + `list_mcp_servers_status` 2 + `is_stdin_closed` 2 + `record_usage` 1 + `plan_dialog_loop` 4(:953/:971/:986/:987)——纯函数/状态机面,全部进程内可钉,tempdir 目录树即可。
- **批次 8(≈22)**:`handle_stdin_line` 4(:1043/:1046/:1060 ×2)+ `parse_permission_result` 6(:1138 ×3/:1141 ×3)+ `discover_loaded_skills_for_workspace` 1 + `SdkHookForwarder::forward` 9(:1190 ×2、:1206-1211 六臂、:1237)+ `ControlElicitationHandler::elicit` 2——parse/discover 进程内;stdin 线/forward 需 bridge waiters(本批 `parked_request_ids`/`answer_single_waiter` helper 可直接复用)。
- 残留 1(`main.rs:1169 skills_base` INERT):随 B2 拍板项 B 处置,不计入。agent 68 / tui 54 维持 B2 建议。
- 备份:`/tmp/b245-b6/backups/`(`control.rs.orig/.pinned`、`mutants.toml.orig`、`wip-tests+toml.patch`、negctl 副本、三门 log/exit、list-before/after.txt、prod-half md5、CI MISSED 全量清单)、`/tmp/b6-negctl/`、`/tmp/b6-gates/`。未 push、未动 main,commit 留 wt-b1 待主控验收。
