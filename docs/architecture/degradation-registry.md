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
| `src/http/session_mirror.rs:153-154`（唯一调用方 mod.rs:2414 同静默） | AGUI teardown 镜像 transcript/meta 双 atomic_write 吞——镜像缺失/陈旧无痕（活路径 #147 有 ?，本站是镜像面） | 无 | 无 | S4 扫描 | ❌ [#232](https://github.com/jeffkit/recursive/issues/232) |
| `src/acp/session.rs:329-344`（insert_with_id 两 Err 臂 :329/:337，原 :327-328 整吞） | ACP 建档初始 metadata 持久化吞——重启 resume 静默丢会话 → create_dir_all 与 save_metadata_to_disk 两个 Err 臂各打 WARN（session_id+pdir+error），会话仍入内存继续服务（行为不变，仅补观测）；save 内部 create_dir_all 吞为良性不动 | `tracing::warn!`「ACP session metadata persist failed; resume will lose this session (degradation)」 | `insert_with_id_warns_when_initial_metadata_persist_fails`（同文件 :1076；traced_test/logs_contain 钉观测本体——负控还原吞错即红已实证；内含正控：健康插入静默且 metadata.json 落盘） | S4 扫描 → #233 | ✅ 有钉 [#233](https://github.com/jeffkit/recursive/issues/233) |
| `crates/recursive-cli/src/cli/session.rs:175` + `crates/recursive-tui/src/backend.rs:888` | transcript `finish()` 定稿吞（迁移路径 `.ok()` 继续+报成功话术 / TUI shutdown `let _ =`）——SessionStatus 静默失真 | 无 | 无 | S4 扫描 | ❌ [#234](https://github.com/jeffkit/recursive/issues/234) |
| `crates/recursive-tui/src/backend.rs:843`/`:903` | `if let Ok(sw) = SessionWriter::create` 隐式 else——create 失败整场 TUI 持久化静默关闭 | 无 | 无 | S4 扫描 | ❌ [#235](https://github.com/jeffkit/recursive/issues/235) |
| `src/session/writer.rs:670`（update_identity）`:697`（set_owner）及同族 set_derived_from/bump_updated_at | `.meta.json` 读改写 best-effort 族吞错——陈旧 model 影响取价（owner 缺失方向 fail-closed）；家族成文 best-effort，但按 ruling 仍须可观测 | 无（doc 自认 best-effort） | 无 | S4 扫描 | ❌ [#236](https://github.com/jeffkit/recursive/issues/236) |
| `src/weixin/daemon.rs:162` | 入站微信消息桥接 send 吞——processor 死亡期用户消息静默丢（实验通道降 SLA 不降观测门，channels.md） | 无（成功路径仅 debug!） | 无 | S4 扫描 | ❌ [#237](https://github.com/jeffkit/recursive/issues/237) |
| `.dev/scripts/self-improve.sh:1134-1135` | review 门三重 fail-open：脚本失败→approve 字面量 / 解析失败→approve / 缺键→approve（脚本已 deprecated，留档修；内层 review-changes.sh:170 有可见 marker，外两层全静默） | 无 | 无（脚本面无单测） | S4 扫描 | ❌ [#238](https://github.com/jeffkit/recursive/issues/238) |
| `src/compact/mod.rs:315`/`:707` | provider 缺 usage → `unwrap_or_default()` 静默按 0 token 记账——预算/成本双少计；:311 info! 只覆盖 structured 回退 | 无 | 无 | S4 扫描 | ❌ [#239](https://github.com/jeffkit/recursive/issues/239) |
| `src/llm/pricing.rs:241`（RetryRecords::take） | poison 锁 → 整批 retry/计费记录静默清零 | 无 | 无 | S4 扫描 | ❌ [#240](https://github.com/jeffkit/recursive/issues/240) |
| `.dev/scripts/agent-mutants.sh:118` + `cli-mutants.sh:93`（EXIT trap） | 恢复臂 `git checkout -- 2>/dev/null \|\| true` 吞——变异体源文件可能静默留树（失败模式 #8/#9 之外的新形态）；红线：退出码语义不动，只补显式报错 | 无 | 无 | S4 扫描 | ❌ [#241](https://github.com/jeffkit/recursive/issues/241) |

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
