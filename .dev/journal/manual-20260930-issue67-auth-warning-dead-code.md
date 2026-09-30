# Issue #67 — CLI 认证告警死代码清理

- Date: 2026-09-30
- Goal: 按 issue #67 更正评论的建议，删除 `crates/recursive-cli/src/main.rs` 中恒不触发的
  HTTP 认证告警死路径（`http_auth_enabled` + `disabled_auth_warning` + 调用点 + 2 个单元测试）。
  运维侧信号不丢：`auth_config_from_env`（`src/http/auth.rs`）在 `build_router` 时对未配置认证
  已打 ERROR（变量名正确），且中间件 503 default-deny（Goal 277 / SEC-003）兜底。
- Files touched:
  - `crates/recursive-cli/src/main.rs`（删除调用点 / 两个 fn / 两个测试；原调用点留注释指向
    `auth_config_from_env`，防止未来再加重复告警）
- Tests added: 无（净删除；`http_auth_enabled_when_either_credential_is_set` 与
  `disabled_auth_warning_is_emitted_only_without_auth` 随死代码一并移除）。产品行为由既有
  auth 中间件测试与 e2e `39-auth` 套件覆盖，不受影响。
- Notes: 更正评论实测三用例：告警在 `RECURSIVE_API_KEY`（出站 key）必设的任何可用部署中恒不触发；
  原 `fix env 名`方案会与 `auth.rs` 的 ERROR 重复，故取删除方案。
- Gates: `cargo fmt --all` ✓；`cargo clippy --all-targets --all-features -- -D warnings` ✓；
  `cargo test --workspace` ✓（链路 exit 0）。
