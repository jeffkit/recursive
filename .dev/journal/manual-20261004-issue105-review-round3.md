# Manual journal — 2026-10-04 — issue #105 评审第三轮（10 条 minor 逐条处理）

## Date
2026-10-04

## Goal
run `pipeline-105-1004102032` 的 rev2 评审给出 2 条 blocking + 8 条 minor。
两条 blocking（weixin 首条消息不建会话 / 切换失败残留旧 sink）已由 `ae3d9083`
修掉；本轮逐条处理剩余 8 条 minor，并重跑三个质量门。

## Files touched

- `src/http/triggers.rs`
  - #3 listing 泄漏 webhook key：`TriggerResponse::from_trigger` 的
    `webhook_path` 改为无 key 的 `/webhooks/{id}`；只有 `create_trigger`
    在 echo secret 的同时补上带 key 的完整 URL（`webhook_path(&trigger)`）。
    字段 doc / 函数 doc 同步改口，单测断言两种形态。
  - #8 OpenAPI 3.0.3 不支持 JSON-Schema `const`：`NotifyTarget` 的两个
    variant 判别字段改为 `{"type":"string","enum":[...]}`。
  - #9 scheduler 每 tick 静默吞掉 `TriggerStore::load()` 错误：失败路径
    加 `tracing::warn!` 后 `continue`（仍然不杀 scheduler）。
- `src/notify.rs` — #4 文档与实现不符：模块头 + `NotifyTarget::Webhook.secret`
  doc 从 `blake3(secret + body)` 改为实现真正使用的
  `hex(blake3_keyed(blake3(secret), body))`，并指向 `webhook_signature`
  作为唯一公式来源。
- `src/triggers.rs` — #7 模块头补一句：`/webhooks/{id}` 挂在受保护路由上，
  `?key=` 是**追加**凭证，调用方还需带服务器 API key（除非显式关掉 auth）。
- `crates/recursive-cli/src/main.rs` — #10 删掉 `state_for_scheduler` 那份
  13 字段 `AppState` 字面量，改为先构造 `state` 再 `state.clone()` 交给
  `spawn_trigger_scheduler`（与 reaper / flush 的既有写法一致）。
- `tests/http.rs`
  - #5 `create_webhook_trigger_echoes_secret_once` 不再断言本地
    `trigger_response_for_test` 复刻实现（那让「listing 不回显 secret」无条件
    通过），改为真跑 `GET /triggers` 与 `GET /triggers/{id}` 断言响应体
    既无 `secret` 也无 key；删除该 helper，新增 `get_json` 小工具。
  - #6 删掉 CRUD 测试里那行结果被丢弃的
    `post_json(.., "/triggers", json!({}))`。

## Tests added
无新增测试用例；#5 把一条假断言换成对真实 handler 输出的断言，
`create_webhook_trigger_echoes_secret_once` 覆盖了 create（带 key）/
list（无 key）/ get（无 key）/ 落盘仍留 secret 四个面。

## Gates (run in this worktree, by hand)
- `cargo fmt --all -- --check` → clean
- `cargo clippy --all-targets --all-features -- -D warnings` → clean (exit 0)
- `cargo test --workspace` → exit 0
- 定点复跑：`cargo test --lib -- triggers` 38 passed / `-- notify` 18 passed /
  `cargo test --test http -- trigger_endpoints` 9 passed

## Notes / traps hit
- `webhook_path` 原先在**所有**响应里带 `?key=`，与「secret 只在 create
  响应回显一次」的承诺直接矛盾。只改注释不改行为会留下真实泄漏面，
  因此选择「listing 去 key、create 补 key」而非改口。
- #5 是典型「测试复刻实现」导致的不变量失效：断言对象是测试自己写的
  序列化逻辑，handler 怎么改都绿。改断言后 #3 的回归才被真正 pin 住。
- 本轮未动 weixin 侧：`weixin` 仍是非 default feature（会拉入
  `wechatbot`/`qrcode`），改默认 feature 不属于本次评审范围，未擅自扩大。
