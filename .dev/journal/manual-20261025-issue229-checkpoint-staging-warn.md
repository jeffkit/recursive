# 2026-10-25 — #229 checkpoint.rs staging 失败静默错账 → WARN 可观测

**[09:xx] 交付行（wt-b2 席）**：`write_workspace_tree` 的 `git add -A --force` 原
`.output()` 整体 `let _` 丢弃、`status.success()` 不查——staging 失败后 write-tree
对空/部分 index 照样成功，`checkpoint diff` 静默错账（「全删除」/「无变更」）→
status 检查 + `tracing::warn!`（含 `stderr_tail` 截 200 字符的 stderr 尾），diff 保持
best-effort 不改控制流。sibling `snapshot_for_session` 的同型静默洞同批补：其 Err 臂
本就有检查（登记页「有检查」属实），但 stderr 为空 / 含 nothing-to-commit / warning:
时穿透即无声——容忍臂穿透补 WARN「snapshot may be partial」，Err 臂语义零改动。
2 测试新增（0555 注入 staging 失败→WARN 断言 / 健康正控不告），负控条件取反还原
静默 → 钉红 `got: `（空日志），还原后 `cmp` 逐字节一致。登记页 #229 ❌→✅。
未 push、未动 main。等主控验收。

## Date

2026-10-25（wt-b2 席，基线 85bac8fb = origin/main，开工 fetch+reset 树净、登记页核
两单均 ❌ 无双开）

## Goal

#229（P1，fix(observability)，Goal 428 S4 扫描立单）：`src/checkpoint.rs:648`
`write_workspace_tree` 把 staging 的 `git add` 输出整体丢弃——任何 staging 失败
（shadow 目录不可写 / git 环境异常）都让 diff 基于空/部分 index 出账且零痕迹；
同文件 `snapshot_for_session` 容忍臂穿透同型。

## Files touched

- `src/checkpoint.rs`
  - `write_workspace_tree`（:653，原 :648）：`let _ = ….output()` → `add_out` 接住
    （`.map_err(git_err)?`——spawn 失败从「下游 write-tree 必然同样失败」提前到这里，
    净结果同为 diff Err，路径更短）+ `!status.success()` 臂 `tracing::warn!`
    「checkpoint diff: git add failed; diff output may misreport workspace changes」，
    `stderr = %stderr_tail(...)` 字段。
  - `snapshot_for_session`（:196-221）：既有 `if !add_out.status.success()` 块的
    容忍臂（stderr 空 / 含 "nothing to commit" / 含 "warning:"）穿透处补
    `tracing::warn!`（session + stderr 尾，「snapshot may be partial」）；
    Err 臂与容忍集合零改动——checkpoint 建档不因 git 噪声硬失败的既有语义保留。
  - 新私有 `stderr_tail(&[u8]) -> String`（:826 起）：尾 200 字符（char boundary
    对齐），WARN 载荷有界。
  - tests：`LogCapture`/`capture_logs`（同 #199 S3(b) config_file.rs / #223
    hardening.rs 范式，thread-local `with_default`，无全局 subscriber、无 env 改动）
    + 2 条测试（见下）。
- `docs/architecture/degradation-registry.md`：B 节 #229 行就地翻转 ❌→✅ 有钉，
  填观测方式 / 回归钉 / 新行号，补记 snapshot 容忍臂同批处置。

## Tests added（src/checkpoint.rs，mod tests）

| 测试 | 钉什么 |
|---|---|
| `diff_staging_failure_warns_instead_of_silent_misaccounting`（cfg(unix)） | staging 注入失败（健康 snapshot 后 shadow_dir 0555 → `tmp-index-diff.lock` 不可创建，而 expand_sha 只读 rev-parse 不受影响，失败恰好落在 staging 步）→ diff 仍 Err（既有控制流）+ WARN 含「git add failed」+ stderr 尾「Permission denied」。杀回改 `let _ = ….output()` |
| `diff_healthy_staging_stays_silent`（正控，跨平台） | 健康 diff → Ok 且 0 WARN——修复不给正常路径加噪声 |

**负控**：WARN 臂条件取反（`!success → success`，等价失败静默）→ 钉红
`staging failure must emit a WARN, got: `（空日志）；还原后与备份 `cmp` 逐字节一致。
注入方式曾试删 `objects/`（git 会 safe_create_leading_directories 自愈重建，add 不败）
——弃，改 0555（`paths.rs:287` 同手法先例；cfg(unix) 按 #224 口径）。

## Notes

1. **登记页勘误**：扫描登记原文「sibling `snapshot_for_session` 有检查」属实
   （Err 臂 :196 起），但其容忍臂穿透即无声——同型静默洞在容忍臂，不在「无检查」。
   本单同批补 WARN，Err 臂未动。
2. **WARN 而非 Err 的口径**：任务单明示「检查 success() 并 WARN」。diff 是观测面
   命令，fail-loud 改控制流超出观测性授权；staging 失败 + WARN 后 diff 仍可能出
   错账，但降级已可观测（owner 裁决的最小满足）。
3. **snapshot 容忍臂 WARN 无独立回归钉**（残口披露）：到达该臂需 git 以「空 /
   informational stderr」非零退出，无 env 注入（PATH shim 需动 env，违本测试模块
   无 env 并行约定）无法自然到达；其 warn! 与 write 路径共用 `stderr_tail` + 同形
   消息，write 路径钉住观测本体。登记页如实写「容忍臂同批补」，钉列仅 write 路径。
4. GitNexus MCP 本会话不可用（工具表无 gitnexus_*），blast radius 手工核：
   `write_workspace_tree` 唯一调用方 `diff`（:452）；`snapshot_for_session` 调用方为
   runtime 落检点面（行为零改动，仅新增 WARN）；`stderr_tail` 新函数无其他调用方。
5. 三门命令无管道：日志 + exit 落盘 `/tmp/b2-229/gate-*.{log,exit}`，`;` 隔开；
   判定比对 .exit 与 .log mtime 防旧文件冒充（#231 journal Note 4 教训）。
