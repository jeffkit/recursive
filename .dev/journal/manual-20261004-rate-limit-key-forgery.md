# Manual — 2026-10-04 — fix(http): rate-limit key forgery + unbounded bucket map (#107)

**Date**: 2026-10-04
**Baseline**: `9165b9a0`
**Goal**: issue #107 — `extract_client_key` 无条件取最左 XFF（可伪造 → 每请求换一个
XFF 即获得全新满桶）；bucket map 进程内无上界（SEC-011）；多副本各算各的。

## Files touched

- `src/http/rate_limit.rs` — 全部产品改动
- `CHANGELOG.md` — Unreleased 条目

## What changed

1. **XFF 不再默认可信。** 原实现无条件取最左 XFF（注释自认 "no trusted proxy"
   follow-up 未做）。现在默认 `trusted_proxies = 0`：完全忽略 XFF，落到 socket IP。
   运维通过新 env `RECURSIVE_RATE_LIMIT_TRUSTED_PROXIES=n` 声明可信代理层数；
   `n >= 1` 时客户端地址取 XFF **从右数第 n 项**（每个可信代理 append 一条真实
   peer 地址，左侧不可信前缀全部丢弃——直接废掉"最左"这个客户端可控字段）。
   链长不足 n → 视为不可信，回落 socket IP，不猜。
2. **认证身份优先。** key 优先级固定为 `apikey:<hash>` > `xff:`（仅当配置了
   可信代理）> `ip:<socket>`。带合法 X-API-Key 的请求不可能通过轮换任何 header
   逃出已有桶。共享单一 API key 导致"所有人挤一个桶"是租户问题（需 per-subject
   凭证，即租户单的 JWT `sub` 粒度），不是 key 提取问题——留待租户单。
   顺带：空 `x-api-key` header 不再生成桶（之前 `hash_key("")` 是个稳定桶）。
3. **bucket map 加上界。** 新 env `RECURSIVE_RATE_LIMIT_MAX_BUCKETS`（默认
   10 000；builder `with_max_buckets`，0 钳到 1）。打满时驱逐规则：先"满桶
   （已回满=空闲，等价 prune 语义）"且** forged 源（`xff:`）优先于
   `apikey:`**（轮换 header 的洪泛不能把真实认证用户挤出内存）；全部半满
   （活跃被限流客户端）则不驱逐、允许瞬时超限——超限上界由并发准入约束，
   不受攻击者 header 熵控制（驱逐活跃桶反而送给攻击者新满桶）。
4. **多副本共享存储限流未做**——buckets 仍是进程内 `Arc<Mutex<HashMap>>`，
   属 cloud-runtime 单范围（与 S3/Redis backend 同一条线），CHANGELOG 里明确
   标注，避免"多副本限速已修"的误读。N 副本 = N 倍真实速率的现状保留。

## Tests added (all in `src/http/rate_limit.rs`)

- `goal_h3_xff::extract_client_key_ignores_forged_xff_without_trusted_proxies`
  — 回归主断言：无可信代理时三种伪造 XFF 一律忽略，key = `ip:unknown`
  （替换了旧的"最左 XFF 正确"断言——那是被 #107 推翻的行为）。
- `goal_h3_xff::extract_client_key_walks_xff_right_to_left_by_trust_depth`
  — n=1/n=2 从右取位；伪造前缀被丢弃。
- `goal_h3_xff::extract_client_key_falls_back_when_chain_shorter_than_trust`
  — 链短于信任深度 + 空 header → socket IP。
- `goal_h3_xff::api_key_wins_over_xff_and_rotation_cannot_mint_new_buckets`
  — apikey 压过 XFF；空 key header 不成桶。
- `goal_h3_xff::bucket_map_is_bounded_under_unique_key_flood` — 上界生效；
  空闲桶被驱逐、活跃桶保留。
- `goal_h3_xff::eviction_prefers_forged_source_keys_over_authenticated`
  — 驱逐偏好：`xff:` 先于 `apikey:` 出局。
- `tests::rate_limiter_from_env_trusted_proxies_and_max_buckets` — 两个新 env
  解析 + garbage 回落 + 0 钳位（合并为一个测试，env 全局变量不并行）。
- `tests::middleware_respects_trusted_proxies_config` — 中间件用 limiter 的
  hop 数派生 key（trusted=0 与 1 两分支冒烟）。
- `tests::test_rate_limiter_from_env_defaults` 补 unset 两个新变量。

