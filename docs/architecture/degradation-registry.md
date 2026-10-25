---
type: Architecture
title: Degradation Registry — 静默降级站点登记（活文档）
description: Owner ruling「任何降级必须可观测」的全仓登记面：已知可观测站点（预载）、2026-10-25 首版全仓扫描的新发现违例（只登记+立单）、判定为非降级的排除口径、与维护约定。
tags: [degradation, observability, fail-open, fail-closed, owner-ruling, issue-201]
timestamp: 2026-10-25T00:00:00Z
---

# Degradation Registry — 任何降级必须可观测

> Owner ruling（2026-10-18）：**任何降级必须可观测**——swallowed errors、default
> fallbacks、fail-open gates、capacity/clamp diversions must leave a
> WARNING-or-louder trace. Silent diversion is a defect even when the fallback
> "works"（#167/#151 类）。

本页是该 ruling 的**单一登记面**（#201 项④，Goal 428 S4 首版）。每个降级站点
（fallback、吞错、默认值换实现、fail-open）登记一行；新站点随修随登记，违例
修复 = 一行 WARN/error/metrics + 一条回归钉 + 状态翻转。扫描新发现的违例
**只登记 + 逐条立单**（标题前缀 `fix(observability):`），不随登记批修。

**状态口径**：

| 标记 | 含义 |
|---|---|
| ✅ 有钉 | 已可观测（warn!/error!/metrics）且回归钉住观测本体 |
| 👁 无钉 | 已可观测，但观测本体无回归钉（变异可杀掉 WARN 而测试仍绿） |
| 📄 成文 | 契约/文档面成文（doc 即观测载体） |
| ❌ 违例 | 静默降级（无观测），已立单，等修复翻转 |
| ⚫ 非降级 | 设计默认/设计豁免（本页 C 节给理由） |

**首版扫描口径**（2026-10-25，基线 main=`20e9ac84` + wt-b2 `880cff08`）：范围
`src/` + `crates/*/src/`（排除 `#[cfg(test)]` 与 `*tests*.rs`）+ `.dev/scripts/`
脚本面。候选模式：`.ok()?`/`.ok();` 吞错、`unwrap_or_default()`/`unwrap_or(默认)`
在 I/O·解析·配置路径、`let _ = <fallible>`、`if let Err(_)` 空臂、
`map_err(|_|)` 纯丢弃、`2>/dev/null`、fallback/`or_else` 换实现。逐候选人工判定
四态。原始命中 ~1700，复核后违例 13（B 节）。

## A. 已可观测站点（预载：#201 规格④列 + #182 + #154）

