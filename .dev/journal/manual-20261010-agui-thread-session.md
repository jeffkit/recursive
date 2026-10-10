# manual-20261010-agui-thread-session

## Date
2026-10-10

## Goal
Issue #147 — `threadId` 即会话：HTTP 的两个通道（REST 会话、AG-UI）接上内核**已有**的
事件级持久化（`SessionPersistenceSink`），崩溃窗口从「一个 run」缩到「一条消息」；并把
「线程已存在 ⇒ 服务端 transcript 为准」的 `messages` 规则落地（空 `messages` 合法）。

## Files touched
- `src/agui_session.rs` — `persist_run`（run 末一次性 append）拆成
  `open_thread_writer`（整个 run 持有 writer，与 sink 共享）+ `finalize_run`（只写
  status / finish_reason / error / `cost.json`）。`RunRecord` 去掉 `workspace` /
  `thread_id` / `messages` / `goal`；`.meta.json` 的 `cost` 由 sink 逐条 append 的
  per-row usage 累加，不再二次 `add_usage`（否则翻倍）。
- `src/http/agui.rs` — driver 打开线程 writer、写入 `seed_to_persist`、把
  `SessionPersistenceSink` 加进 `CompositeSink`、run 末 `finalize_run`（不再重放
  transcript）。`prepare_run`：线程已有 `transcript.jsonl` ⇒ seed 取磁盘 transcript，
  请求的 `messages` 只提供本轮 goal；空 `messages` + 已有线程 ⇒
  `CONTINUE_GOAL_DIRECTIVE`（新增）；新线程 ⇒ 保持 #62 的全量 history seeding，并由
  `seed_is_new_history` 标记「这份历史得先落盘」。新增 `load_thread_seed`（丢存储的
  system 行、丢前导孤儿 tool 行）+ `unanswered_tool_calls`（**尾部**孤儿 tool_call 用
  `ORPHAN_SKIPPED_RESULT` 补答，等价 `recursive resume --orphans=skip`）。
- `src/http/handlers.rs` — REST turn 把同一个线程镜像目录开成 live writer
  （`session_mirror::open_live_writer`）并接进 sink；`/agui` 传 `seed_to_persist`；
  `build_session_runtime_parts` 的 #92 注释更新（AG-UI 不走 per-turn 水位线，走
  per-message sink）。
- `src/http/session_mirror.rs` — 新增 `open_live_writer`（`SessionWriter::open_or_create`
  包成 `Arc<Mutex<_>>`，拿不到锁/路径不安全返回 `None`，镜像本就是 best-effort）。
- `src/runtime/builder.rs` — `persist_transcript_per_turn` 文档补充 #147 后的分工。
- `docs/architecture/agui.md` — `prepare_run` / `spawn_agui_run` 两节改写，新增
  “The thread id is the session (issue #147)” 一节（契约 + 代价 + 孤儿补答）。
- `CHANGELOG.md` — Unreleased 增 #147 条目。

## Tests added
- `src/agui_session.rs`：`run_once` fixture 按 driver 的路径驱动（writer → 逐行 append
  → finalize），原有 4 个 `persist_run_*` 测试改为走它。
- `src/http/agui.rs`：`load_thread_seed_answers_a_killed_run_s_unpaired_tool_call`、
  `load_thread_seed_leaves_an_answered_call_alone`；`prepare_run` 的
  `prepare_run_existing_thread_uses_its_own_transcript` /
  `prepare_run_existing_thread_accepts_empty_messages`（工作树中已有）。
- `tests/agui_e2e.rs`：`agui_thread_id_alone_continues_the_conversation`（nonce 只出现
  一次：seed 里一次、磁盘上一次）、`agui_client_seeded_history_is_persisted_before_the_run`。
- `tests/http.rs`：`post_message_mirrors_the_session_transcript_before_teardown`
  （teardown 前镜像已有整轮、无重复行）；`/agui` 的 5 个测试改为「每测试一个临时
  workspace」+ `agui_state` fixture，并把 `agui_request_body` 的 threadId 变成显式参数。

## Notes
- **修红线**：`agui_endpoint_rejects_empty_messages_and_context` 在 #147 后必然红——它
  与同文件的其它 `/agui` 测试共用 `t-test`，而线程存在与否现在是**行为**输入（该
  测试拿到 200）。改名 `..._for_an_unknown_thread`，并让 `/agui` 测试各自带唯一
  thread id；`RECURSIVE_SESSIONS_DIR` 每进程 pin 一次（`/agui` 的会话根没有
  injection point，测试否则会写进开发者真实 sessions store）。
- **顺带发现的 42s**：`/agui` 失败路径会写 fallback checkpoint（`record_turn_failure`
  → `write_failure_checkpoint`），shared fixture 的 workspace 是 `/tmp`，新线程
  key ⇒ 冷 shadow index ⇒ `git add -A` 走完整个 /tmp。实测 41.8s（`sample` 栈：
  `ShadowRepo::snapshot_for_session`）。改用每测试临时 workspace 后该测试 0.08s，
  http 全量 135 项 1.4s。这不是 #147 引入的，但共享 `/tmp` + 新线程 id 会把它挖出来。
- **不做**：不落流式事件（sink 本就只匹配 `MessageAppended` / `MessageAppendedWithAudit`）；
  不加 `mode` 字段；不与 #146 合并。REST 侧仍保留 `persist_transcript_per_turn`
  （那是冷加载读的 flat `StorageBackend` 记录，与镜像目录是两份不同用途的落盘，
  teardown 镜像会用权威快照覆盖 live 写入的同一目录，因此不会重复行）。
- 质量门：`cargo fmt --all --check` / `cargo clippy --workspace --all-targets
  --all-features -- -D warnings` / `cargo test --workspace --no-fail-fast` 全绿。
