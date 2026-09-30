# manual-20261001-issue63-http-call

## Date
2026-10-01

## Goal
Issue #63（B2 阻塞项）：为「无 fs / 无 shell」的 C 端形态提供受控的业务 API 调用通路
（原生 `HttpCall` 工具 + `EndpointRegistry`）。基于 `pipeline/issue-70`（PR #72，
其 allow-list 后置装配是本工具收窄的前提，见 #65）。

## Files touched
- `src/tools/http_call.rs`（新增）：EndpointRegistry / EndpointSpec / HttpCall
- `src/tools/mod.rs`：注册模块 + re-export（`HttpCall` / `EndpointRegistry` / `EndpointSpec`）
- `src/tools/url_guard.rs`：`parse_lenient_ipv4` 改 `pub(crate)` 供装载期 guard 复用
- `crates/recursive-cli/src/cli/builder.rs`：`build_tools` 在 skills 之后按
  endpoints 配置注册 HttpCall（无配置 → 不注册，默认工具面不变）

## Tests added
`src/tools/http_call.rs` 内 24 个：config 解析/默认值/非法 method·name·path、
装载期 base_url 分级 guard（loopback/RFC1918 需 opt-in；IMDS/metadata/
unspecified 连 opt-in 也拒绝；lenient IPv4 拼写同杀）、spec 零 URL 泄漏、
side_effect_class 随注册表变化、GET/POST/占位符替换/百分号编码/query 追加、
未知端点与 transport 错误不含 URL、非 2xx 是数据、超长截断、302 拒绝、
auth_env 注入与缺失只报变量名。

## Notes
- 关键设计：模型永远不给 URL，只选端点名（schema enum）；URL 服务端拼装 ⇒
  SSRF 在结构上不存在。redirect `Policy::none()` 补上 url_guard 自认缺口。
- 非 2xx 按「数据」返回（业务语义 404/400 是可推理的答案），transport 错误
  才是 Tool error；idempotent 端点 transport 失败重试一次。
- 一个踩坑：mock server 不 read 请求就 write+close，会因 unread data 触发
  RST，把客户端 send 打成 connection failed——幂等重试掩盖了根因。loopback
  mock 必须 read-then-respond。
- 已知范围：container/policy/microvm 档 ToolSetProvider 未注入本工具（出网
  面另议）；JSON 路径投影（响应裁剪进阶）留作后续。
- 实测闭环（真实 provider）：`RECURSIVE_ALLOW_TOOLS=HttpCall` 下 `/tools`
  恰为 `['HttpCall']`；一轮 AG-UI 对话中模型调用
  `HttpCall(get_order, order_id=A-1001)`，mock 业务端收到
  `GET /api/orders/A-1001`，回答含 已发货/张伟/¥1299/SF1234567890。
  #63 评论中 R3 / R3-e2e / R4-e2e 场景全绿，harness 零改动。
- gates：`cargo test --workspace` ✓ / `clippy --all-targets --all-features
  -D warnings` ✓ / `cargo fmt --all` ✓（在 e33ab02 之上）。
- GitNexus MCP 本会话不可用；影响面以人工 grep 代替（新工具无上游调用方，
  build_tools 为唯一注册点）。