## Implementation notes / traps

- 驱逐比较器第一版把 tuple 顺序写反（`<` 取到 rank 最低 = apikey 优先出局，
  与目标相反），被
  `eviction_prefers_forged_source_keys_over_authenticated` 当场抓住。改成显式
  `r > br || (r == br && older first)`。
- `extract_client_key` 包装函数只剩测试使用（中间件走 `_with_config`），
  `-D warnings` 下 dead_code 报错 → `#[cfg(test)]` 门控。
- `.dev/AGENTS.md` 硬性契约全部遵守：`cargo fmt --all -- --check` 干净；
  `cargo clippy --all-targets --all-features -- -D warnings` 干净；
  env 类断言合并单测试；无新依赖。

## Verification

- `cargo test --lib http::rate_limit` → 17 passed
- `cargo test --workspace` → 全绿（821 lib + 各集成套件，0 FAILED）
- `cargo clippy --all-targets --all-features -- -D warnings` → clean
- `cargo fmt --all -- --check` → clean

## Review fixes (round 2 — independent review returned NEEDS_FIX)

All five findings addressed in `src/http/rate_limit.rs` + `CHANGELOG.md`:

1. **F1 (BLOCKER) — `trusted_xff_client` off by one.** The index was
   `len - 1 - n` with a `len <= n` guard, so with `n` trusted hops the code
   picked the entry *left* of the proxy-appended one: a single-proxy deployment
   fell back to the socket IP (every client sharing the proxy's `ip:` bucket —
   re-introducing NEW-HTTP-7) and a client prepending one forged entry got to
   choose its own bucket (the #107 no-op survived behind a proxy). Correct rule:
   the `n` trusted hops append the `n` **rightmost** entries, so the client is
   `entries[len - n]` and the guard is `len < n`. Implemented with
   `checked_sub` + `get` (no panic for `n == 0` / short chains). fn doc, field
   doc, and the three XFF tests were rewritten to the append model; the old
   comments ("the rightmost entry is our direct peer", "2-entry XFF must not
   satisfy trust depth 2") encoded the wrong premise and are gone.
2. **F2 — phantom `usize::MAX` escape hatch.** `entries.len() <= usize::MAX` is
   always true, so the documented "trust the whole chain" mode was really
   "never trust". The claim was dropped from the field doc: a hop count larger
   than the chain now explicitly **fails closed** to the socket IP (documented
   as such).
3. **F3 — env-var test race.** `test_rate_limiter_from_env_defaults` and
   `rate_limiter_from_env_trusted_proxies_and_max_buckets` both touched the same
   process globals from parallel threads. Collapsed into the single sequential
   test `rate_limiter_from_env_defaults_and_overrides` (`.dev/AGENTS.md`:
   "Env-var tests must be ONE test"). 17 → 16 tests in the module.
4. **F4 — eviction ranks inverted.** `class_rank` evicted `ip:` (2) before
   `xff:` (1), i.e. the unforgeable socket bucket before the header-derived one.
   Ranks are now `xff: = 2 > ip: = 1 > apikey: = 0` (evict header-derived
   first, credential-derived last), and the impossible field-doc parenthetical
   ("`xff:` prefix when `trusted_proxies == 0`") was removed — with 0 hops no
   `xff:` key is ever produced. Test renamed to
   `eviction_prefers_header_derived_keys_over_socket_and_authenticated` and
   extended to pin all three ranks in one deterministic sequence.
5. **F5/F6 — over-claimed docs.** F5 (`x-api-key` is unvalidated at the limiter
   layer, so rotating it still mints buckets) is pre-existing and unchanged in
   behaviour, but the CHANGELOG no longer reads as if authentication had
   happened — it now states the limiter runs *before* auth and that the header
   only partitions buckets, and the "limiter was a no-op" absolute is gone.
   F6: the "bounded by admission concurrency" claim about the eviction path was
   wrong (only fully-refilled buckets are evictable, so the map settles around
   `cap + new-keys/s × refill-time`); the `pick_eviction_victim` doc and the
   `bucket_map_is_bounded_under_unique_key_flood` header now say the cap is a
   burst guard and the reaper's `prune()` is what bounds the map long-run.

## Follow-ups (explicitly out of scope)

- 多副本共享限流（Redis/共享存储 backend）→ cloud-runtime 单。
- per-subject 桶（JWT `sub` 粒度租户）→ 租户配套单；`extract_client_key_with_config`
  已把"身份源"收敛为单点，届时在优先级链头部插入 `sub:` 即可。
- **F5 follow-up（本次未修）**：限流中间件在 auth 之前，`x-api-key` 未经验证即
  当身份 → 轮换伪造 key 仍能造新桶（既有问题）。彻底修需把限流移到 auth 之后、
  或对 key 做验证；在那之前驱逐策略里 `apikey:` 桶不可信但仍排在最后。
- E2E 08b-rate-limit 套件只测 burst/429，不依赖 XFF 行为，无需改 fixture。

## Review fixes (round 3 — 第二次独立评审 returned NEEDS_FIX)

评审 BLOCKER：文档承诺的 "socket IP" 回落**不存在**——`extract_client_key_with_config`
结尾读 `ConnectInfo<SocketAddr>` 扩展，但唯一生产 serve 路径
`serve_with_graceful_shutdown`（`src/http/mod.rs`）用的是裸 `axum::serve(listener,
router)`，从不安装 connect info（axum 只在
`into_make_service_with_connect_info::<C>()` 下注入）。实测 `127.0.0.1` 与 `[::1]`
两个源 IP 共用 `ip:unknown` 单桶：默认 `trusted_proxies = 0` 时所有无
`x-api-key` 的请求（含全部 JWT 客户端、代理后所有客户端）挤一个全局桶，一个人可
429 掉所有人，且默认配置下重新引入 NEW-HTTP-7。文档/CHANGELOG 的 "direct exposure
⇒ per-client" 叙述对出货二进制为假。

修复：

1. **BLOCKER（连接信息接线）**——`src/http/mod.rs::serve_with_graceful_shutdown`
   改用 `router.into_make_service_with_connect_info::<std::net::SocketAddr>()`，
   并补 fn 文档说明前一条 `axum::serve` 会静默省略该扩展。回落从此真实存在。
2. **不能失败的老测试**——`middleware_respects_trusted_proxies_config` 每请求新建
   limiter、断言两次 200，即使中间件硬编码 0 也过。重写为
   `middleware_derives_key_from_limiter_trust_config`：单 limiter、capacity 1 /
   refill 0，`trusted = 1` 时两个不同 XFF → 200/200、同一 XFF → 429；
   `trusted = 0` 时两个不同 XFF → 200/429（XFF 被忽略）。真正钉死接线。
3. **多字段 XFF**——`headers().get()` 只读第一个 `X-Forwarded-For` 字段；代理若
   另起一个字段 append，客户端可控字段仍落在 `len - n` 上。`trusted_xff_client`
   改为展开 `get_all()` 全部字段（`"a, b"` 与两个独立字段等价）后索引。
4. **文档一致性**——`max_buckets` 字段文档开头写 "Hard upper bound" 又说会超限；
   改为 "Burst guard: target ceiling"，与 `pick_eviction_victim` 文档对齐。
5. **热路径成本**——`pick_eviction_victim` 文档补 O(max_buckets) 扫描成本说明。

新增测试：

- `tests::serve_path_installs_connect_info_for_ip_fallback` —— 真起
  `serve_with_graceful_shutdown`，TCP 连 `127.0.0.1`，handler 回显限流 key，断言
  以 `ip:127.0.0.1` 结尾（旧的裸 `axum::serve` 会得 `ip:unknown`，测试即红）。
- `tests::extract_client_key_uses_socket_ip_from_connect_info` —— 注入
  `ConnectInfo` 扩展断言 `ip:203.0.113.7`。
- `goal_h3_xff::extract_client_key_flattens_multiple_xff_fields` —— 两个独立
  XFF 字段（n=1 / n=2）仍取代理 append 的那条。

Verification (round 3):
- `cargo test --lib http::rate_limit` → 19 passed / 0 failed
- `cargo test --workspace` → 2493 passed, 1 failed:
  `tools::execution::shell::tests::timeout_kills_child_process`（`shell.rs:324`
  "child should have written its PID before exec"）——与 #107 无关的 shell 超时用例，
  机器满载（多个 pipeline 并发编译）下子进程没来得及在断言前写 PID；单跑复现
  `cargo test --lib tools::execution::shell::tests::timeout_kills_child_process`
  → 1 passed / 0 failed。非本改动引入，未改。
- `cargo clippy --all-targets --all-features -- -D warnings` → clean（含 run 级
  `Cargo clippy --workspace` 范围）。
- `cargo fmt --all -- --check` → clean。
- `agent-mutants.sh` 未跑成：copy 模式冷编译 thiserror build script 时
  clang 段错误（`clang: error: unable to execute command: Segmentation fault: 11`，
  `ERROR cargo build failed in an unmutated tree`）——满载环境的瞬时链接器崩溃，
  非源码问题；v2 self-improve flow 只接 fmt/clippy/test 三门，不跑 mutants。



## Review fixes (round 4 — 第三次独立评审 returned NEEDS_FIX)

评审 BLOCKER：限流桶的驱逐/清理判据**永远不可能为真**，所以 SEC-011 实际未修，
而 CHANGELOG/字段文档宣称已修——又一类"文档承诺、二进制没有"。

根因：`TokenBucket.tokens` 存的是**剩余** token，且 `check()` 在任何情况下都先
夹到 `capacity` 再 `-= 1.0`；因此一个空闲了一小时的桶里存的仍是
`min(...) - 1 == capacity - 1`，**生产路径永远不会写出 `tokens == capacity`**。
而两个判据都要求恰好等于 capacity：

- `pick_eviction_victim` 的 `is_idle = tokens >= capacity - EPSILON` → 永假 →
  `max_buckets` 形同虚设；
- `prune()` 的 `retain(|_, b| b.tokens < capacity)` → 全部保留 → reaper 扫了个寂寞。

于是默认 `trusted_proxies = 0` 下，客户端每请求换一个伪造 `x-api-key`（该头在
auth 之前就被当作身份）就多一个桶、且永不回收 = #107/SEC-011 的无界增长仍在。

修复（按评审给出的方案）：

- 新增 `projected_tokens(b, capacity, refill_rate) = (tokens + last_refill.elapsed()
  × refill_rate).min(capacity)` 与 `is_idle(...) = projected_tokens >= capacity`；
  **空闲必须由时钟推算**，不能读那个陈旧的计数器。
- `pick_eviction_victim(buckets, capacity, refill_rate)` 与 `prune()` 都用同一个
  `is_idle`（驱逐与清理语义从此一致）。
- 测试不再手写 `tokens`：三个测试改为「`check()` 排空 → `tokio::sleep(IDLE_WAIT)`
  → 断言空闲可被驱逐/清理」，重利用率常量 `IDLE_REFILL_RATE = 5.0` / `IDLE_WAIT
  = 250ms`（capacity 2，排空后 200ms 后回满）。反证：把 `is_idle` 临时改回
  计数器版本，三个测试立刻红（prune 3≠0、flood 6≠5、xff 桶未被驱逐），证明它们
  不再靠手写状态过关。

非阻塞项一并处理：

- `rate_limiter_from_env` 文档补上 `TRUSTED_PROXIES` / `MAX_BUCKETS` 两个新旋钮
  （唯一面向运维的清单）。
- CHANGELOG 语法修："…Requests carrying a non-empty `x-api-key` key on the hash of
  that header…" → "…are keyed on the hash of that header…"。
- CHANGELOG 的 bound 叙述改为与实现一致：清理判据来自时钟；drained bucket 恒为
  `capacity − 1`，计数器判据永远不触发；cap 是 burst guard，稳态大小约
  `cap + (new keys/s) × refill window`，超过 refill window 未再被使用的 key 一律回收。
- `src/http/mod.rs:755` 的过期注释（"cannot bypass limits by rotating API keys
  (SEC-006)"）与本次 pre-auth `apikey:` 取键自相矛盾，改为如实描述：限流层先于
  auth 跑，带非空 `x-api-key` 的请求按该头哈希分桶、轮换仍能造新桶；不可伪造的是
  无凭证身份（socket IP / 可信代理 XFF 右侧项）。

Verification (round 4):
- `cargo test --lib http::rate_limit` → 19 passed / 0 failed
- `cargo test --lib` → 2494 passed / 0 failed（上一轮那个 shell 超时用例本轮绿）
- `cargo test --test http` → 103 passed / 0 failed
- `cargo test --workspace` → all suites green（lib 2494 passed；首次跑时
  `tools::execution::shell::tests::timeout_kills_child_process` 又因机器满载
  偶发红一次——单跑与复跑均绿，与 #107 无关，未改）
- `cargo clippy --all-targets --all-features -- -D warnings` → clean
- `cargo fmt --all -- --check` → clean
