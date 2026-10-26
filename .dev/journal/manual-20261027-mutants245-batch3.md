# 2026-10-27 — #245 批次 3:recursive-cli 后续段(init.rs 28 + workspace.rs 1)

- **Date:** 2026-10-27,交付行 [00:5x]
- **Goal:** #245 批次 3 — run 40256596759 MISSED 清偿,recursive-cli 后续段(≤30,与 B2 批次 2「会话族+main.rs 首段」错峰)
- **Files touched:** `crates/recursive-cli/src/cli/init.rs`(+52,仅 mod tests)、`crates/recursive-cli/tests/cli_init_wizard.rs`(新,6 子进程钉)、`crates/recursive-cli/tests/cli_workspace_show.rs`(新,1 钉)、`.cargo/mutants.toml`(+30,2 条行号锚等价豁免)
- **Tests added:** 11(4 单测 + 6 向导子进程钉 + 1 workspace 钉)

## [00:5x] 交付行(wt-b1 席)

**#245 批次 3 清偿:recursive-cli 后续段 29 条 = init.rs 28 + workspace.rs 1,与 B2 批次 2(control.rs 78 + main.rs 首段)零文件交集;27 杀 + 2 上下文等价豁免(行号锚 exclude_re)。** 产品逻辑零改动(全部 hunk 在 mod tests / 新测试文件 / mutants.toml 豁免段);三门无管道全绿(fmt EXIT=0 / clippy EXIT=0 / test --workspace EXIT=0,78 suites 5393 passed 0 failed = 批次 1 基线 76/5382 + 新 2 suite 11 钉);负控实证 29 循环(27 红 + 2 绿等价证明)逐条还原 cmp 一致。**清单口径修正**:run 40256596759 head=8a2048a7 **早于批次 1a/1b**(20523693/482260ac),其 claude_json 31 条是批次 1b 已清偿存量、eval bin 前 10 条批次 1a 已杀——任务书里「claude_json 余量」实为 0,本批按「其它子命令文件」取 init.rs + workspace.rs。

**杀伤明细(27)**:init.rs 纯函数 6 条用 4 个单测钉——slugify `&&`→`||`(:23,`slugify("a- b")` 必须出 `"a-b"`,变异体推 `"a--b"`)、resolve `idx >= 1`→`<`(:70,选 "2" 必须解析到 `presets[1]`;既有 `numeric_one` 测试因 presets[0]==anthropic==default 放走了它,pin 内 `assert_ne!(presets[1].id, default)` 防目录重排空转)、detect_current_preset 体替换 ×3 + `delete !`(:84,临时 config.toml 断言 `preset=deepseek (model=` 前缀 + 缺文件 None 双向)。run_init 体 20 条用 6 个子进程钉(env_clear + RECURSIVE_HOME tempdir + RECURSIVE_PROVIDERS_URL 指 127.0.0.1:9 拒绝端口、stdin 管道喂脚本,同 cli_init_keeps_env 先例):catalog 三段渲染钉的期望行从 `recursive::all_presets()` 同源重算(bundled-only OnceLock,测试进程与子进程天然一致、零 env),**编号镜像真实公式**——international 用 `i+1`,mainland/local 用 `all_entries.len()+i+1` 且边推边增长 → 打印 7,9,11…23 步进 2(手工裸跑核对),:273 段成员/:276×2 滤网/:278 滤网/:284×2/:301×4/:318:53×2/:318:49\* 共 13 条被 16 行逐字节 contains + 退出码(usize 下溢 panic 族)尽杀;key 三态钉::493 guard→true(空 env 变量必须落 prompt 并保存手打键)、:500 `delete !`(anthropic key_url 提示行)、:559 三兄弟(手打键必须写 secrets.env;env 预填键**绝不**写——`&&`→`||` 与 :559:31 由「不写」侧红,:559:8 由「写」侧红);manual 路径钉::427 `delete !`(拒存答 "n" 不得出 providers.d 文件、config 走 type/api_base 内联且无 `preset` 键)、:528:12 `delete !`(存Y 后 toml 必须含 `[[providers.models]]`,跳过 push 即缺段)。workspace.rs :180 print_workspace 体替换:`workspace add`+`show` 两段子进程,record 块 id/name/state/not-indexed 提示全断言。

