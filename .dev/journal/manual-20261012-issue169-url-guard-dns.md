# manual-20261012-issue169-url-guard-dns

- **Date**: 2026-10-12
- **Goal**: #169 — `url_guard` 只查字面主机：解析到内网的域名绕过 SSRF 防护
- **Baseline HEAD**: 6c724056（issue 正文的 `path:line` 证据写于 14bac15a，行号已漂）

## 根因

两层都缺：

1. **连接期不校验**。`validate_url`（`src/tools/policy_domain/url_guard.rs`）只有字面层：
   `Host::Domain` 仅拦 `localhost` / `*.localhost` / `metadata.google.internal`
   与「写得像 IP」的串；全仓无自定义 DNS resolver，reqwest 拿到 `getaddrinfo`
   的结果直接连。于是 `http://127.0.0.1.nip.io:6379/`、
   `http://169.254.169.254.nip.io/latest/meta-data/` 这类**域名**通过校验后落到
   环回 / 云元数据地址；DNS rebinding 同理（第二次解析无人管）。
2. **保留段不全**。`is_private_ip` 只看 127/8、RFC1918、169.254/16、
   255.255.255.255、0.0.0.0；漏 CGNAT `100.64.0.0/10`、`0.0.0.0/8` 的其余地址、
   IPv4-compatible `::a.b.c.d`（只用了 `to_ipv4_mapped`）、NAT64。

**红线证据（未修复的 6c724056）**：把 issue 的复现写成测试，`WebFetch::new()` +
`execute("http://127.0.0.1.nip.io:<port>/latest/meta-data/")` 的请求真的打到了本机
环回 mock 服务并读回 body：

```
SSRF bypass: http://127.0.0.1.nip.io:64480/latest/meta-data/ resolved to loopback
and was fetched; body=Ok("IMDS-SECRET-LOOPBACK"), loopback_hits=1
```

同一次运行里保留段测试也红：

```
assertion failed: is_private_ip("100.64.0.1".parse().unwrap())
expected http://100.64.0.1/ to be rejected
assertion failed: is_private_ip("::127.0.0.1".parse().unwrap())
```

## 改动

- **`src/tools/policy_domain/url_guard.rs`**
  - `GuardedResolver`（`reqwest::dns::Resolve`）：先经 `AddrSource` 取地址
    （生产 `SystemResolver` = `tokio::net::lookup_host`；测试可换桩），再过
    `guard_addrs`。**整份答卷任一地址命中即整份拒绝**——过滤掉私网地址会让连接回退到
    下一个地址，攻击者仍能把连接钉在私网。
  - `guarded_resolver()` 生产装配入口；`ALLOW_PRIVATE_DNS_ENV`
    （`RECURSIVE_SSRF_ALLOW_PRIVATE_DNS`，默认关）给「企业代理 / 内网服务名解析到私网」
    的部署留豁免。
  - 拒答时 `tracing::warn!`：reqwest 只渲染最外层 `error sending request`，工具层
    `Error::Tool` 又把 cause 拍平成字符串，日志是唯一能看到「哪个域名 / 哪个地址被拒」
    的地方。
  - `is_private_ip` 补齐：`0.0.0.0/8`（RFC 1122 §3.2.1.3）、CGNAT `100.64.0.0/10`
    （RFC 6598）、`to_ipv4` 覆盖 mapped + compatible、NAT64。
  - `nat64_rejects`：`64:ff9b:1::/48`（RFC 8215 local-use，专用于非全局 v4）整段拒绝；
    `64:ff9b::/96`（RFC 6052 WKP，DNS64 用它合成**全局**目标）只按内嵌 v4 判定 ——
    `64:ff9b::7f00:1`（127.0.0.1）与 `64:ff9b::a00:1`（10.0.0.1）拒，
    `64:ff9b::808:808`（8.8.8.8）放行。
- **`src/tools/web_fetch.rs`**：拆出 `client_builder()` / `from_builder()`，`new()`
  装上 `guarded_resolver()`（测试用同一 builder 注入桩 resolver）。
- **`src/tools/a2a.rs`**：`build_client()` 装 `guarded_resolver()`，
  a2a_call / a2a_card / a2a_task_check 共用。
- **`src/tools/policy_domain/url_guard.rs`**（review 返工 2 补）：接线钉的环境门
  `assert_loopback_bypasses_proxy`（`#[cfg(test)]`）——`NO_PROXY`/`no_proxy` 未豁免
  `localhost` + `127.0.0.1` 时直接 `panic!`，不让 pin 在代理环境里假绿。
