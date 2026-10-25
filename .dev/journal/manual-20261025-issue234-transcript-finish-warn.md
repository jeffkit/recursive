# 2026-10-25 — #234 transcript finish() 定稿吞（CLI 迁移 + TUI shutdown）→ WARN 可观测

**[10:2x] 交付行（wt-b1 席）**：两处会话定稿口吞 `finish()` 结果——CLI
`cmd_session_migrate_legacy` 的 `writer.finish(Interrupted).ok()`（append 走
`?`、定稿失败被吞后照样打印「Migrated to: …」成功话术）/ TUI `worker_loop`
Shutdown 臂 `let _ = sw.finish(Completed)`——SessionStatus 静默失真，后续
resume/审计读到错的终态 → 各抽最小观测缝（`finalize_migrated_session` /
`finalize_session_on_shutdown`），缝内 `if let Err` + `tracing::warn!`
（session_dir+error，「… keeps no final status marker (degradation)」）；
定稿仍 best-effort、控制流零改动。3 测试新增（CLI 1+正控内含 / TUI 2；
`.meta.json` 目录占位注入 finish 读改写必败，平台中性无 cfg 按 #224 口径；
env 按 #222 走 cli `pinned_home`/kernel `PinnedRecursiveHome`），负控缝内
还原吞 → 钉红 got: 空、正控绿，`cmp` 逐字节还原。新增 dev-dep
`tracing-test = "0.2"` ×2（两 crate 首次进程内日志捕获，为钉观测本体；
lock 仅两接线无新包）。登记页 #234 ❌→✅。三门无管道全绿（fmt --check
EXIT=0 / clippy --all-targets --all-features -D warnings EXIT=0 /
test --workspace EXIT=0 5295/0）。未 push、未动 main。等主控验收。

## Date

2026-10-25（wt-b1 席，基线 aa8a5cb7；#232 后同工单第二单）

## Goal

#234（P2，fix(observability)，Goal 428 S4 扫描立单，规格=issue 本体）：
`crates/recursive-cli/src/cli/session.rs:175` 与
`crates/recursive-tui/src/backend.rs:888` 两处 `finish()` 结果静默丢——
两处都是会话持久化定稿口，失败即状态/终局标记丢失。

## Files touched

- `crates/recursive-cli/src/cli/session.rs`
  - 新缝 `finalize_migrated_session`（:202）：finish+WARN；调用线 :175 原
    `.ok()` → 缝调用；doc 记裁决（transcript 已落盘故非硬错，但无终态必须可痕）。
  - tests：`migration_finish_failure_warns_and_healthy_finish_stays_silent`
    （:697，内含正控）。
- `crates/recursive-tui/src/backend.rs`
  - 新缝 `finalize_session_on_shutdown`（:618）：take+lock+finish+WARN；
    Shutdown 臂（:906，原 :886-891 内联三行）→ 缝调用；毒锁静默跳过为
    既有形态未动（#234 规格只指 finish 结果）。
  - tests：`shutdown_finalize_warns_when_finish_fails`（:3819）/
    `shutdown_finalize_healthy_finish_is_silent_and_completes`（:3842，正控）。
- `crates/recursive-cli/Cargo.toml` / `crates/recursive-tui/Cargo.toml`：
  dev-dep `tracing-test = "0.2"`（各带用途注释）；`Cargo.lock` 两接线。
- `docs/architecture/degradation-registry.md`：B 节 #234 行就地翻转 ❌→✅ 有钉。

## Tests added

| 测试 | 钉什么 |
|---|---|
| `migration_finish_failure_warns_and_healthy_finish_stays_silent`（cli，:697） | 先正控：健康 finish 静默且 meta.status 实测 `Interrupted`；再 `.meta.json` 目录占位（finish 的 read-modify-write 读目录必败）→ WARN 含「legacy session migration: finish failed」。杀缝内回吞 `let _ =` |
| `shutdown_finalize_warns_when_finish_fails`（tui，:3819） | 同注入 → WARN 含「TUI session shutdown: finish failed」；slot 仍被 take（shutdown 行为零改动） |
| `shutdown_finalize_healthy_finish_is_silent_and_completes`（tui，:3842，正控） | 健康 finish 静默且 meta.status 实测 `Completed` |

**注入口径（#224）**：目录占位注入平台中性，无 cfg 门；env 口径（#222）：cli
钉走本 crate 既有 `pinned_home()`（EnvGuard 锁 + RECURSIVE_HOME 钉 +
RECURSIVE_SESSIONS_DIR 移除），tui 钉走 kernel `test_util::PinnedRecursiveHome`
（锁 + RECURSIVE_HOME 钉 + RECURSIVE_SESSIONS_DIR 移除），两 crate 测试均触
`user_sessions_dir` 的 env 读面，故必须隔离。

**负控**：① cli——缝内 Err 臂临时还原为 `let _ = writer.finish(...)`（等价
旧吞）→ 钉红 `a failed finish must be reported as a degradation`（logs 空）；
② tui——缝内还原 `let _ = sw.finish(...)` → 失败钉红、正控绿（设计如此）。
还原后与备份 `cmp` 逐字节一致。另注：只还原 :175 调用线（`.ok()`）钉不红——
钉的是观测本体（缝），生产接线为单调用线，与 #231「接线一行不在进程内可测面」
同口径，登记页已注记。

## Notes

1. **抽缝理由**：finish 失败注入需在 create 与 finish 之间动手，而
   `create_with_tools` 建档即写 `.meta.json`、函数体内不可外部拦截——直接测
   `cmd_session_migrate_legacy` 无法到达失败臂；#231 `file_config_or_report`
   先例：抽最小缝 + 缝级钉观测本体。
2. **调用方 blast radius**（GitNexus MCP 本席不可用，手工 grep 核）：
   `finalize_migrated_session` 生产调用方唯一 `cmd_session_migrate_legacy`
   （:175）；`finalize_session_on_shutdown` 生产调用方唯一 `worker_loop`
   Shutdown 臂（:906）。两缝 WARN-only，零控制流改动；`recursive sessions
   migrate-legacy` 与 TUI Ctrl-C/Shutdown 的用户可见行为不变。
3. **dev-dep 依据**：`tracing-test` 已在 root dev-deps（#233 先例），本次为
   两 sub-crate 首次使用；dev-only、无新包入 lock（仅两处依赖接线），
   「无依据依赖」invariant 的依据即回归钉本体。
4. 三门命令无管道（`> log 2>&1; echo EXIT=$?`）：fmt --check EXIT=0 /
   clippy EXIT=0 / test --workspace EXIT=0（5295 passed / 0 failed，含本单
   3 钉）；log 在 `/tmp/gate-{fmt,clippy,test}-b1.log`。