**2 条上下文等价豁免(.cargo/mutants.toml,#223/#228/#243 口径)**:①`:318:49` `+`→`-`(local 段编号)——向导显示列表是 `all_presets()` bundled 目录恒单条 ollama,i=0 时 `len-i+1` ≡ `len+i+1`;目录加第二条本地预设时随目录变更一并解除豁免。②`:528:51` `==`→`!=`——snapshot 块只在 manual 臂可达,`models` 恒为新建 `Vec::new()`,`any()` 两算符同假,push 皆发生(push 本身有 :528:12 钉)。**方法论发现(建议留档)**:cargo-mutants 的 exclude_re 匹配串**含 `file:line:col: name` 全串**,行号锚可外科手术定位——本批探针实证(行号锚 pattern 恰摘 :318:49,同名同胞 :284:33/:301:× 与 :528:51 的孪生 :278:68 全数保留)。此二条 name 与 7 条可杀 mutant 完全同名(`replace + with - in run_init` ×4、`replace == with != in run_init` ×2 等),无行号锚则要么豁免不了、要么连带降级可杀同胞。`--list` 自检:两 pattern 各恰命中目标,同族其余 10 条全在单。

**负控实证 29 循环**(驱动 /tmp/b3_negctl.sh,逐条日志 /tmp/b3-negctl/,全部 VERDICT=OK):27 红 EXIT=101——slug_23_andor、resolve_70_getlt、detect_84_none/empty/xyzzy/delnot、intl_273_delnot、ml_276_andor、ml_276_neq、local_278_eqneq、intl_284_minus、intl_284_times、ml_301_49_minus、ml_301_49_times、ml_301_53_minus、ml_301_53_times、local_318_53_minus、local_318_53_times、local_318_49_times、persist_427_delnot、key_493_guard_true、key_500_delnot、snap_528_12_delnot、secret_559_8_delnot、secret_559_28_andor、secret_559_31_delnot、ws_180_body;2 绿等价证明 EXIT=0——local_318_49_minus(全套件 6/6)、snap_528_51_eqneq(6/6)。每循环还原后 cmp 逐字节一致,零 RESTORE_MISMATCH。

**红线自证**:kernel.rs/runtime.rs/run_core.rs/agent/、tools/、http_call.rs、a2a.rs、mcp.rs、hardening.rs、agui_session.rs 零触碰(git status 仅 4 交付文件,mutants.toml 增量仅注释+两行正则);产品逻辑零改动;#222:全部新测试 env_clear + RECURSIVE_HOME tempdir,零环境依赖(单测走显式 tempdir path 不触 env);#224:零 cfg、零 include_str、无平台分段(渲染钉无 CRLF 敏感面);ubuntu-only 自查:子进程仅 std::process::Command + 退出码/stdout 断言,127.0.0.1:9 拒绝端口同 cli_init_keeps_env(ubuntu CI 既有先例),零 unix 专属 API、零 ignore。本地 mutants 未跑(§2b);GitNexus 不可用,grep 代:print_workspace 唯一调用方 workspace show(workspace.rs:104)、run_init 唯一调用方 main.rs:1248、detect_current_preset/slugify_preset_id 文件内私有、resolve_preset_choice/detect_model_from_api_base 无其他调用方。

## 剩余条数与下批分段(run 40256596759 口径,减已清偿)

- **recursive-cli 余 145**:output.rs 58(建议下批主力,可拆两批:print_usage/print_finish_note/exit_for_finish 22 条 + stream_events/stream_events_repl/stream_events_json/drain_events 36 条;与批次 1b claude_json 钉同族,断言模式可复用);otel.rs 14 + interrupt.rs 3 = 17 可凑一批;**resume.rs 5 与 main.rs 4——resume/interrupt 疑属「会话族」,本批为避撞车未取,归属(B2 抑或下批)请主控拍板**;control.rs 78 在 B2 批次 2。
- **agent 余 68**(批次 1 台账原建议维持):eval bin 33 → audit_log 14 + agui_session 5(#152 禁区,钉走集成测试)→ checkpoint 7 + compact/mod 9。
- **tui 54**:走 tui-mutants 通道,建议 tui 席认领。

备份:/tmp/b3-backups/(终态 4 交付文件 + negctl 驱动脚本)+ /tmp/b3-negctl/(29 循环逐条日志)。未 push、未动 main,待主控验收。
