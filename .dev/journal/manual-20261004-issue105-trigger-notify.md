# Manual journal — 2026-10-04 — issue #105 触发/通知生态

## Date
2026-10-04

## Goal
issue #105（feat(triggers)）：补齐「定时入口 / 事件入口 / 完成外呼 / weixin 多会话」四块缺失，
让「每天 9 点汇总并微信通知我」这类助理闭环可以在 recursive 服务端搭起来。
基线 HEAD = 8f7fd83c（v2-pipeline-105 分支）。

## Files touched

**新增（domain，transport-free）：**
- `src/triggers.rs` — Trigger / TriggerSpec（cron | webhook）/ TriggerStore（`triggers.json`
  原子落盘）；std-only cron 解析器（`min hour dom month dow`，支持 `*`、`*/n`、`n`、`n-m`、
  逗号列表，dom/dow 双受限时 vixie-cron OR 语义）；`next_after`（分钟粒度、严格晚于 from、
  4 年 horizon 终止）；`advance_cron`（补火语义：从上一个 due 时间向前走，停机 3 天只补 1 次）；
  webhook key 常量时间比对（blake3）；`webhook_key_matches` / `generate_secret` /
  `generate_trigger_id`。
- `src/notify.rs` — NotifyTarget（Webhook | File）/ Notifier trait / HttpNotifier（reqwest
  带 5s connect + 15s request 超时）；webhook 签名 `X-Recursive-Signature: blake3-keyed(secret,
  body)`；File 目标沙箱（限制在 user-workspace dir，词法归一化 `..`，路径逃逸拒绝）；
  `dispatch_notify` / `notify_best_effort`（fire-and-forget，失败只 log 不炸 run）。
- `src/weixin/session_map.rs` — `user_id → session_id` 持久绑定
  （`weixin_sessions.json`，原子写）；`render_session_tail`（/list N，读 full history，
  压缩边界如实显示）；`list_recent_sessions` / `render_sessions_list`（/s，与
  `recursive resume` 同序 updated_at desc）；`resolve_change`（/c N）。
- `src/http/triggers.rs` — 唯一 axum-aware 层：POST/GET/PATCH/DELETE `/triggers`、
  POST `/webhooks/{id}?key=`；`fire_trigger` 共用发火路径（per-trigger fence 走
  `SessionHost::try_begin_run`，webhook 重试 409 不排队）；one-shot run 落盘
  `trigger-run/<uuid>`；`spawn_trigger_scheduler`（30s tick；先 advance 消费窗口再 spawn
  fire，fire 完成只 stamp last_result，保证一窗一发）；OpenAPI paths/schemas 拼接。

**修改：**
- `src/lib.rs` — `pub mod triggers; pub mod notify;`
- `src/http/mod.rs` — 注册 5 条路由到 protected 子路由（auth+rate-limit 内）；OpenAPI
  合并 trigger paths/schemas + `TriggerId` parameter；`SessionMessageRequest.notify` +
  `SessionMessageResponse.notify_result`。
- `src/http/handlers.rs` — `send_session_message` 完成 hook：带 `notify` 时投递本轮
  final_text，结果放 `notify_result`，投递失败不失败请求。
- `src/weixin/daemon.rs` — `/list` `/c` `/r` 全部接上 session_map（删除占位文案与旧
  `list_sessions`/`format_elapsed`）；错误路径（map 损坏/绑定失败）降级为用户可读文案。
- `src/weixin/commands.rs` — HELP_TEXT 更新（per-user 绑定语义）。
- `src/weixin/mod.rs` — `pub mod session_map;` + 文档更新。
- `crates/recursive-cli/src/main.rs` — `recursive http` 启动时：`set_file_context` 绑定
  notify 沙箱根 + `spawn_trigger_scheduler`（30s tick，独立 AppState clone 共享 Arc 字段）。
- `tests/http.rs` — 两个新测试模块（10 个集成测试）：trigger CRUD / webhook 鉴权与发火 /
  scheduler 推进 / session message notify / OpenAPI 覆盖。

## Dependencies added
无新依赖。blake3/uuid/reqwest/serde 均为既有依赖；cron 解析为手写 std-only
（invariant #6：不引入 `cron` crate，issue #105 要求最小实现）。

