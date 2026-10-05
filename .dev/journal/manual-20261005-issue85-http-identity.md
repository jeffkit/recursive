# manual-20261005 — issue #85: HTTP 身份模型（会话归属）

- Date: 2026-10-05
- Goal: issue #85 `security(http): 无租户身份模型`（P0，基线 HEAD 9165b9a0）
- Upstream: `okguitar` 提报

## 问题

HTTP 面认证只回答「能不能进」，不回答「你是谁」：

- `JwtConfig::is_valid` 用 `decode::<serde_json::Value>(...).is_ok()` 验签后**丢弃全部 claims**（`sub` 没进请求上下文）。
- `RECURSIVE_HTTP_AUTH_KEYS` 是**可互换**的 key 列表，任何持 key 者等同。
- 会话没有归属维度：`GET /sessions` 返回全部会话；`GET/DELETE/PATCH /sessions/:id`、
  `messages` / `events` / `fork` / `goal` / `interrupt` / `plan/confirm|reject` 只按 id 操作；
  任一持凭据者可横向读取/删除/干扰他人会话、代批他人 plan。

## 改动

**身份（`src/http/auth.rs`）**

- 新增 `AuthIdentity { subject, tenant, admin }`：JWT `sub`（+ 可选 `tenant`）或 API key 的
  subject。`AuthConfig::identify` / `identify_bearer` 解析身份（保持原 `is_valid` 的
  常数时间扫描：命中也要扫完全部 key，不泄漏「命中哪一个」）。
- `JwtConfig::identity` 读 `sub` / `tenant`；**无 `sub` 的有效签名 token 不再算身份**
  （无法归属 → 401，而不是塌缩成一个共享匿名主体）。`is_valid` 保留为「验签」问句。
- 新 env：
  - `RECURSIVE_HTTP_AUTH_KEY_OWNERS` = `subject=key,...` 把 key 归属到调用者（首个 `=` 切分；
    未归属的 key 共用 `DEFAULT_KEY_SUBJECT` = `api-key`，即「单一服务主体」语义，与旧行为一致）。
  - `RECURSIVE_HTTP_AUTH_ADMINS` = subject 列表，拥有 `admin` 角色。**admin 只来自服务端配置**，
    不读 token 自报的 `role`/`roles`——否则 token 能自我提权（有单测钉住）。
- 中间件把身份放进 request extensions（`Extension<AuthIdentity>`）；auth 未配置的 debug
  逃生通道使用 `AuthIdentity::local()`（`local`/admin），保持单用户 dev 语义不变。

**归属（`SessionState.owner` / `.tenant`）**

- `SessionState` + `SessionMeta` 增加 `owner`/`tenant`；创建/fork 时写入，经 #98 元数据路径
  持久化，冷加载恢复（`SessionMeta` 老 blob 无该字段 → owner=None）。
- `handlers::ensure_access` 断言：owner（且 tenant 相同）或 admin，否则 **403**
  （id 是 UUIDv7，不可枚举，因此不用 404 掩盖）。未归属的旧会话 = 无主 → 仅 admin。
- 覆盖面：`GET /sessions` 过滤；`GET/DELETE/PATCH /sessions/:id`、`messages`、`events`、
  `fork`、`plan/confirm|reject`、`goal`（POST/DELETE）、`interrupt` 全部断言。
- `cold_load::get_or_load_session` 增加 identity 参数：**先判归属、再构建 runtime**
  （外来 id 不会把别人的会话 materialize 进内存表）。triggers 走 `AuthIdentity::local()`
  （服务端无请求方的 actor，同 admin）。
- **顺带补齐同一授权面**：`POST /triggers` 可带 `session_id`，触发时以服务端身份在**该会话**
  里跑 turn——所以注册时也要断言归属（`ensure_session_access_by_id`：内存表优先，否则读
  持久化元数据；完全未知的 id 放行，因为无会话可劫持），否则 `/sessions` 的隔离只是表面功夫。

**不改动的边界**：`/agui`（按 thread id 的独立存储）、`POST /run`（无会话）、数据面
（workspace/记忆跨用户互通）留给配套单。

## 有意为之的行为变更（breaking）

1. 无 `sub` 的 JWT → 401（此前通过）。
2. 身份模型之前写入的会话（元数据无 owner）→ 仅 admin 可访问（default-deny 无主数据）。
3. 两个不同 API key 若未用 `KEY_OWNERS` 归属，仍共用 `api-key` 主体（保持旧语义；
   要隔离就配置归属，或用 JWT）。

## Files touched

- `src/http/auth.rs`（身份、JWT claims、env、中间件、单测）
- `src/http/mod.rs`（`SessionState` 字段、re-export）
- `src/http/handlers.rs`（`ensure_access`/`ensure_owned`/`ensure_session_access_by_id` + 各路由）
- `src/http/cold_load.rs`（`SessionMeta` owner/tenant、加载前归属断言）
- `src/http/triggers.rs`（注册时归属断言、服务端身份）
- `tests/http.rs`（跨调用者隔离 / admin 豁免 / JWT sub 隔离 / 无 sub 拒绝 / 无主会话 / trigger 越权）
- `CHANGELOG.md`、`.env.example`、`website/{en,zh}/guide/config.md`

