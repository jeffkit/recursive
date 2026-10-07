# Issue #101 — Independent append-only audit stream (actor + hash chain + retrieval/export)

## Date
2026-10-07

## Goal
安全 gap P1（okguitar 提报，baseline `9165b9a0`）：工具级 `AuditMeta`
（`src/tools/policy_domain/audit.rs`）只记 step/时间/args_hash/副作用/退出状态，
随 transcript（整文件覆写、非 append-only、无签名链）存储——无 actor、无认证/审批
事件、可被任意写者改写、不可按人/时间检索。按单据建议实现独立 append-only 审计流。

## 设计要点
- **新模块 `src/audit_log.rs`**：`AuditRecord`（seq/ts/actor/session/action/prev_hash/hash）
  + `AuditAction`（tagged：`tool_call` / `approval_decision` / `admin_action` /
  `auth_failure`）。BLAKE3 哈希链：`hash = blake3(canonical(seq,ts,actor,session,action,prev_hash))`，
  首条链接 `GENESIS_HASH`（64 个 0）。
- **append-only + 0600**：`OpenOptions::append(true).create(true)`（Unix `.mode(0o600)`），
  永不 truncate/rewrite；`AuditLog::open` 扫尾恢复 `next_seq`/`last_hash`，append 是 O(1)。
- **可验证**：`verify_chain()` 重算整条链——内容被改（HashMismatch）、记录被删/重排
  （SeqMismatch / PrevHashMismatch）都会报错。`/audit?verify=true` 暴露该结论。
- **可检索**：`AuditQuery`（from/to epoch-millis、actor、tenant、session、limit）。
- **进程上下文**（对齐 `notify.rs` 的 `FILE_ROOT` 模式，避免 24 处 `AppState { .. }`
  字面量的改动）：`set_file_context(workspace)` 绑定
  `<user_workspace_dir>/audit/audit.jsonl`；`emit()` best-effort（未绑定即丢弃、
  写失败只 warn，绝不反噬被审计操作）。
- **actor 归因**：HTTP 审计中间件（`src/http/audit.rs`）位于 auth 层**内层**，故能读到
  已解析的 `AuthIdentity`；对 `/sessions/{id}*` 请求在 `next.run` **之前**
  把 `session → actor` 登记进进程表，使运行中（run_core 内）发出的工具事件也能归因。

## Files touched
- `src/audit_log.rs`（新）— 流类型、哈希链、append/read/verify、查询、进程上下文、
  session→actor 表 + 单元测试。
- `src/lib.rs` — 导出 `pub mod audit_log`。
- `src/http/audit.rs`（新）— `GET /audit`（过滤 + `verify`）、`GET /audit/export`
  （NDJSON）、`audit_middleware`（审批/管理动作分类 + session actor 登记）。
- `src/http/mod.rs` — 挂 `mod audit`、路由 `/audit`、`/audit/export`、把审计层放在
  `auth_middleware` 之内（`rate_limit → auth → audit → route`），OpenAPI 增补两条路径。
- `src/http/auth.rs` — 401（凭据无效）与 503（未配置认证）各发一条 `auth_failure`。
- `src/kernel.rs` / `src/run_core.rs` / `src/runtime.rs` / `src/multi.rs` /
  `src/kernel/tests.rs` / `tests/agent_team_integration.rs` —
  `TurnContext` / `RunCore` 增加 `audit_session`（会话 id 由 runtime 传入），
  `RunCore::process_tool_results` 把每个工具调用镜像进审计流（tool + 副作用 + ok）。
- `src/tools/policy_domain/audit.rs` — `ToolSideEffect::as_str()`（审计流用稳定标签）
  + 与 serde 标签一致性测试。
- `crates/recursive-cli/src/main.rs` — `recursive http` 启动时
  `audit_log::set_file_context(&config.workspace)`。

## Tests added
- `src/audit_log.rs`：链 seq/prev 链接、reopen 恢复链头、verify 通过、内容篡改检测、
  删记录检测、空链、查询（actor/tenant/session/时间/limit）、serde 往返、
  全局 no-op / 绑定后可检索可验证、session→actor 表往返。
- `src/http/audit.rs`：`classify`（审批 confirm/reject、DELETE purge、PATCH、POST
  /sessions，读请求/无关路由不分类）、`session_id_from_path`、query→`AuditQuery`、
  `/audit` 按 session 过滤 + `chain.ok`、`/audit/export` NDJSON + content-type。