## Tests added
- `src/triggers.rs` 26 个单测：cron 解析/边界（dow 7→0 归一化后再判星号）、UTC 往返
  （含闰日）、next_after（日滚/星期限制/dom-dow OR/月快进/严格晚于/永火表达式终止）、
  Trigger::is_due 契约、store 往返/损坏即报错/advance 从上次 due 前进、webhook key 契约。
- `src/notify.rs` 10 个单测：file 投递/追加/逃逸拒绝/`..` 穿越拒绝、BadUrl、死端点
  Delivery 错误、**loopback 真实 webhook 往返**（起 TCP server 收请求 + 校验
  X-Recursive-Signature 与 body）、outcome 字符串、签名确定性。
- `src/weixin/session_map.rs` 5+ 单测：绑定往返/重启持久/损坏报错/render 空态与
  missing session（RECURSIVE_SESSIONS_DIR 用 guard 钉住并合并为单测避免 env race）。
- `src/http/triggers.rs` 5 个单测：请求体默认值（enabled=false 契约）、响应序列化
  （listing 不回显 secret、webhook_path 带 key）、OpenAPI 全量。
- `tests/http.rs` 10 个集成：CRUD/400（坏 cron、坏 kind、空 goal）/webhook 403·404·409·
  202 + notify sink 轮询断言 + last_result 落库/scheduler 先 advance 后 stamp 一窗一发/
  OpenAPI paths/schemas/session message notify（含无 notify 字段缺失的向后兼容 pin）。

## Notes / traps hit
1. **测试静态 FILE_ROOT 竞态**：`notify::set_file_context` 是进程级单槽，两个 fire 测试
   并发时后 set 者使先 set 者的 sink 路径被沙箱拒绝（`notify_best_effort` 吞错只留日志，
   表现为 sink 永不出现）。用 `crate::test_util::env_lock()` 串行化并
   `#[allow(clippy::await_holding_lock)]`（与 handlers.rs `agui_prompt_fixture` 同一姿势）。
2. **`..` 词法逃逸**：`absolutise` 若不归一化组件，`root.join("..")/x` 的
   `starts_with(root)` 为 true。现在逐组件折叠 `.`/`..`。
3. **scheduler 双 advance bug（测试抓到的）**：fire_trigger 原本也 advance_cron，
   一次窗口被推两天。改为 scheduler advance（消费窗口）、fire 只 `stamp_result`。
4. **reqwest 无超时**：HttpNotifier 显式 5s/15s（AGENTS.md 网络规则）。
5. **dom/dow 星号判定顺序**：dow `*` 解析为 [0..=7]，归一化 7→0 后必须把 max 也改成 6
   再判「全量 = 星号」，否则 `0 9 * * *` 误入 OR 分支。
6. **env race 纪律**：weixin session_map 里动 `RECURSIVE_SESSIONS_DIR` 的三个断言合并成
   一个 `#[test]`（AGENTS.md env-test 规则）。
7. 触发器默认 **disabled**（`CreateTriggerRequest.enabled` 默认 false），注册→验证→启用；
   webhook 无 key 403、disabled 409、cron id 400 语义分档。

## Verification
- `cargo test --workspace`：58 个 target 全绿（0 failed）。
- `cargo test -p recursive-cli --features weixin`：全绿。
- `cargo clippy --all-targets --all-features -- -D warnings`：0 error 0 warning。
- `cargo build --no-default-features` / `--features http` / `--features weixin` 全部编译。
- `cargo fmt --all -- --check`：clean。

## Review round 2 — NEEDS_FIX fixes (2026-10-04)

独立 reviewer 复核后打回（4 blocking + 5 minor），逐条修复：

1. **webhook 投递在 async 调用方 panic（notify.rs）**：`deliver_request` 原实现先
   `Runtime::block_on`、失败再「换线程重试」，而嵌套 runtime 是 **panic 而非 Err**，
   重试分支永远到不了 → 所有 `notify: {kind:webhook}`（fire_trigger /
   send_session_message 都是 async）必炸。改为**先判上下文**：
   `Handle::try_current()` 有则起独立线程跑自己的 runtime，无则本地 current-thread
   runtime，与 `container_transport.rs` 同姿势；重试路径顺手去掉了无超时的
   `reqwest::Client::new()`（复用带 5s/15s 超时的共享 client）。
   新测试：`webhook_delivers_from_inside_a_tokio_runtime`（`#[tokio::test]` 内投递
   loopback 端点 + 校验 body/signature）、`webhook_from_tokio_runtime_reports_transport_failure`。
