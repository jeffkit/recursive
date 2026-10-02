# Goal #77: HttpSkillSource —— 远程拉取 + https-only allowlist（#74 拆单 2/3）

Date: 2026-10-02
Goal: #74 拆单 2/3，depends-on #76（SkillSource trait + StaticSkillSource）。
续接 `v2-pipeline-77-1002155041-cont` worktree 上的 WIP（上轮 pipeline
两次 engine_error，代码已大体成形但测试有死路）。

## Files touched
- `src/skills.rs`
  - `pub struct HttpSkillSource`（`Clone` + 手写 `Debug`）——构造时
    https-only / allowlist / no-userinfo 校验，构造器不失败不做 I/O，
    配置错误存 `config_error`，`load_skills()` 首先回报（log-and-degrade
    友好，对齐 endpoints registry 的做法）。
  - `pub enum HttpSkillSourceError { Disallowed / Request / Parse }`。
  - `load_skills()`：`block_in_place` + `Handle::current().block_on`
    同步一次拉取；结果驻内存，`SkillSource for HttpSkillSource::skills()`
    失败降级为空目录（warn 日志），不 panic。
  - `fetch_remote_index()`：no-redirect client（3xx 报错不跟随——allowlist
    校验过的 origin 不允许中途漂移）、Content-Length 与实读 body 双重
    8 MiB 上限、错误文本 `without_url()` 剥离 URL。
  - `skills_from_remote_entries()`：条目 → content-backed `Skill`
    （Goal 64：`body: Some`、`/virtual/skills/<name>` 合成路径）；空名跳过、
    重名（case-insensitive）保首现；无 frontmatter 且带 `description` 时
    注入为 frontmatter。
  - `normalize_allowlist_host()`：lowercase + 去尾点 + 默认端口 443 归一。
  - 测试 +11：http:// 拒绝、其它 scheme / 不可解析 URL、allowlist 未列
    host、精确匹配无后缀（双向）、case/尾点/443 归一（走 `validate` 纯
    函数 seam，不碰 runtime）、userinfo 拒绝、loopback mock 全链路
    fetch+parse、404 → Request 错误、超大 Content-Length 拒绝、死端点
    降级为空目录（走真 `skills()` impl）、object-safe + Debug。
- `src/lib.rs` — re-export `HttpSkillSource` / `HttpSkillSourceError`。

## 安全姿态
- **https only**：skill 是 agent 执行的指令，明文传输即注入通道；
  比 `web_fetch`/`url_guard`（仍许 http）刻意更严。
- **allowlist 精确匹配**：无后缀/通配——被攻陷的子域不得供给 skill。
- **no redirects**：每一跳都可能漂出 allowlist 校验过的 origin。
- **8 MiB body 上限**：防恶意端点无限流。

## 上轮 WIP 测试修复（本次主要增量）
1. `http_skill_source_fetches_and_parses_the_index` 原 WIP 起 TcpListener
   mock 后从不对它发请求，注释自认"test double"，然后 `handle.join()`
   前只 sleep——真实死锁点：mock 写完 body 但 reqwest 从未连接，线程
   永不返回（join 挂死 → pipeline 超时 engine_error 的直接嫌疑）。改为
   经共享 `fetch_remote_index()` 对 mock 发真请求，全链路断言；edge
   cases（重名保首/空名跳过）单独走 mapping helper。
2. `http_skill_source_allowlist_is_case_and_trailing_dot_insensitive`
   原依赖"匹配对会走到网络路径"——但 `load_skills()` 需要 Tokio runtime，
   纯 `#[test]` 里 panic "there is no reactor running"。改走
   `HttpSkillSource::validate()` 纯函数 seam，语义等价且无 I/O。
3. `http_skill_source_skills_degrades_to_empty_on_request_failure` 原只是
   重演 match 形状（同义反复）。改为构造真 `HttpSkillSource`（死端点
   https URL），驱动完整 fetch → fail → 降级路径。
4. clippy `manual_ok_err`：`new()` 里 match 改 `.err()`。

## Tests
- `cargo test --workspace`：3832 passed / 0 failed（lib 2463）。
- `cargo clippy --all-targets --all-features -- -D warnings`：clean。
- `cargo fmt --all`：applied，`--check` clean。

## NEEDS_FIX resolution：merge main，消除 phantom deletions
独立 review 打回（VERDICT:NEEDS_FIX）：分支自 `fafac9eb` 分叉后 main 前进到
`cd9b5755`（#56 AG-UI 分层 + session-id 同秒碰撞修复 + test_util pin），
`git diff main` 携带 −2732 行本分支从未触碰的 phantom deletions——即
AGENTS.md 已知失败模式 #2。

修复：`git merge main`（对齐 pipeline-56 在 `b921b56d` 的做法）。两支改动
文件集完全不相交（本支仅 `src/skills.rs` / `src/lib.rs` / 本 journal），
'ort' 策略零冲突合并。合并后核验：

- `git diff main` 收缩为 3 文件：`src/skills.rs` +627、`src/lib.rs` +5、
  本 journal +59，无任何删除。
- `src/http/handlers.rs` 与 main byte-identical（分层版 3256 行）；
  `src/http/agui.rs` / `writer.rs` 碰撞回归测试 / `test_util.rs` pin 全部在位。

合并后重跑（旧 Tests 数字是 stale-base 树上跑的，作废）：
- `cargo test --workspace`：全部 ok，0 failed（lib 2475，含
  `http_skill_source_*` 11 项与 `create_in_same_second_gets_distinct_dirs`）。
- `cargo clippy --all-targets --all-features -- -D warnings`：clean。
- `cargo fmt --all -- --check`：clean。
