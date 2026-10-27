# 2026-10-27 — #245 批次 7:recursive-cli control.rs 后段 25 条(:805 → :987)

- **Date:** 2026-10-27,交付行 [13:0x](本机本地时钟)
- **Goal:** #245 批次 7 — run 40256596759 CLI 段,control.rs 批次 6 后余 47,本批取**行号序后段前 25 条**(`:805 file_suggestions` → `:987 deny 臂`),与批次 6 预留分段一致
- **Files touched:** `crates/recursive-cli/src/cli/control.rs`(+303,全部在 `mod tests`:7 个新测试 fn + 3 个 helper)
- **Tests added:** 7 新 fn,suites 数不变

## [13:0x] 交付行

**结果一句话**:取段 25 条 → **25 条全部钉死、零豁免**;control.rs 剩 **22**;产品逻辑零改动(生产半区与开工基线 md5 一致 `ccc30ce9…`),三门无管道冻结树全绿,负控 25/25 判死。

**清单取法**:CI MISSED 落盘剥 ANSI,control.rs `:798` 之后行号序 25 条 = CI 段(`:805`×3 / `:813` / `:815` / `:824`×2 / `:834`×4 / `:838`×3 / `:839` / `:850` / `:868`×2 / `:934`×2 / `:939` / `:953` / `:971` / `:986` / `:987`),与批次 6 台账预留「批次 7(≈25)」逐条吻合。

**各家族钉法(全部进程内 + tempdir,无子进程钉)**:
- **file_suggestions 5 条**(`:805`×3 / `:813` / `:815`):一棵 6 文件树(visible 3 + `.hidden` + `target/` + `node_modules/`),空查询全量 `sort` 后全等(判 `vec!["xyzzy"]`/`vec![String::new()]`/`vec![]` 整体体替换)、非空查询 `"MAIN"` 大小写不敏感恰一条(判 `||→&&`)、limit 2/1 双切(判 `>=→<`:变异后 len 1/3)。walk 顺序依赖 fs,断言一律 sort 后比。
- **walkdir_shallow 10 条**(`:824`×2 / `:834`×4 / `:838`×3 / `:839`):一棵 7 节点树(visible `a.txt`、`d1/b.txt`,深层 `d1/d2/c.txt`,剪枝 `.hidden.txt`/`.hiddendir/x.txt`/`target/out.o`/`node_modules/n.js`),`walkdir_shallow(ws,1)` 期望恰 `{a.txt, d1/b.txt}`(绝对路径,`ws.join()` 口径)——剪枝 4 算子(`||→&&`×2 / `==→!=`×2)各自让垃圾节点入榜或全灭;深度 4 条(`<→<=`/`<→==`/`<→>`/`+→*`)分别让 `c.txt` 入榜或 `d1/b.txt` 消失;体替换 2 条(整体 `vec![]`/`vec![Default]`)死于全等。单钉判 10,负控逐条确认杀手命中。
- **rewind_files 1 条**(`:850`):dry_run 正路逐字段(`dry_run=true`/`would_rewind=false`/`note` 全串)+ 无/显式 false 双走 Err 全串——体替换 `Ok(Default)`(Null)两头皆红。
- **list_mcp_servers_status 2 条**(`:868`):四态垃圾先钉(缺文件/非 JSON/无 `mcpServers`/`mcpServers` 非对象 → 空)再正路 2 server(name,status) 排序全等。
- **is_stdin_closed 2 条**(`:934`):新会话 false → `store(true)` 后 true,双侧各杀一只。
- **record_usage 1 条**(`:939`):两次累计六字段逐一 `assert_eq!` + `api_ms` 50+70=120;`TokenUsage` 是 Copy,锁内解引用非 clone(clippy `-D warnings` 红过一次 `clone_on_copy`,已修)。
- **plan_dialog_loop 4 条**(`:953` / `:971` / `:986` / `:987`):真线型三幕——P1 answer `approve` → 真 `wait_for_approval()` 得 `Approved` 且 `pending_plan` 清空(判 :953 整体 `()` 无 waiter、:986 删 allow 臂落 `_` 变 reject);P2 answer `deny+message` → `Rejected{reason}` **逐字等于 message**(判 :987 删 deny 臂落 `_` 的 `"cancelled by host"` fallback);P3 换文本计划必须再问(判 :971 `==→!=` 把「新计划」当「已问过」永久跳过)。**关键坑**:批次 6 的 `parked_request_ids` 是纯 yield 轮询,等不到 200ms tick 的 loop——新 helper `eventually_parked_request_ids`(真 sleep 25ms × 5s 窗);`bridge.pending` 只存 oneshot sender,请求载荷进程内不可观测,判死观测量=waiter 是否出现 + 决策型/理由。