2. **weixin `/c N` 绑定被截断的显示 id（session_map.rs）**：`list_recent_sessions`
   原来把 id 截成 12 字符再当 id 用，`/c N`→`/list` 永远「会话不存在」。
   现在返回**完整** session id，截断只发生在 `render_sessions_list` 的展示层。
   新测试：`change_index_binds_the_full_session_id_and_list_finds_it`（真建 session →
   `/c 1` 拿全 id → `find_session_dir` 命中；原 `SessionsDirGuard` 换成
   `test_util::IsolatedWorkspace`，顺带补上缺失的 env_lock）。
3. **cron 补火一窗一发（triggers.rs / http/triggers.rs）**：停机 3 天会按 tick 逐窗
   补火（`* * * * *` 停机数小时 = 成千次 agent run），与模块文档「只补 1 次」矛盾。
   `advance_cron` 改为**从 now 起推进**（collapse backlog）：一次 catch-up、错过窗口
   合并；进程在线只迟到一点时仍是前进一窗。文档同步改成 backlog collapse 语义。
   `next_after` 扫描 horizon 由 366 天修正为真正的 4 年（`HORIZON_DAYS`），与
   「never fires within 4 years」报错口径一致。
   测试重写：`advance_cron_collapses_a_downtime_backlog_to_one_window`（3 天前的窗口 →
   下次严格晚于 now 且 ≤1 天，`is_due` 随之为 false）、
   `next_after_finds_a_leap_day_within_the_horizon`。
4. **per-user 绑定没进消息路径（daemon.rs / main.rs）**：`WeixinRequest` 新增
   `session_id: Option<String>`，daemon 转发普通消息时带上发送者的绑定；headless
   后端比较绑定变化后切换 transcript（`set_transcript` + `open_existing` 续写），
   无绑定则 `WeixinSessionMap::create_bound_session` 建新会话并绑定，
   使 `/c N` / `/r` 的语义（下一条消息切换/新开对话）真正成立。
   `handle_command` 那个没用上的 `_req_tx` 删除。新测试：
   `create_bound_session_makes_a_real_bound_session`。

Minor 一并修：
- webhook key 不匹配 403 → **401**（对齐 handler 文档 + OpenAPI），`ApiError::unauthorized`
  新增；`tests/http.rs` 断言同步。
- `/webhooks/{id}` 真正 fire-and-forget：handler 先取 per-trigger fence（跑着就 409）
  再 spawn，立即 202；`fire_trigger` 改为接收调用方已取的 guard 并返回 `()`。
  新测试：`webhook_fire_returns_409_while_a_run_is_in_flight`。
- `TriggerStore` 文档谎称有 mutex → 改为如实说明 load-modify-save、last-writer-wins。
- `patch_trigger` 重新启用时**总是**重算 next_fire_at（原来只在 `is_none()` 时算，
  长时间停用的 cron 一启用就补火）。
- `stamp_result` 文档去掉「atomically」措辞。

验证（本轮实测）：
- `cargo test --features http --test http -- trigger_endpoints session_message_notify`
  → 11 passed / 0 failed。
- `cargo test --lib -- notify:: triggers::` → 45 passed / 0 failed。
- `cargo test -p recursive-agent --lib --features weixin -- weixin::` → 14 passed / 0 failed。
- `cargo check -p recursive-cli --features weixin --all-targets` → clean。
- `cargo test --workspace` → 全 target 绿（0 failed）。首轮
  `tools::execution::shell::tests::timeout_kills_child_process` 在并发编译抢 CPU 时
  偶发 flake，单独复跑通过，与本改动无关。
- `cargo clippy --all-targets --all-features -- -D warnings` → 0 warning 0 error。
- `cargo fmt --all -- --check` → clean。

未覆盖：`switch_weixin_session` 本体（`#[cfg(feature="weixin")]` 的 CLI 胶水，需要真
AgentRuntime + API config 才能驱动）只做了编译验证；其可测的那半
（`create_bound_session` 建会话 + 绑定）已在 `session_map.rs` 单测中覆盖。
