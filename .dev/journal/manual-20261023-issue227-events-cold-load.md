# manual-20261023-issue227-events-cold-load

- **Date**: 2026-10-23
- **Goal**: #227 — `/sessions/{id}/events` 表查询不触发 cold_load（W1 验收 1 真机形态残余；B1 席，基线 ecd1b471 = origin/main）
- **Files touched**:
  - `src/http/handlers.rs` — `session_events` 的直读表块（`sessions.get(&id)`/None→404）换 `cold_load::get_or_load_session`（与 `get_session`/`send_session_message` 同口径：tombstone 尊重 + ensure_access 先于物化；404 语义对缺失/墓碑/不可恢复转录不变）
  - `tests/issue132_sse_snapshot.rs` — headline `stale_cursor_across_cold_load_gets_snapshot_not_silent_resume` **去掉预热 REST 调用**（`cold_load()` 助手调用移除，`/events` 订阅即重启后首次触碰；watermark 播种/空 ring 断言移到订阅后仍成立）
- **Tests added**: 无新增（headline 形态升级即验收）；`--test issue132_sse_snapshot` 11/11、`--test http` 135/135、`http_cold_load` 11、`issue97_sse_replay` 7，零回退。
- **Notes**: `ensure_access` 直调点在 handlers.rs 其余 8 处保留，无死代码；本单为 422 W1 journal 遗留观察（原第 23 行）闭环。三门与工作区全量见交付时口径。