- `src/tools/policy_domain/audit.rs`：`ToolSideEffect::as_str` 与 serde 标签一致。

## Notes
- 审计流**与 transcript 解耦**：不再依赖整文件覆写的会话文件；文件系统写者改写历史
  会被哈希链验证抓到。
- 工具事件在 `process_tool_results` 逐调用发出（覆盖 sentinel 与普通路径）。
- 未接线（后续单）：配置变更审计事件（本构建无 config-change 端点）、凭据轮换事件。
- 无新依赖（blake3/serde_json/tempfile 均已存在）。

## 复审修复（独立复审 NEEDS_FIX → 已改）

**Blocker 1 — `/audit`、`/audit/export` 无授权（跨租户读全量流）**
- `src/http/audit.rs`：两个 handler 现在都取 `Extension<AuthIdentity>`，结果先过
  `visible_to()`：admin 看全量，非 admin 只看 `actor.subject == identity.subject &&
  actor.tenant == identity.tenant` 的记录（与 #85 `AuthIdentity::may_access_session`
  同一条隔离规则）。NDJSON 导出同样过滤。过滤发生在 `limit` 之前，`limit` 只截断
  调用者可见的记录。
- 新增测试：`visible_to_hands_a_non_admin_only_its_own_records`、
  `list_audit_filters_and_scopes_to_the_caller`（同 session 的他人记录对非 admin 不可见、
  对 admin 可见）、`export_audit_returns_ndjson_scoped_to_the_caller`。
- OpenAPI 描述补上"非 admin 只看自己的记录"。

**Blocker 2 — session→actor 登记早于handler、且无视结果**
- 只对**非读**请求登记（`GET`/`HEAD` 不登记）：读请求不会以调用者身份派发工具，
  因此一个 `GET /sessions/{id}` 不应能"认领"一个它可能无权查看的会话。
- 登记会覆盖旧值，因此中间件在 `next.run` 之后、响应**非 2xx** 时回滚：有旧值时
  恢复旧 actor，没有则 `forget_session_actor`。这样 403/404 的请求（无权的调用者）
  无法改写他人在审计流中的归因。
- `SessionActors` 变为 `{actors: HashMap, order: VecDeque}` 并加 `MAX_TRACKED_SESSIONS
  = 1024` 的 FIFO 上限：任意凭据持有者无法用任意 session id 无限撑大进程表；
  `forget` 同时清理 order，避免死 id 泄漏。
- 新增测试：`middleware_attributes_a_successful_mutation_to_the_caller`、
  `middleware_never_attributes_a_read`、
  `middleware_drops_the_attribution_when_the_handler_refuses`、
  `middleware_restores_the_displaced_attribution_on_refusal`、
  `middleware_records_a_session_deletion_and_forgets_it`、
  `session_registry_evicts_the_oldest_once_capped`、
  `session_registry_forget_releases_the_slot`。

**Minor**
- `AuditChainError::Parse` 不再不可达：`verify_chain` 自己逐行解析（物理行号随
  `Parse { line, .. }` 上报），不再经由把解析失败折成 `io::Error` 的 `read_records`。
  `/audit?verify=true` 遇到无法解析的行返回 `chain.ok=false`（而非 500）——既然调用者
  问的就是"链是否完好"，读不出来本身就是答案。测试：
  `verify_reports_unparsable_line_as_chain_failure`。
- `/audit`、`/audit/export` 默认 `limit = 1000`（`DEFAULT_AUDIT_LIMIT`），防止全文件
  返回；模块文档说明"无 rotation"为已知限制（运维归档），检索默认取最近 N 条。
- `AuditAction::ToolCall` 文档修正：只覆盖**到达 dispatch** 的调用（含 registry 拒绝，
  `ok=false`）；plan mode / permission hook / hook skip-error 这些 dispatch 前拒绝没有
  `AuditMeta`，因此不进流——不再宣称"or was denied before dispatch"。
- `set_file_context` 打不开日志（如尾部被截断）时由 `warn` 升为 `error`，明确写出
  "auditing is DISABLED for this process"——静默失去审计正是该功能要防的失败模式。
- 子代理归因仍未接线（worker runtime 无 session id）：在 `src/audit_log.rs` 模块文档
  的 Known limits 中写明工具事件会记为 `local`，作为后续单处理（需要把父会话 id 接进
  worker runtime）。