**负控 25 条**(`/tmp/b7-negctl/negctl.py` + `negctl.log`,行号级手工变异:体替换=签名后插 return、算子=行内列位替换、删臂=行区间删除;预期杀手单测过滤,`compile_err` 单独甄别):首轮 **23 红 + 2 SPARED**——SPARED 两条(`:805`/`:824` `vec![]`)为**负控脚本自身建模错**,非钉盲:手改只换初始化行,循环仍 push,`vec![]` ≡ `Vec::new()` 等价原码;按 cargo-mutants **整体体替换**语义重做(签名后插 `return vec![];`)双红。终局 **25/25 KILLED,compile_err=0**,每次还原 `filecmp` 字节断言 True。

**三门无管道全绿**(冻结终树复跑,日志 `/tmp/b7-gates/{fmt,clippy,test}.log|.exit` 独立取码):fmt --check EXIT=0 / clippy --workspace --all-targets --all-features -D warnings EXIT=0 / test --workspace EXIT=0,**81 suites 5438 passed 0 failed**(= 批次 6 的 5431 + 7,对账一致)。

**红线自证**:`git status` 仅 `control.rs`;`git diff -U0` 单 hunk `@@ -2001,0 +2002,303 @@`(纯 `mod tests` 追加,生产半区 md5 `ccc30ce9…` 与开工基线一致);真缺陷零发现。#222:无进程内 `set_var`(仅 `std::env::temp_dir()` 读取);#224:无 include_str!/源码自匹配钉,断言全 sort+eq/逐字段,路径拼接走 `MAIN_SEPARATOR`;无 libc/pty/signal,5s timeout 只上界「必须完成」之事;ubuntu-only 自查:tempfile/std::fs/tokio 常规件,无 `cfg(unix)` 之需,windows dead_code 无新面(无 cfg 门代码)。本地 mutants 未跑(§2b)。GitNexus MCP 不可用,grep 核 blast radius:被钉 8 个函数的全部调用点(`handle_host_control_request` dispatch / `stdin_control_loop` / run loop plan gate 臂)零改动,改动仅测试。

## 剩余条数与下批分段(cli 余 22;agent 68 / tui 54 不变)

- **本批取段**:control.rs `:805` → `:987` 行号序 25 条;**control.rs 余 22 = 47 − 25**。
- **批次 8(≈22,control.rs 收口)**:`handle_stdin_line` 4(`:1043`/`:1046`/`:1060`×2)+ `parse_permission_result` 6(`:1138`×3/`:1141`×3)+ `discover_loaded_skills_for_workspace` 1(`:1159`)+ `SdkHookForwarder::forward` 9(`:1190`×2、`:1206-1211` 六臂、`:1237`)+ `ControlElicitationHandler::elicit` 2(`:1302`)——parse/discover 进程内;stdin 线/forward/elicit 需 bridge waiters(本批 `eventually_parked_request_ids`/`answer_the_parked_waiter` 与批次 6 helpers 可直接复用)。
- 残留 1(`main.rs:1169 skills_base` INERT):随 B2 拍板项 B 处置,不计入。agent 68 / tui 54 维持 B2 建议。
- 备份:`/tmp/b7-negctl/`(`negctl.py`、`negctl.log`、`control.rs.orig`、`control.rs.orig.postgates`)、`/tmp/b7-gates/`(三门 log/exit)、`/tmp/b7-mutants-full.log`(CI MISSED 全量)。未 push、未动 main,commit 留 wt-b2 待主控验收。