- **`docs/architecture/tools/web-tools.md`** / **`docs/architecture/tools/a2a-tools.md`**
  （review 返工补）：用户可见文档写明两层校验、豁免开关
  `RECURSIVE_SSRF_ALLOW_PRIVATE_DNS`（默认关，设置后连接期放行私网地址），以及代理路径
  的已知限制（"Known limitation (proxy)"）。

未新增依赖，未动 `Cargo.toml` / `Cargo.lock`：`tokio::net` 由 reqwest 的
`tokio = { features = ["net", "time"] }` 依赖经 feature 统一提供。

## 验证

```
cargo test -p recursive-agent --lib url_guard        → 27 passed; 0 failed
cargo test -p recursive-agent --lib web_fetch        → 23 passed; 0 failed
cargo test -p recursive-agent --lib                  → 3312 passed; 0 failed
cargo test --test invariants                         → 55 passed; 0 failed
cargo clippy -p recursive-agent --lib -- -D warnings → Finished（0 warning）
cargo fmt --all -- --check                           → 通过
```

**反向对照（negative control）**：把 `is_private_ip` 还原成 6c724056 的函数体、并把
`guard_addrs` 改成 `Ok(addrs)` 直通（**测试与装配全部保留**），同一组测试：

```
url_guard: FAILED. 20 passed; 7 failed
  - 169.254.169.254 must be refused, got [169.254.169.254:0]
  - assertion failed: validate_url("http://[::127.0.0.1]/").is_err()
  - expected http://100.64.0.1/ to be rejected
  - a private DNS answer must be refused
web_fetch: FAILED. 22 passed; 1 failed
  - the guard must refuse a loopback DNS answer: "LOOPBACK-SECRET"
```

随后按 sha256 还原（`url_guard.rs` 50b6ea4b…、`web_fetch.rs` bbb75c4f…、
`a2a.rs` 5254c0ad…），scoped 测试重新全绿。

**真实 DNS 端到端**：另跑一次性（已删除、未入库）测试，`WebFetch::new()` +
`http://127.0.0.1.nip.io:<port>/latest/meta-data/`（真解析，非桩）修复后为 `Err` 且
环回服务 `hits=0`；同一条路径在修复前 hits=1 并读回 body。

### review 返工（第二个 commit）

验收者做了变异：删掉 `WebFetch::new()` 与 `a2a::build_client()` 里的
`.dns_resolver(guarded_resolver())` 后**已提交的测试没有任何一条转红**（守卫可被静默
删除而 CI 全绿）。补两条 hermetic「接线钉」：

```
cargo test -p recursive-agent --lib connection_time_guard
  → web_fetch_new_installs_connection_time_guard ... ok
  → a2a_build_client_installs_connection_time_guard ... ok
test result: ok. 2 passed; 0 failed
```

两条都用 `localhost`（必然解析到私有地址，且不依赖外网 DNS、不依赖真实域名）：

- 生产 client（`WebFetch::new()` / `A2aCallTool::build_client()`）访问
  `http://localhost:<port>/pinned` 必须 `Err`，且环回 listener 命中数为 **0**；
- 对照组（同一 builder 去掉 resolver / 裸 client）命中 **1** 并读回 body，证明断言
  非空转。

`web_fetch` 侧走私有 `fetch()` 而不是 `execute()`：`validate_url`（第一层）本来就会拦掉
字面 `localhost`，本测试钉的是 resolver 层。`.cargo/config.toml` 的
`NO_PROXY=127.0.0.1,localhost,::1` 保证 cargo 起的进程不会把 `localhost` 交给代理，
因此 resolver 一定被调用。

**接线负向对照**：删掉两处 `.dns_resolver(guarded_resolver())` 后同两条测试：

```
web_fetch: FAILED — WebFetch::new() no longer installs the DNS guard: "PINNED-SECRET"
a2a:       FAILED — A2aCallTool::build_client() no longer installs the DNS guard:
                    Response { url: "http://localhost:65443/pinned", status: 200, ... }
test result: FAILED. 0 passed; 2 failed
```

按 sha256 还原（`web_fetch.rs` 545a614e…、`a2a.rs` 91f89b37…），两条测试重新全绿。

返工后门禁：

```
cargo test -p recursive-agent --lib url_guard                       → 27 passed; 0 failed
cargo test -p recursive-agent --lib web_fetch                       → 24 passed; 0 failed
cargo test -p recursive-agent --lib                                 → 3314 passed; 0 failed
cargo test --test invariants                                        → 55 passed; 0 failed
cargo clippy -p recursive-agent --all-targets -- -D warnings        → Finished（0 warning）
cargo fmt --all -- --check                                          → 通过
```