| 站点 | 降级形态 | 观测方式 | 回归钉 | 来源 | 状态 |
|---|---|---|---|---|---|
| `src/tools/transport_layer/docker_provider.rs:134` | Docker 启动失败 → 回退宿主 Bash（隔离层 container→host） | `tracing::warn!` + `sandbox_mode()` 改报真实 host 层（不再谎称 Container） | `container_start_failure_warns_and_reports_actual_tier`（同文件 ：224） | #167 | ✅ 有钉 |
| `src/http/mod.rs:480`/`:1019`（permission_mode 解析） | 未知 permission_mode 原静默降级 Default（放水假安全档）→ **已改 400 fail-closed，降级消除** | 400 Bad Request（降级不复存在） | `unknown_mode_is_bad_request`（`src/http/handlers.rs:6252`） | #151 / b8695a9a | ✅ 已消除 |
| `src/config_file.rs:527-532` | 权限层规则文件读失败/坏 TOML 原静默跳层（规则面 fail-open）→ WARN「layer skipped (fail-open degradation)」（path+source+error）；缺文件仍静默=合法无配置非降级 | `tracing::warn!`(target recursive::permissions) | `load_permission_layer_bad_toml_warns_and_skips_layer` / `_unreadable_file_…` / `…_missing_file_stays_silent`（负控） | 053f2e01（426 S3(b)） | ✅ 有钉 |
| `src/hooks/external.rs:755` | 审批 hook 显式配 `fail_mode=open`（hook 坏=当 yes）→ 配置期 WARN；缺省 fail_mode 审批事件 fail-closed | `tracing::warn!`(event) | `default_fail_mode_is_closed_for_approval_events_only`（:2297）+ Goal 281 fail_mode 组（:2116 起） | #154 / a2eaa9f8 | ✅ 有钉 |
| `src/tools/policy_domain/permission_pipeline.rs:177`/`:360` | auto 分类器失败 → 降级静态规则 + WARN；无分类器的 auto 全拒 fail-closed | `tracing::warn!` ×2 | `auto_without_classifier_denies_fail_closed` / `auto_permission_mode_is_rejected` | #182 / 7539f9bc | ✅ 有钉 |
| `src/hardening.rs:108`/`:129-130`/`:147` + `crates/recursive-cli/src/main.rs:617` | prctl 失败 fail-open + env scrub 结果 + 非 Linux env 暴露 → `HardenOutcome::report`（WARN 双边值 mechanism+errno）/ DEBUG scrub 行 / `warn_if_exposed`（os/count/变量清单） | WARN+DEBUG（结构化字段） | #223 批 2 tracing 捕获层 6 条（955a2fa9：report/warn_if_exposed/scrub 双向钉） | #150 / 85a17c05 | ✅ 有钉 |
| `src/http/event_log.rs:131` + `handlers.rs:3375-3391` + `mod.rs:3257`/`:3289` | SSE ring 逐出/持久化失败 → 计数上 /metrics（sessions_evicted、persist_failures）；游标 gap 显式化而非静默空续流 | /metrics 计数 + replay gap 标记 | `push_evicts_oldest_beyond_capacity` / `replay_of_evicted_cursor_returns_all_and_flags_gap`（event_log.rs:299/:349） | Goal 422 / f4c35413 | ✅ 有钉 |
| `src/config_file.rs:318-334`（config set 写路径） | 既有 config.toml 坏 TOML 时原 `unwrap_or_default()` 静默整文件替换为单键文档 → WARN「refusing to write a reduced replacement」+ 硬错误（降级消除，拒绝写入） | `tracing::warn!`(path+error) + 硬错误 | 写路径解析失败即败（#213 验收） | #213 | ✅ 已消除 |
| `src/http/uplink.rs`（read_bounded_uplink） | 有界上行超限 → typed 413 + `Metrics.uplink_overflows` + WARN（session_id/limit/observed）；读体中断 400 不计数 | 413 信封 + metrics + WARN | Goal 423 W2a 测试组 | ff3cfcd9 | ✅ 有钉 |
| `src/atomic.rs:109` | Windows 臂 sync_all 不可用 → 非 Windows 臂 WARN（带 path）；Windows 豁免成文 | 注释成文 + 非 Windows WARN | 注释即口径（「Per the owner red line…」） | 85a17c05 邻接 | 👁 无钉 |
| `docs/architecture/compaction.md` §4 | 压缩四契约含「可观测降级」段（降级行为成文） | 契约成文 | — | Goal 424 / d369f5d3 | 📄 成文 |
| `docs/architecture/cancellation.md` | 取消/回收降级面成文（reclaim WARN 口径） | 契约成文 | — | Goal 425 / e6b0fd56 | 📄 成文 |
| `docs/architecture/channels.md` | 六通道降级面成文（auth 503 / admission 503+Retry-After / 实验级降 SLA 不降门） | 契约成文 | — | Goal 427 / 4cce895b | 📄 成文 |

## B. 首版扫描新发现违例（2026-10-25，只登记+立单，未修；逐单修复后就地翻转状态列）

