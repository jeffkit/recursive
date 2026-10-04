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
