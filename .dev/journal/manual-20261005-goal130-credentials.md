# #130 凭据引用间接层 + owner-only 校验 + 脱敏报错 + 授权流

- **Date**: 2026-10-05
- **Goal**: `#130 feat(security): 凭据引用间接层 + owner-only 校验 + 脱敏报错 + 授权流（借 DSH credentials）`
- **Status**: 完成（库内能力 + 供应商逐请求解析；CLI 装配未改，见 Notes）

## 新增

`src/credentials/`（新模块，4 个文件；不是 tool 也不是 provider，所以不落在
`src/tools/`、`src/llm/`，与 `permissions/`、`knowledge/` 同级）：

- `types.rs`
  - `CredentialRef`（env 名）与 `CredentialKey`（`<scope>/<id>`，scope = 注册方）
    **两个不相交键空间**——给前端的 key 无法用来读 `std::env`。
  - `CredentialSource`（inherited-env / credentials-file / cwd-env-file /
    home-env-file）+ `writable()`。
  - `CredentialInfo`：`{key, configured, source, writable}`，**结构上没有值槽**；
    测试断言其序列化字段集恰好是这 4 个（加字段即失败），且不含 `sk-secret`。
  - `CredentialErrorCode`：稳定错误码（`as_str()` 即 Display，直接进错误文本）。
- `store.rs` — `CredentialStore`
  - 四层信任排序：继承环境（只读、胜出）> `<user-data-dir>/.credentials.yaml`
    （唯一可写层）> `<cwd>/.env` > `<user-data-dir>/.env`；`discover()` 走
    `paths::user_data_dir()`（尊重 `RECURSIVE_HOME`）。
  - **逐操作解析、不跨操作缓存**（`resolve` 每次重新读层）；**空值即缺席**。
  - **owner-only 硬防护**：`ensure_owner_only` 在**读内容之前**用 `metadata`
    检查 `mode & 0o077`，拒绝时给出 `chmod 600 <file>` 修复指引；非 unix 平台 no-op。
  - **脱敏解析报错**：`parse_credentials_yaml` 只重建 `line/column`，绝不回显
    源行（serde 的 type error 会引号回显值，所以整条消息由我们重写）。
  - `set()`：以 `OpenOptions::mode(0o600)` 创建 + 显式 `set_permissions(0o600)`，
    是授权流"提交确实发生"的落地点；拒绝写入空值。
  - `ApiKeySource`（trait，定义在 `mod.rs`）+ `StoredCredential`（store+ref 实现）。
- `authorization.rs` — `AuthorizationRegistry`
  - 一 key 仅一个 flow（`register` 重复即 `DuplicateFlow`）；同时仅一次尝试
    （`InProgress`；锁**不跨** `attempt()` 持有，所以 flow 可以安全重入）；
  - 成功以"提交确实发生"验证：flow 说 `Committed` 只算声明，必须
    `verify_committed()` 观察到提交，否则 `Unverified`；
  - `AuthorizationDeclinedError` 等价物：decline 与 infra failure 落成不同 code；
  - 每次 settle 广播 `AuthorizationSettled` 且按 key 记住终态
    （`terminal_state`），第二界面晚接入也能知道结束；
  - `CallbackFlow`：闭包实现，前端注册一个 flow 不必新建 struct。

## 修改

- `src/error.rs`：新增 `Error::Credential { code, message }`（Display 里带稳定码）
  与 `Error::credential_code()`——这就是"拒绝与故障在错误码级可区分"的 API。
- `src/lib.rs`：注册 `pub mod credentials`。
- `src/llm/openai.rs` / `src/llm/anthropic.rs`：新增
  `api_key_source: Option<Arc<dyn ApiKeySource>>` + `with_api_key_source()`，
  两处发请求的 header 改走 `current_api_key()`（每次调用解析一次，**不是**每次重试）。
  默认 `None` → 行为与之前完全一致（构造期字符串）。
- `tests/invariants/test_coverage.rs`：把 4 个新文件加进 `MUST_HAVE_TESTS`
  （invariant #4 的显式清单）。

## 验收对照

1. **配置与日志不出现密钥值（含解析错误路径）** →
   `parse_errors_report_a_location_and_never_echo_the_source_line`（含 serde
   type error 这条最危险的路径）、`credential_info_has_no_value_slot`、
   `group_or_world_readable_credentials_file_is_refused_with_a_fix_hint`
   里对 message 不含值的三处断言。
2. **组/他人可读凭据文件被拒 + 修复指引** →
   `group_or_world_readable_credentials_file_is_refused_with_a_fix_hint`
   （0o644 与 0o640 都拒；断言含 `chmod 600`；改回 0600 后可读）。
3. **轮换后下一请求即生效** →
   存储层 `rotation_is_visible_on_the_next_resolve_without_restart`；
   请求层 `llm::openai::tests::api_key_source_is_resolved_per_request_so_a_rotation_lands_immediately`
   与 anthropic 同名测试：起 2 连接 mock server，断言两次请求发出的
   `authorization: Bearer` / `x-api-key` 分别是 `sk-old` / `sk-new`
   （中间只改凭据文件，不重建 provider、不重启进程）。
4. **拒绝与故障错误码可区分** →
   `a_decline_is_settled_and_distinguishable_from_a_failure`
   （`authorization_declined` vs `authorization_failed`）
   + `error_codes_are_stable_and_distinguish_decline_from_failure`。

## 测试

`cargo test --lib credentials::` → 37 passed / 0 failed；
`cargo test --lib api_key_source_is_resolved_per_request` → 2 passed / 0 failed；
`cargo test --workspace` → 全绿（lib 2883 passed / 0 failed，其它 target 全部 ok，
0 failed；中途一次全 lib 跑出 1 failed 是我自己新写的一个 Debug 断言写错了
字段名，已修，之后 37 + 2 + workspace 全绿）；
`cargo fmt --all -- --check` → 干净；
`cargo clippy --all-targets --all-features -- -D warnings` → exit 0；
`cargo check --workspace --all-features` → exit 0。

## Notes

- **无新依赖**（invariant #6）：YAML 解析复用已有的 `serde_yaml_ng`，
  错误/结果类型复用 `thiserror`，测试用已有的 `tempfile`。
- **CLI/装配层未改**（`crates/recursive-cli/src/{main.rs,cli/builder.rs}`、
  `src/llm/factory.rs`、`src/http/handlers.rs` 的 `build_llm_provider(... &str)`
  签名保持）。这样做的理由：`.dev/AGENTS.md` 已知失败模式 #8——碰 `main.rs` 会让
  `cli-mutants` 扇出到 ~200 mutant / 40–60 分钟。`ApiKeySource` +
  `with_api_key_source` 已经把 seam 留在库边界，CLI/HTTP 侧接线是独立一步
  （`Config` 里把 `provider.api_key` 解析成引用而不是字面值）。
- **`.env` 层不做 owner-only 检查**（只对 `.credentials.yaml` 做）：`.env` 常是
  项目共享文件，拒绝它会砸掉正常用法；密钥文件的硬防护按 DSH 规格只落在
  credentials 文件上。
- 凭据文件位置取 `<user-data-dir>/.credentials.yaml`（即
  `~/.recursive/.credentials.yaml`，尊重 `RECURSIVE_HOME`），与 `paths.rs`
  的 per-user 状态布局一致。