| 站点 | 降级形态 | 观测方式 | 回归钉 | 来源 | 状态 |
|---|---|---|---|---|---|
| `src/checkpoint.rs:653`（write_workspace_tree，原 :648） | checkpoint diff 的 `git add -A` 进临时 index，`.output()` 结果整丢——staging 失败后 write-tree 对空/部分 index 成功，diff 静默错账（「全删除」/「无变更」）→ status 检查 + `tracing::warn!`「checkpoint diff: git add failed; …」（含 stderr 尾，`stderr_tail` 截 200 字符）；diff 保持 best-effort 不改控制流。sibling `snapshot_for_session`（:196-221）的同型静默洞同批补：容忍臂（stderr 空 / 含 nothing-to-commit / warning:）穿透时 WARN「snapshot may be partial」，Err 臂不变 | `tracing::warn!`（默认 `--log info` 可见） | `diff_staging_failure_warns_instead_of_silent_misaccounting`（shadow_dir 0555 注入 staging 失败，cfg(unix) 按 #224 口径）+ `diff_healthy_staging_stays_silent`（正控）（src/checkpoint.rs tests；负控条件取反还原静默 → 钉红 `got: ` 空） | S4 扫描 → #229 | ✅ 有钉 [#229](https://github.com/jeffkit/recursive/issues/229) |
| `src/session/index.rs:876`/`:894`（restrict_dir/restrict_file，原 :873/:883） | 私密会话索引/库 0700/0600 chmod `let _ =` 吞——失败留 umask 默认权限（安全邻接），doc 宣称契约但失败无痕 → `if let Err` + `tracing::warn!`「session index: failed to restrict … permissions to 0700/0600」（path+error 字段）；windows 无 chmod 等价物（ACL 治理，readonly 位非同一契约），维持 no-op = 按 #224 口径注记的已知平台缺口，非静默吞 | `tracing::warn!`（默认 `--log info` 可见） | `restrict_dir_failure_warns_with_path_and_expected_mode` / `restrict_file_failure_warns_with_path_and_expected_mode`（不存在路径注入，直达与 EACCES/EROFS 同一的 Err 臂）+ `restrict_sets_private_modes_and_stays_silent_on_success`（正控：0700/0600 实测+不告）（全 cfg(unix)；同文件 :1580 起；负控两臂还原 `let _ =` → 两钉红 `got: ` 空、正控绿） | S4 扫描 → #230 | ✅ 有钉 [#230](https://github.com/jeffkit/recursive/issues/230) |
| `crates/recursive-cli/src/cli/builder.rs:382`（`file_config_or_report`，原 :363） | `~/.recursive/config.toml` 读失败/坏 TOML 原 `.ok().flatten()` 静默落 tier3 allow-all（规则面 fail-open；doc 宣称「logged to stderr」而代码不打）→ Err 臂 `eprintln!`「permissions: failed to read/parse <path>: <error>」对齐同函数 env 分支；缺文件 / 无 `[permissions]` 节仍静默 = 合法 tier3 非降级 | `eprintln!`（stderr，不受 `--log`/`RUST_LOG` 过滤） | `file_config_or_report_names_a_malformed_config_toml_instead_of_silently_allowing_all` / `…_names_an_unreadable_config_toml_with_its_path` / `…_is_silent_for_valid_and_absent_config_toml`（正控）（同文件 :1487/:1510/:1538；sink 层钉观测本体——负控 Err 臂还原静默即两钉红；`eprintln!` 接线一行不在进程内可测面） | S4 扫描 → #231 | ✅ 有钉 [#231](https://github.com/jeffkit/recursive/issues/231) |
| `src/http/session_mirror.rs:159`/`:168`（mirror_into 两写臂，原 :153-154 整吞） | AGUI teardown 镜像 transcript/meta 双 atomic_write 吞——镜像缺失/陈旧无痕（活路径 #147 有 ?，本站是镜像面）→ 两 Err 臂各打 `tracing::warn!`（session_id+path+error「AGUI session mirror persist failed; the session may be missing or stale in CLI listings (degradation)」），镜像仍 best-effort、teardown 控制流零改动 | `tracing::warn!`（默认 `--log info` 可见） | `mirror_write_failures_warn_with_session_id_and_path`（同文件 :515；目录占位 transcript.jsonl/.meta.json 注入双写失败，平台中性无 cfg 按 #224 口径）+ `mirror_healthy_write_stays_silent`（:543，正控）（traced_test/logs_contain 钉观测本体——负控还原 `let _ =` → 钉红 got: 空、正控绿） | S4 扫描 → #232 | ✅ 有钉 [#232](https://github.com/jeffkit/recursive/issues/232) |
| `src/acp/session.rs:329-344`（insert_with_id 两 Err 臂 :329/:337，原 :327-328 整吞） | ACP 建档初始 metadata 持久化吞——重启 resume 静默丢会话 → create_dir_all 与 save_metadata_to_disk 两个 Err 臂各打 WARN（session_id+pdir+error），会话仍入内存继续服务（行为不变，仅补观测）；save 内部 create_dir_all 吞为良性不动 | `tracing::warn!`「ACP session metadata persist failed; resume will lose this session (degradation)」 | `insert_with_id_warns_when_initial_metadata_persist_fails`（同文件 :1076；traced_test/logs_contain 钉观测本体——负控还原吞错即红已实证；内含正控：健康插入静默且 metadata.json 落盘） | S4 扫描 → #233 | ✅ 有钉 [#233](https://github.com/jeffkit/recursive/issues/233) |
| `crates/recursive-cli/src/cli/session.rs:175`（迁移 finish 吞，观测缝 :202 `finalize_migrated_session`）+ `crates/recursive-tui/src/backend.rs:906`（shutdown 吞，观测缝 :618 `finalize_session_on_shutdown`，原 :886-891 内联） | transcript `finish()` 定稿吞（迁移路径 `.ok()` 继续+报成功话术 / TUI shutdown `let _ =`）——SessionStatus 静默失真 → 两缝各打 `tracing::warn!`（session_dir+error「… keeps no final status marker (degradation)」），定稿仍 best-effort、控制流零改动 | `tracing::warn!`（默认 `--log info` 可见） | `migration_finish_failure_warns_and_healthy_finish_stays_silent`（cli/session.rs :697；.meta.json 目录占位注入 finish 读改写必败，平台中性无 cfg 按 #224 口径；内含正控 Interrupted 落盘且不告；EnvGuard `pinned_home` 按 #222）+ `shutdown_finalize_warns_when_finish_fails` / `shutdown_finalize_healthy_finish_is_silent_and_completes`（backend.rs :3819/:3842；PinnedRecursiveHome 按 #222；负控缝内还原吞 → 钉红 got: 空、正控绿；生产接线为单调用线，同 #231「不在进程内可测面」注记） | S4 扫描 → #234 | ✅ 有钉 [#234](https://github.com/jeffkit/recursive/issues/234) |
| `crates/recursive-tui/src/backend.rs:888`/`:942`（两 goals 入口，观测缝 :640 `create_session_writer_or_warn`，原 :843/:903 内联 `if let Ok(sw)`） | `SessionWriter::create` 失败（工作区不可写/磁盘满）走 `if let Ok` 隐式 else——整场 TUI 持久化静默关闭、运行如常 → 抽观测缝两调用线共接，Err 臂 `tracing::warn!`（workspace+error「TUI session persistence disabled: SessionWriter::create failed (degradation)」），仍返回 `None`、控制流零改动 | `tracing::warn!`（默认 `--log info` 可见） | `session_writer_create_failure_warns` / `session_writer_create_healthy_is_silent`（backend.rs :3897/:3925；PinnedRecursiveHome 按 #222；pin home 后以公共 `user_sessions_dir` 解析真实 sessions 路径、普通文件占位令 `create_dir_all` 穿文件必败，平台中性无 cfg 按 #224 口径；断言 WARN 语义句+workspace 路径字段+返回 None；内含正控健康 create 不告；负控缝内还原吞 → 钉红 got: 空、正控绿，cmp 逐字节还原） | S4 扫描 → #235 | ✅ 有钉 [#235](https://github.com/jeffkit/recursive/issues/235) |
| `src/session/writer.rs` meta 读改写族（原 :633 set_derived_from / :670 update_identity / :697 set_owner / :722 set_run_code 四函数内联 + :474 append 吞 bump_updated_at） | `.meta.json` 是消费方唯读的那份副本——resume 最近挑选读 `updated_at`（bump 的存在理由恰是崩溃会话）、成本取价读 `model/provider/preset`（陈旧=错账）、AG-UI 访问检查读 `owner`（缺失方向 fail-closed=锁死本人访问）、fork 血统 `derived_from`、run_code 能力位——读写失败静默=拿陈旧值无痕；家族 rustdoc 的 best-effort 指控制流（不中止调用方）而非后果可忽略，按 ruling 仍须可观测 → 抽私有缝 `rewrite_meta`（读改写 `?` 传播）+ `rewrite_meta_best_effort`（:536，唯一观测缝 `if let Err` + `tracing::warn!`（session_id+path+field+error）「session meta update failed; .meta.json keeps a stale value (degradation)」），5 调用点接线，公开签名与控制流零改动（失败仍不中止 run；「已是最新值→跳过写」幂等分支原样）；`finish_with_details` 失败本就 `?` 传播，保持自有实现 | `tracing::warn!`（默认 `--log info` 可见） | `meta_update_failures_warn_with_session_id_path_and_field`（平台中性：`.meta.json` 目录占位→读败，5 field 各钉 `field="…"`）+ `meta_update_write_failure_warns_and_idempotent_resets_stay_silent`（cfg(unix)：0555 目录→读成写败=旧 `let _ =` 恰吞的形态，附带幂等重设不 WARN 钉——误尝试写必败必告）+ `meta_update_healthy_writes_stay_silent`（正控）（src/session/writer.rs tests；traced_test/logs_contain 钉观测本体——负控缝内还原 `let _ = self.rewrite_meta(mutate)` → 两钉红 got 空、正控绿，cmp 逐字节还原；测试走 IsolatedWorkspace/SessionEnvPins 按 #222）+ `update_identity_persists_partial_changes`（#243 mutants 门硬化，CI run 40167681411：幂等三连 `&&` 的 `:687:36`/`:687:65` 两处 `&&`→`||` 部分-更新钉——两场景各只改一个字段（仅 provider / 仅 model 且 preset None==None），全变场景对两 mutant 不分叉；平台中性） | S4 扫描 → #236 | ✅ 有钉 [#236](https://github.com/jeffkit/recursive/issues/236) |
| `src/weixin/daemon.rs:162`（观测缝 :336 `bridge_raw_incoming`，原 on_message 闭包内联） | 入站微信消息桥接 `raw_tx.send` 吞——processor 死亡期（receiver drop）用户消息静默丢、无替代记录（与出站「对端已去=良性」惯例的差别=这是入站业务数据非控制面；实验通道降 SLA 不降观测门，channels.md）→ 吞改 `if let Err` + `tracing::warn!`（user_id+error「inbound weixin message dropped (degradation)」），桥接保持 best-effort（轮询闭包不 panic 不阻塞）、控制流零改动 | `tracing::warn!`（默认 `--log info` 可见） | `bridge_send_failure_warns_with_user_id_and_error`（drop receiver 注入 send 必败=processor 死亡形态，平台中性无 cfg 按 #224 口径；钉语义句+`user_id=`+`error=` 字段）+ `bridge_healthy_send_is_silent_and_delivers`（正控：消息原样送达且不告）（src/weixin/daemon.rs tests，`--features weixin`；traced_test/logs_contain 钉观测本体——负控缝内还原 `let _ =` → 钉红、正控绿，cmp 逐字节还原；生产接线为单调用线，同 #231「不在进程内可测面」注记） | S4 扫描 | ✅ 有钉 [#237](https://github.com/jeffkit/recursive/issues/237) |
| `.dev/scripts/self-improve.sh:1134-1135` | review 门三重 fail-open：脚本失败→approve 字面量 / 解析失败→approve / 缺键→approve（脚本已 deprecated，留档修；内层 review-changes.sh:170 有可见 marker，外两层全静默） | 无 | 无（脚本面无单测） | S4 扫描 | ❌ [#238](https://github.com/jeffkit/recursive/issues/238) |
| `src/compact/mod.rs:315`/`:720`（try_structured_compact / summarize free-text 回退臂，原 :315/:707） | provider 响应缺 usage → `unwrap_or_default()` 静默按 0 token 记账——预算（BudgetExceeded 判定）/成本双少计无痕；:311 info! 只覆盖 structured→free-text 回退、不覆盖 usage 缺失 → 两臂改 `match` 显式 `None` 臂 `tracing::warn!`（target `recursive::compact`；step+path=`"structured"`/`"free-text"`「compaction usage missing; accounted as 0 tokens (degradation)」），记账语义零改动（仍记 0；#42/#193 族 usage 口径归一不在本单）；Compactor 无会话身份字段（运行时租户透明 ruling）故携 step 不携 session_id | `tracing::warn!`（默认 `--log info` 可见） | `test_compact_contract_missing_usage_warns`（同文件 :990；`MockProvider` structured 队列硬编码 `usage: None` 即 structured 臂注入、`Completion { usage: None }` 即 free-text 臂注入，平台中性无 cfg 按 #224 口径；断言语义句+`path="…"`+`step=N` 字段+返回 usage==default；内含正控 `usage: Some(15)` 不告且取值保留；负控两臂各单独还原 `unwrap_or_default()` → 钉各在对应臂断言红（终态 :1035/:1063；负控 panic 报 :1018/:1042 为还原态行号，差 −13）、正控绿，cmp 逐字节还原） | S4 扫描 → #239 | ✅ 有钉 [#239](https://github.com/jeffkit/recursive/issues/239) |
| `src/llm/pricing.rs:244`（`RetryLog::take` poison 臂，原 :241 `lock().map(take).unwrap_or_default()`） | mutex poisoned（持锁线程 panic 过）时整批累计 retry/计费记录静默丢弃返回空——poison 后所有 retry 从 run_core 步级账面消失、成本归集少计无痕 → 改 `match`：`Ok` 臂原样 `mem::take`，`Err(poisoned)` 臂 `tracing::warn!`（target `recursive::llm`；`lost` = `poisoned.into_inner().len()` 即仍压在锁后的记录数「pricing retry records lost to poisoned lock (degradation)」）后仍返回 `Vec::new()`，drain 语义零改动（记录不回收——回收=产品决策，存照 journal「两面」）；sibling `record()` 的 `if let Ok` 吞 poison 不在本单（issue 只列 :241） | `tracing::warn!`（默认 `--log info` 可见） | `retry_log_take_warns_when_lock_is_poisoned`（同文件 :975；poison 注入=另起 `std::thread` 持锁 `panic!`、`join().is_err()` + `records.lock().is_err()` 双证锁确已 poison，平台中性无 cfg 按 #224 口径，不触 env 无 #222；断言返回空 + 语义句 + `lost=3` 计数；内含正控：健康 drain 返回 2 条且不告（置于注入前）；负控还原 `unwrap_or_default()` → 钉红「a poisoned drain must be announced」（终态 :1012，panic 报 :1004 为还原态行号 −8），正控绿，cmp 逐字节还原） | S4 扫描 → #240 | ✅ 有钉 [#240](https://github.com/jeffkit/recursive/issues/240) |
| `.dev/scripts/agent-mutants.sh:118` + `cli-mutants.sh:93`（EXIT trap） | 恢复臂 `git checkout -- 2>/dev/null \|\| true` 吞——变异体源文件可能静默留树（失败模式 #8/#9 之外的新形态）；红线：退出码语义不动，只补显式报错 | 无 | 无 | S4 扫描 | ❌ [#241](https://github.com/jeffkit/recursive/issues/241) |
| `src/tools/policy_domain/permission_pipeline.rs:290`（Phase 5 无 hook 臂，WARN 于 :297） | `interactive` 规则命中但 registry 无 permission hook——允诺的人审无法发生、静默放行（可达面：plain 文本 `recursive run` / 库调用；TUI/ACP/stream-json/AG-UI 有通道 hook 走 if-let 臂不触发）→ 拍板 (a) WARN+放行（控制流零变；HTTP 通道的 Deny 方向由 Goal 429 S2 单独落地） | `tracing::warn!`(target recursive::permissions; `tool=<canonical>`「interactive rule matched but no permission hook is installed; allowing (degradation)」) | `interactive_rule_without_hook_allows_and_warns`（同文件 :647；断言 Allow+语义句+`tool=Bash`；内含正控 `interactive_rule_with_hook_stays_silent` :682；负控还原 else 臂为裸 `drop(guard)` → 钉红「the promised approval gate vanishing must be announced」panic :657，cmp 逐字节还原） | Goal 429 S1 → #88 | ✅ 有钉 [#88](https://github.com/jeffkit/recursive/issues/88) |

## C. 判定为非降级的排除口径（供后续扫描复用）

高频良性模式——**不是**降级站点，后续扫描不必再逐条复核：

- **env 缺省 = 文档化默认**：`std::env::var(..).ok()` / `.unwrap_or(文档化默认)`
  （Err=未设置而非失败）。~200+ 处（`src/config.rs` 主体、TUI/backend 等）。
  例外已单列：credentials/特性开关类 env 的静默缺失若关掉安全特性则入 B 节
  （本版未发现此类）。
- **清理/drop 路径 best-effort**（带注释惯例）：临时文件清理
  （checkpoint.rs tmp-index 族）、Drop kill（process/mcp/acp 子进程收尾）、
  OTEL flush at exit（observability/exporter.rs:148-157）、
  panic-hook 写日志（tui lib.rs:118）。失败无权威副本可丢。
- **channel send 失败 = 对端已去**：UI 事件流（backend.rs ~45 处）、
  oneshot 回执（接收方已超时）、shutdown 广播。权威副本在 event log/accumulator。
- **失败方向更响而非更静**：`src/rewind.rs:162` read().ok() 误读 → 多报
  conflict 阻断破坏性操作（fail-safe）；`src/checkpoint.rs:398` 同型（恢复路径，
  误读为「当前已删」≈恢复语义本身，delete 有 exists() 护栏）——边界，暂不立单。
- **协议允许的缺省**：`src/mcp.rs:1064` content 缺省 → 空串
  （MCP CallToolResult.content 可缺省）。
- **fail_mode 分类已兜底**：`src/hooks/external.rs:838-839`/`:891-893` hook
  stdin 写失败吞（EPIPE）——spawn 失败/超时/不可解析输出已由 fail_mode 分类
  处理（审批缺省 fail-closed），早退 hook 通常非零退出。边界：若后续实证
  「早退+成功输出」形态，并入 #234 类修。
- **显式失败面**：`self-improve.sh:987/1078` `cargo build 2>/dev/null` 有
  `set -euo pipefail` 兜底（失败=显式中止）；`ci-mutants-gate.sh:136-138`
  gh 查询空态进 TIMEOUT→exit 3 显式失败；`acp/permission.rs:178` 超时静默=deny
  （fail-closed 方向）。
- **显式哨兵值**：`src/tools/a2a.rs:521-522` A2A 轮询探针
  `2>/dev/null || echo UNKNOWN`——失败显式落哨兵 `UNKNOWN` 供上游判读，
  非静默错值。
- **清理尽力而为（脚本面）**：`src/tools/transport_layer/container_transport.rs:543-547`
  超时清理 `kill -TERM/-KILL 2>/dev/null`——目标已消亡时 kill 报错属预期，
  无行为替换。
- **测试专用面**：`crates/tui-pty-harness`（仅测试基建）、全部 `#[cfg(test)]`
  块与 `*tests*.rs`。
- **providers 双保险**：e2b/container provider 拒绝静默回退（eprintln+exit 2，
  transport_layer 两处）——无回退即无降级。

## 维护约定

1. 新降级站点**随修随登记**：修复 PR = 代码 WARN/metrics + 回归钉 + 本页行
   （状态 ❌→✅ 或新增行）。
2. 扫描新违例：只登记 + 逐条立单（`fix(observability):` 前缀，正文引本页与
   owner ruling）；「一行 WARN + 一条测试」量级且主控批准的可随单带
   （426 S3(b) 先例）。
3. 回归钉口径：观测本体可被变异杀掉才算钉住（#223 批 2 tracing 捕获层先例，
   capture_logs/`tracing::subscriber::with_default` 线程内 subscriber）。
4. 状态口径见页首四态表；「👁 无钉」是补钉欠账（#228 批次同款轨道可承接）。

— 首版：Goal 428 S4（2026-10-25），基线 `20e9ac84`+`880cff08`，扫描方式与
逐条判定记录见 `.dev/journal/manual-20261025-goal428-s3-s4.md`。
