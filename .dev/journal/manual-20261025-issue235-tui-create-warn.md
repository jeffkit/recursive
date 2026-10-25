**[13:1x] 交付行（wt-b1 席）** #235 fix(observability) 落地：TUI `SessionWriter::create` 失败静默禁用整场持久化 → WARN 可观测（观测缝 `create_session_writer_or_warn` backend.rs :640，两 goals 入口 :888/:942 共接）；2 钉全绿、负控实证红转绿、三门无管道全绿（fmt --check EXIT=0 / clippy --workspace --all-targets --all-features -D warnings EXIT=0 / test --workspace EXIT=0 5300 passed / 0 failed / 12 ignored）；登记页 #235 ❌→✅；commit 留 wt-b1 待主控验收。

- **Date**: 2026-10-25
- **Goal**: #235（P2，主控轮值注入）——backend.rs 两 goals 入口（issue 基线 :843/:903，本 HEAD :863/:919）`if let Ok(sw) = SessionWriter::create(&ws, …)` 隐式 else：create 失败（工作区不可写/磁盘满等）整场 TUI 会话持久化静默关闭，无 warn/error，运行如常；违反 owner ruling「任何降级必须可观测」。
- **Files touched**:
  - `crates/recursive-tui/src/backend.rs`——抽最小观测缝 `create_session_writer_or_warn(ws, goal) -> Option<Arc<Mutex<SessionWriter>>>`（:640，紧邻 #234 的 `finalize_session_on_shutdown`）：缝内 `match SessionWriter::create`，Err 臂 `tracing::warn!(workspace=%ws.display(), %error, "TUI session persistence disabled: SessionWriter::create failed (degradation)")` + 返回 `None`；`StartLoop`/`SendMessage` 两调用线改 `if let Some(sw_arc) = create_session_writer_or_warn(...)`，sink 装配与 `set_event_sink` 原样留在调用线，控制流零改动（`None` 即原隐式 else 的「无 writer」行为）。`use std::path::Path` 新增。
  - `docs/architecture/degradation-registry.md`——#235 行 ❌→✅（站点行号/观测方式/回归钉列更新）。
- **Tests added**（backend.rs `mod tests`，:3897/:3925）:
  - `session_writer_create_failure_warns`——注入：`PinnedRecursiveHome` pin home（#222）后先用公共 `recursive::paths::user_sessions_dir(ws.path())` 解析出真实 sessions 路径，再以普通文件占位（remove_dir + write），`create_dir_all` 穿文件必败，平台中性无 cfg（#224 口径）；断言返回 `None` + WARN 语义句「TUI session persistence disabled」+ workspace 路径字段进日志（钉观测本体，同 #234 口径）。
  - `session_writer_create_healthy_is_silent`——正控：健康 workspace 返回 `Some` 且无新告。
- **负控实证**: 缝内 Err 臂还原吞（`Err(_error) => None`）→ `session_writer_create_failure_warns` 红（`logs_contain` got: 空，panic 于 :3905）、正控绿；`cmp` 逐字节还原后 2/2 绿。
- **Notes**:
  1. 变异面：backend.rs 刚过 e4ef3c67 变异审计，新缝代码会进下一次 tui-mutants scope——钉断言了语义句 + workspace 字段 + `None`/`Some` 返回，去 warn / 去 field / 改返回值的 mutant 均可杀；本地 mutants 未跑（§2b）。
  2. 观测缝直测而非走 worker_loop 进程内接线（同 #234/#231 注记：生产接线为单调用线，进程内不可测面）。
  3. GitNexus MCP 不可用（本席工具面无 `gitnexus_*`），爆炸半径手工核：`SessionWriter::create` 全仓调用点 grep 仅 backend.rs 两处改接线 + TUI 内既有测试（#234 两钉、#383 interrupt 测试走 SendMessage 健康路径，行为不变），seam 为纯新增 fn、无其它调用方。
  4. test --workspace 5300 = 5298（e4ef3c67）+2 新钉。
