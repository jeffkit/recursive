# 2026-10-01 — Issue #57: AG-UI thread 落地为原生 session

## Date
2026-10-01

## Goal
#57（P0，架构缺陷系列 5/9）：AG-UI 另起一套更弱的 session——不写 `.meta.json` 对
`episodic_recall` / `sessions list` / resume picker 完全不可见；不落成本；裸 `std::fs`；
无 per-thread 锁；`threadId` sanitizer 碰撞。okguitar 已端到端实测确认前两项
（`verify-session-visibility.sh` / `verify-thread-concurrency.sh`，见 issue 评论）。

## Files touched
- `src/agui_session.rs`（新增）：thread→session 映射。`thread_session_key`（blake3 前
  16 hex，替代有碰撞的 `-` sanitizer）、`session_dir`（`<sessions>/<slug>/agui-<key>/`
  原生布局）、`resolve_session_dir`（旧扁平目录惰性迁移：拷贝三件套 + raw Message 行
  重写为 TranscriptEntry + 合成 meta）、`persist_run`（走 SessionWriter + CostTracker）、
  `apply_resume_tool_results`（resume payload 回写磁盘，防止下次 resume 播种 deny 文本）。
- `src/session/writer.rs`：`create_at` / `open_or_create`（显式目录、目录名即 session_id）、
  `add_usage`（run 级 usage 不挂具体条目）、`update_identity`（meta 的 model/provider/preset
  跟随最近一次 run）。
- `src/session_host.rs`：`try_begin_run` + `ActiveRunGuard`（per-session 运行栅栏，Drop/
  unwind 释放）——issue §④ 的「按 threadId 串行化」用 fail-fast 409 实现（排队重复 run
  会把同一 prompt 跑两遍，比立即拒绝更糟）。
- `src/http/handlers.rs`：`/agui` 入口接栅栏（409）；checkpoint 链 id/日志迁移进 session
  目录；driver 持久化改走 `persist_run`（只追加本 run 新消息，`drv_pre_run_len` 防止
  resume 时把 seed 重复落盘；interrupt → Interrupted，Ok → `for_finish`，Err → Crashed）；
  resume 分支把 payload 修补同步到磁盘；删除 `sanitize_thread_id_for_session`（移入
  `agui_session::legacy_sanitize_thread_id` 仅作旧目录查找）。
- `src/lib.rs`：注册 `pub mod agui_session`。
- `tests/agui_e2e.rs`：3 个新 e2e——可见性（list_sessions 能看到 AG-UI 线程 + meta/cost
  落盘 + load_transcript 可读）、同 thread 并发 409（BarrierProvider 钉住 in-flight）、
  interrupt→resume 全链路（含 invariant #8 配对跨持久化边界、payload 落盘、状态翻转）。

## Tests added
- 单测 12：`agui_session` 9（key 确定性/防碰撞/charset、原生布局可见性、跨 run 追加与
  成本累加、无 usage 跳过 cost.json、旧目录迁移+幂等、迁移后追加、resume 修补、旧
  sanitizer 防穿越）、`writer` 3（open_or_create 二段行为、add_usage、update_identity）、
  `session_host` 2（栅栏互斥+Drop 释放、panic unwind 释放）、`handlers` 2（key 目录安全、
  碰撞对）。
- e2e 3（见上）。

## Notes
- 老线程迁移是**拷贝**不是移动：旧扁平目录保留（无 meta，不影响 list；老二进制仍可
  resume），有需要再做一次性清理。
- checkpoint 链 id 从 sanitized-thread 改为 `agui-<hash16>`：修复前的旧 checkpoint refs
  在 shadow-git 里仍存在但不会被新链续上（可接受——修复前碰撞的线程本就共享一条链）。
- 已知限制：intra-run compaction 触发时 `drv_pre_run_len` 切片会失准（文件里多一条
  summary 行、不丢 run 内新消息）；AG-UI 路径目前未观测到 mid-run compaction，留待
  #56 分层时一并处理。
- **未做（有意）**：§③ StorageBackend/水平扩展——依赖 #56（AG-UI 服务端分层），
  `ThreadStore` trait 等 #56 落地后再接，避免在 654 行 handler 里再叠抽象。
- 三门全绿：`cargo test --workspace`、`cargo clippy --all-targets --all-features -- -D
  warnings`、`cargo fmt --all`。
