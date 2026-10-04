# Manual journal — 2026-10-04 — issue #105 第四轮评审（1 blocking + 5 non-blocking）

## Date
2026-10-04

## Goal
run `pipeline-105-1004132600` 的 rev3 评审给出 `NEEDS_FIX`：1 条 blocking
（`next_after` 在 dom/dow 受限时丢窗口）+ 5 条 non-blocking。本轮逐条处理，
不改任何既有契约。

## Files touched

- `src/triggers.rs`
  - `next_after`（blocking）：day fast-forward 由 `candidate += 86_400`
    改为 `candidate = (candidate.div_euclid(86_400) + 1) * 86_400`。
    旧写法保留扫描当下的 time-of-day，而下面 hour/minute 扫描只能向前走，
    于是「在匹配日的 15:00 才走到该日」会跳过该日更早的 09:00 窗口，整周期
    丢掉（`0 9 * * 5` from Wed 15:00 返回下周五而非本周五；
    `0 2 * * 1`、`0 0 1 * *` 同理）。`day_ok` 与 time-of-day 无关，所以
    「下一个午夜」就是更晚某日的最早可能匹配点；月跳（`jump_to_next_month`）
    本来就归午夜，不受影响。
  - 新增 3 条回归测试：非午夜 `from` 的三个定点用例、
    「任意整点起算，周表达式 ≤ 7 天」的整周性质测试、
    `advance_cron` 周级重启场景的 store 级测试。

- `src/http/triggers.rs`
  - `stamp_result`：`TriggerStore::load()` 失败不再静默 `return`，
    加 `tracing::warn!`（带 trigger_id / result）；`save()` 失败也补 warn。
    scheduler 路径上一轮已加同类日志，这条补上 fire 路径，否则「9 点那次
    跑完到底发出去没有」在日志里查不到。
  - `create_trigger`：notify 校验从「只查 `File` 路径」扩到
    `Webhook`——新增 `validate_notify_webhook_url`，要求 URL 可解析且
    scheme 为 http(s)。坏 URL 现在在注册时 400，而不是等到第一次 fire
    才发现。
  - 明确不套 `WebFetch` 的 SSRF guard，理由写在函数 doc：notify 目标是
    **已认证调用方**自己给的 sink（他本来就能通过 session 跑任意命令），
    而监听在 loopback/LAN 的自建接收端是正常部署——与 `HttpCall` 把内网
    base_url 放在显式 `allow_private` 后面的判断一致。

- `src/notify.rs`
  - `WEBHOOK_TIMEOUT` 抽成常量，`send_webhook` 里额外
    `RequestBuilder::timeout(WEBHOOK_TIMEOUT)`：builder 失败回退到
    `Client::default()`（无默认超时）时，请求本身仍有 15 s 上限，
    兑现 `new()` 的「must not hang」承诺；回退路径补 `tracing::warn!`。
  - 新增 `shared_notifier()`（`OnceLock<HttpNotifier>`）供 turn / trigger
    两条路径复用，不再每轮 turn 新建 `reqwest::Client`（丢连接池 + 重建成本）。

- `src/http/handlers.rs` / `src/http/mod.rs`
  - `send_session_message` 的 notify 注释、`SessionMessageRequest::notify`
    的 doc、OpenAPI 里 `notify` 的 description：`fire-and-forget` 改成
    `best-effort / non-fatal`，并写明投递仍在请求路径上（响应字段
    `notify_result` 就是投递结果，spawn 掉就没有这个字段了），但被 15 s
    请求超时和共享 client 兜住。

- `tests/http.rs`
  - 新增 `create_trigger_rejects_malformed_notify_webhook_url`：坏 URL 400
    且不落盘，合法 URL 仍 201。

- `.dev/journal/manual-20261004-issue105-review-fix.md`
  - 更正 stale doc：weixin 修复实际是 `switched` + `active_session` 两个变量
    （`ae3d9083`，避开 `clippy::option_option`），不是初稿的三态
    `Option<Option<String>>`。

## Tests added
- `triggers::tests::next_after_keeps_a_matching_days_earlier_window`
  （3 个非午夜定点：`0 9 * * 5` / `0 2 * * 1` / `0 0 1 * *`）
- `triggers::tests::next_after_is_within_one_cycle_from_a_non_midnight_start`
  （一周内每个整点起算，周表达式必须 ≤ 7 天且严格向后）
- `triggers::tests::advance_cron_keeps_weekly_schedules_within_one_cycle`
- `http::triggers::tests::notify_webhook_url_validation`
- `tests/http.rs::trigger_endpoints::create_trigger_rejects_malformed_notify_webhook_url`

回归验证：把 day 分支临时改回 `candidate += 86_400` 后，前两条新测试
立即失败（`0 9 * * 5` from Wed 15:00 得到 `2026-01-16` 而非 `2026-01-09`；
09:00 起算得到 11 天外），改回后全绿。store 级那条依赖真实 `epoch_now()`，
不是每次都能抓到旧 bug（起算时刻在该周窗口之前时旧实现也正确），
所以重复的定点断言放在 `next_after` 层。

## Gates (run in this worktree, by hand)
- `cargo fmt --all -- --check` → clean
- `cargo clippy --all-targets --all-features -- -D warnings` → clean（exit 0）
- `cargo test --workspace --no-fail-fast` → exit 0，58 个 target 全 `test result: ok`，
  0 failed（含评审提到的 `tools::execution::shell::tests::timeout_kills_child_process`，
  本轮未复现 load-flake）
- 定点复跑：`cargo test --lib -- triggers::` 49 passed；
  `cargo test --test http -- create_trigger_rejects` 2 passed

## Notes / traps hit
- 这条 blocking 的隐蔽点：所有既有单测要么从 `00:00` 起算，要么正好从
  触发分钟起算，两者都让「保留 time-of-day」和「归午夜」等价；只有
  「先走到匹配日、且当时已过该日窗口」才暴露。回归测试必须用非午夜 `from`。
- `notify_result` 是**已发布契约**（响应字段 + OpenAPI + `tests/http.rs`
  断言 + 落盘 sink 立刻可读），所以评审建议的 `tokio::spawn` 不能照搬——
  spawn 掉就等于删字段，属于回退既有行为。改为：保留同路径投递 + 15 s 硬上限
  + 共享 client，并把注释/OpenAPI 从 `fire-and-forget` 改成 `best-effort`，
  让文档与实现一致。真正的 `fire-and-forget` 语义在 `/webhooks/{id}` 与
  scheduler 那条路径上（`fire_webhook` 返回 202 后 `tokio::spawn`）。
- SSRF：notify 的 URL 来自已认证的调用方，且 loopback/LAN 接收端是自建部署的
  常见形态；套 `url_guard` 会把合法部署挡在注册之外。折中为「格式/scheme 早
  校验、私有地址不拦」，并把理由写进代码而不是留在评审里。