### review 返工 2（第三个 commit）：pin 在被覆盖的 `NO_PROXY` 下不再假绿

验收者指出 pin 的**假绿**：`.cargo/config.toml` 的 `[env]` 不覆盖用户已导出的值，所以
`NO_PROXY=example.com` + 一个不可达 `HTTP_PROXY` 时，生产臂在"代理不可达"处就失败了，
`expect_err` + 命中 0 被**以错误的理由**满足 —— 即使删掉 `.dns_resolver(...)`，两条 pin
仍然绿（=守卫可被静默删除）。

修法：两条 pin 的第一句调用 `url_guard::assert_loopback_bypasses_proxy()`
（`#[cfg(test)]`）。reqwest 的取值顺序是 `NO_PROXY` → `no_proxy`；该值必须豁免
`localhost` 与 `127.0.0.1`（`,`/空白/`;` 分隔、大小写不敏感、容忍一个前导 `.` 与 `*`），
否则 `panic!` 并列出当前设置的代理变量（`HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` 大小写
两种）。**没有**改用自建 `.no_proxy()` builder —— 那就不再钉生产
`WebFetch::new()` / `build_client()` 的接线。

证据：

```
# 默认环境：两条 pin 有意义地通过（gate 由 .cargo/config.toml 提供的 NO_PROXY 满足）
cargo test -p recursive-agent --lib connection_time_guard
  → 2 passed; 0 failed

# 被覆盖环境 + 接线完好：gate 大声失败（该环境下这条用例不可信，拒跑）
NO_PROXY=example.com HTTP_PROXY=http://127.0.0.1:1 cargo test -p recursive-agent --lib connection_time_guard
  → FAILED. 0 passed; 2 failed
  panicked at src/tools/policy_domain/url_guard.rs:494:
  connection_time_guard would pass for the wrong reason: NO_PROXY="example.com" does not exempt
  ["localhost", "127.0.0.1"], so the request would be sent to the proxy and fail there instead of
  being refused by the connection-time SSRF guard (deleting the guard would stay green). Unset
  NO_PROXY or add the loopback names. Proxy env vars currently set: ["HTTP_PROXY"]

# 被覆盖环境 + 接线被人为删除：修复前这里是静默绿，现在同样大声失败（gate 先拦下）
NO_PROXY=example.com HTTP_PROXY=http://127.0.0.1:1 cargo test -p recursive-agent --lib connection_time_guard
  → FAILED. 0 passed; 2 failed（同样 panic 在 url_guard.rs:494）

# 同一变异 + 默认环境：pin 按设计报出接线缺失
cargo test -p recursive-agent --lib connection_time_guard
  → FAILED. 0 passed; 2 failed
  web_fetch.rs:929 WebFetch::new() no longer installs the DNS guard: "PINNED-SECRET"
  a2a.rs:1861       A2aCallTool::build_client() no longer installs the DNS guard: Response { ... }
```

按 sha256 还原（`web_fetch.rs` abcde778…、`a2a.rs` 839c4970…）后默认环境重新全绿。
同轮把代理限制写进用户文档（`web-tools.md` "Known limitation (proxy)"）。

返工 2 后门禁：

```
cargo test -p recursive-agent --lib connection_time_guard           → 2 passed; 0 failed
cargo test -p recursive-agent --lib url_guard                       → 29 passed; 0 failed
cargo test -p recursive-agent --lib web_fetch                       → 24 passed; 0 failed
cargo test -p recursive-agent --lib                                 → 3316 passed; 0 failed
cargo test --test invariants                                        → 55 passed; 0 failed
cargo clippy -p recursive-agent --all-targets -- -D warnings        → Finished（0 warning）
cargo fmt --all -- --check                                          → 通过
```

## 提交与 diff stat（真实数字，`git show --numstat`）

首版 commit（被验收的哈希，未 amend）：

| file | + | - |
|---|---|---|
| `.dev/journal/manual-20261012-issue169-url-guard-dns.md` | 123 | 0 |
| `src/tools/a2a.rs` | 5 | 0 |
| `src/tools/policy_domain/url_guard.rs` | 428 | 5 |
| `src/tools/web_fetch.rs` | 145 | 14 |

`git show --stat`：`4 files changed, 701 insertions(+), 19 deletions(-)`。
注意 `--stat` 的 per-file 列是**增删之和**（`url_guard.rs` 433、`web_fetch.rs` 159），
与该文件**总行数**（`url_guard.rs` 800 行）不是一回事。

review 返工 commit（不含本 journal 自身的改动）：