## Tests added

- `src/http/auth.rs`：`identify_*`（默认 subject / 未知名 / `with_key_subject` 覆盖与新增）、
  `admins_are_resolved_from_the_subject`、`with_admin_ignores_duplicates`、
  `may_access_session_*`（subject / tenant / 无主 / admin）、`AuthIdentity::local`、
  `identify_bearer_*`（sub+tenant / 无 sub / 坏签名 / 过期 / 无 verifier）、
  `jwt_admin_role_comes_from_config_not_from_the_token`、
  `auth_config_from_env_reads_keys_owners_and_admins`（单测合并为一个，避免 env 竞争）。
- `src/http/cold_load.rs`：`cold_load_refuses_a_session_owned_by_someone_else`
  （异主/异租户 403 且**未** materialize），元数据 round-trip / 改标题保留 owner。
- `tests/http.rs`：`sessions_are_scoped_to_their_owner`（11 条路由 403 + 失败请求无副作用）、
  `admin_identity_reaches_other_callers_sessions`、`jwt_sub_scopes_sessions`、
  `jwt_without_sub_is_rejected`、`unattributed_sessions_are_admin_only`、
  `trigger_registration_cannot_target_a_foreign_session`。
- 既有 user-/audience-/rate-limit 测试与 e2e（单 key 用例）语义不变。

## Notes / follow-ups

- 观察到但**未改**：`src/http/cold_load.rs` 顶部 `get_or_load_session` 的文档块被后面的
  `deleted_marker_key` 文档块「粘」在一起（历史遗留，rustdoc 上挂到了后一个 item）——
  与本次安全修复无关，保持最小改动。
- 单 key 部署（含全部 e2e HTTP 用例）行为不变：一个 key = 一个 `api-key` 主体，自己创建的
  会话自己可见。
- 后续（不在本单）：`/agui` thread 归属、审计事件带 actor（配套单）、数据面隔离（配套单）。

## Review fix（独立评审 NEEDS_FIX）

评审复现并确认两处授权面遗漏，均已修：

1. **fork 会话重启后无主**（阻塞项）。`fork_session` 只写内存 `owner`/`tenant`，不像
   `create_session` 那样调 `persist_session_meta`；而 `flush_all_sessions` 会落 fork 的
   transcript，于是重启后 `/sessions/:fork` 冷加载 `meta == None` → 创建者自己拿 403、
   从 `GET /sessions` 消失、DELETE 404。修：fork 后立即 `persist_session_meta`，写入
   `owner`/`tenant` + 继承的 `preset`（`system_prompt`/`permission_mode`/`title` 保持 None，
   因为 fork 就是用服务端默认 base prompt 和无权限覆盖组装的——写进去反而让它按从未运行过的
   配置重建）。回归测试 `fork_ownership_survives_a_restart` 先验证在无此改动时确实 403。
2. **trigger 表是 workspace 全局且无归属**（原 diff 只挡了注册路径）。任一凭据者可列出全部
   trigger（id/goal/session_id），并 `PATCH` 改 `goal` → 该 goal 以 admin 身份在**别人的
   会话**里跑，注册时的归属断言被 patch 路径旁路。修：`Trigger` 增加 `owner`/`tenant`
   （`serde(default)`，旧 blob = 无主 = 仅 admin），`create_trigger` 从身份写入；
   `GET /triggers` 过滤、`GET`/`PATCH`/`DELETE /triggers/:id` 与 `POST /webhooks/:id` 均
   `ensure_trigger_access` 断言（403）。webhook 触发两条凭据都要：`?key=` 秘密证明「请求属于
   该 trigger」，服务端凭据证明「调用者拥有该 trigger」——泄露的 id（+ 空 secret 的 local-only
   配置）不再足以驱动别人的定时任务。

评审列出的其余两点按现状处理（均已说明）：冷会话的 `DELETE`/`PATCH`/`events` 仍只查内存表
（404 而非 403）——冷加载的按需materialize 语义所致，无泄漏；`/agui` 与数据面跨用户互通
留给配套单。

### Review fix — Files touched

- `src/http/handlers.rs`（`fork_session` 持久化元数据）
- `src/http/triggers.rs`（`ensure_trigger_access`，各路由断言，注册写入归属）
- `src/triggers.rs`（`Trigger.owner`/`.tenant`）
- `tests/http.rs`（`fork_ownership_survives_a_restart`、`triggers_are_scoped_to_their_owner`）
- `CHANGELOG.md`

### Review fix — 验证

```
cargo test --features http --test http        # 122 passed
cargo test --workspace                        # 59 个 test target 全绿
cargo clippy --all-targets --all-features -- -D warnings   # clean
cargo fmt --all -- --check                    # clean
```