| file | + | - |
|---|---|---|
| `src/tools/a2a.rs` | 73 | 0 |
| `src/tools/web_fetch.rs` | 66 | 0 |
| `docs/architecture/tools/web-tools.md` | 27 | 0 |
| `docs/architecture/tools/a2a-tools.md` | 13 | 0 |

`git diff --stat -- src docs`：`4 files changed, 179 insertions(+), 0 deletions(-)`。

review 返工 2 commit（不含本 journal 自身的改动）：

| file | + | - |
|---|---|---|
| `docs/architecture/tools/web-tools.md` | 8 | 0 |
| `src/tools/a2a.rs` | 5 | 0 |
| `src/tools/policy_domain/url_guard.rs` | 115 | 0 |
| `src/tools/web_fetch.rs` | 5 | 0 |

`git diff --stat -- src docs`：`4 files changed, 133 insertions(+), 0 deletions(-)`。

## Notes

- 未装 guard 的 client：**`http_call`**。它的 base URL 由运营者在 endpoint registry 里
  配置，并显式支持 `allow_private: true`（内网业务网关是它的正常用法）；resolver 是
  client 级、无法按 endpoint 生效，接上去会把这个 opt-in 直接打死。issue 的「边界」也
  写明本单不含 `http_call` 的路径语义，故未动它 —— 需要的话应做成双 client
  （guarded / unguarded）另开一单。其余 client（`web_search`、`install_skill`、
  `providers_cache`、`skills`、`llm/*`）的目标 URL 来自运营者配置而非模型，同样未动。
- **代理路径 fail-open（已知限制，本单不修）**：进程设了 `HTTP_PROXY` /
  `HTTPS_PROXY` 时，reqwest 把请求交给代理、由**代理**解析目标名，`GuardedResolver`
  根本不被调用（只有直连才会走 resolver）。验收者在带代理的环境复现：
  `http://<内网主机>:port/` 返回 `Ok("PROXY-SECRET")`。这是修复前就存在的行为（此前
  也没有守卫），不属本单验收项，**本单不实现**。
  建议后续单（可直接用的措辞）：

  > security(net): #1xx proxy 路径上 SSRF 守卫 fail-open —— reqwest 在
  > `HTTP_PROXY`/`HTTPS_PROXY` 下让**代理**解析目标名，`GuardedResolver` 不参与，
  > 内网目标可经代理访问。建议二选一：(a) 默认拒绝非 loopback 代理（只有显式
  > `RECURSIVE_ALLOW_PROXY=1` 才启用系统代理）；(b) 代理模式下先对目标名做 DNS
  > 预解析并拒绝私网地址（代理侧可能再次解析，需注明残余风险）。两者都要保留
  > `RECURSIVE_SSRF_ALLOW_PRIVATE_DNS` 语义，并补一条「带代理时守卫仍生效」的测试。

  当前缓解：`GuardedResolver` 的上述语义 + `.cargo/config.toml` 里
  `NO_PROXY=127.0.0.1,localhost,::1`（仅 cargo 起的进程）。用户可见版本见
  `docs/architecture/tools/web-tools.md` 的 "Known limitation (proxy)"。
- **错误可见性**：guard 拒绝在用户可见错误里只表现为
  `request failed: error sending request for url (...)`（见上），只有 `tracing` 的 warn
  能看到原因。没有改 `web_fetch` / `a2a` 的错误渲染（不在本单范围）。
- **NAT64 精度**：只按内嵌 v4 判定 `64:ff9b::/96`，而不是整段拒绝 —— 整段拒绝会让
  IPv6-only + DNS64 的网络把所有公网站点都判成私网。运营者自定义的 DNS64 前缀（非 WKP）
  无法通用识别，属已知残余缺口。
- **仍未覆盖的保留段**（本单未改，避免过度拦截）：`192.0.0.0/24`、`198.18.0.0/15`、
  `240.0.0.0/4`、`2001:db8::/32`（后者被既有测试显式要求放行）。要收需另开单并同步调整
  既有断言。
- **spec drift**：
  - spec 写 `url_guard.rs:58-145`，main 已漂到 `Host::Domain` 分支 `:79-112`、
    `is_private_ip` `:126-145` —— 行为与 spec 描述一致，只是行号变了。
  - spec 设计稿里的 `reqwest::dns::GaiResolver` **在 0.12.28 不是公开 API**
    （registry `src/dns/mod.rs` 是 `pub(crate) mod gai`）。spec 自己允许「或直接包系统
    解析」，实现取了这条：`SystemResolver` + `tokio::net::lookup_host`。
  - spec 的 `fn guard_addrs(Vec<SocketAddr>)` 多了一个显式 `allow_private` 参数，以免测试
    去改进程环境（本仓惯例，测试不 `set_var`）。
