# Goal 215 — 拆分 src/http.rs：按职责分离 Auth / RateLimit / Handlers

**Roadmap**: 代码健康 — 大文件专项整治（第四批）

**设计原则检查**:
- 纯代码组织重构，运行时行为不变
- 将 `src/http.rs` 改造为 `src/http/mod.rs` + 子模块
- 通过 `pub use` 保持所有公开 API 在 `crate::http::*` 路径不变
- ❌ 不改变任何 HTTP 路由逻辑或 handler 行为

## 背景

`src/http.rs` 当前 **2666 行**，混杂了认证配置、限流器、
request/response 数据类型、路由注册和所有 HTTP handler。
将 `src/http.rs` 改造为目录模块可以在不破坏任何调用方的前提下
大幅提高可读性。

## 目标

将 `src/http.rs` 改造为 `src/http/` 目录模块，拆分为 4 个子模块：

| 新文件 | 迁移内容 | 预估行数 |
|--------|---------|---------|
| `src/http/auth.rs` | `AuthConfig`, `JwtConfig`, `auth_config_from_env`, `auth_middleware` | ~270 |
| `src/http/rate_limit.rs` | `RateLimiter`, `rate_limiter_from_env`, `extract_client_key`, `rate_limit_middleware`, `metrics_middleware` | ~200 |
| `src/http/handlers.rs` | `run_agent`, `create_session`, `list_sessions`, `get_session`, `list_tools`, `health`, `openapi_spec`, `metrics_handler`, `generate_session_id`, `format_timestamp`, `days_to_ymd`, `is_leap` 等所有 handler 函数 | ~1100 |
| `src/http/mod.rs` | `AppState`, request/response 数据类型（`SessionState`, `RunRequest`, `RunResponse`, `SseEvent`, `SseContentBlock` 等）, `build_router*`, `build_openapi_spec`, `Metrics` | ~600 |

拆分后最大文件 `handlers.rs` 约 1100 行，`mod.rs` 约 600 行。

## 实施细节

### 1. 文件结构变更

从：
```
src/http.rs            # 2666 行
```

改为：
```
src/http/
  mod.rs               # ~600 行（原 http.rs 骨架）
  auth.rs              # ~270 行
  rate_limit.rs        # ~200 行
  handlers.rs          # ~1100 行
```

**重要**：Rust 对 `src/http.rs` 和 `src/http/mod.rs` 视为等价，
所以这次重构需要：
1. 删除 `src/http.rs`
2. 创建 `src/http/mod.rs`（内容为原 http.rs 中保留的部分 + 子模块声明）
3. 创建 `src/http/auth.rs`, `src/http/rate_limit.rs`, `src/http/handlers.rs`

`src/lib.rs` 的 `pub mod http;` 声明不需要改变。

### 2. 子模块声明（在 `mod.rs` 顶部）

```rust
mod auth;
mod rate_limit;
mod handlers;

// Re-export public types so `crate::http::AuthConfig` etc. still work
pub use auth::{AuthConfig, JwtConfig};
pub use rate_limit::RateLimiter;
```

`auth_config_from_env` 和 `rate_limiter_from_env` 是内部函数，
在 `mod.rs` 的 `build_router_with_auth_and_rate_limit` 中通过
`auth::auth_config_from_env()` 调用即可，不需要 re-export。

### 3. 迁移 `src/http/auth.rs`

剪切以下代码到新文件：
- `pub struct AuthConfig { ... }` + `impl AuthConfig`
- `pub struct JwtConfig { ... }` + `impl JwtConfig`
- `fn auth_config_from_env() -> AuthConfig`
- `async fn auth_middleware(...)`

### 4. 迁移 `src/http/rate_limit.rs`

剪切以下代码到新文件：
- `pub struct RateLimiter { ... }` + `impl RateLimiter`
- `fn rate_limiter_from_env() -> RateLimiter`
- `fn extract_client_key(...) -> String`
- `async fn metrics_middleware(...)`
- `async fn rate_limit_middleware(...)`

### 5. 迁移 `src/http/handlers.rs`

剪切所有 handler 函数：
- `async fn run_agent(...)`
- `async fn create_session(...)`
- `async fn list_sessions(...)`
- `async fn get_session(...)`
- `async fn list_tools(...)`
- `async fn health() -> &'static str`
- `async fn openapi_spec() -> Json<...>`
- `async fn metrics_handler(...)`
- 辅助函数：`generate_session_id`, `format_timestamp`, `days_to_ymd`, `is_leap`

Handlers 需要访问 `AppState`、请求/响应类型，在文件顶部加
`use super::{AppState, RunRequest, RunResponse, ...};`。

### 6. 更新各子模块的 `use` 语句

运行 `cargo build` 后，根据编译错误补全各文件缺少的 `use` 语句。
不要手工猜测，先写结构，再跑编译修错。

## 验收标准

1. `cargo build --all-features` 通过
2. `cargo test --workspace` 全绿
3. `cargo clippy --all-targets --all-features -- -D warnings` 干净
4. `cargo fmt --all -- --check` 干净
5. `src/http.rs` **不再存在**（已改为 `src/http/mod.rs`）
6. `src/http/mod.rs` 行数 **≤ 700**
7. 所有已有对 `crate::http::AuthConfig` / `crate::http::AppState` / 
   `crate::http::build_router*` 的引用无需修改

## 明确不在范围内

- ❌ 不改变任何 HTTP handler 的业务逻辑
- ❌ 不改变路由配置（路径、方法、中间件顺序）
- ❌ 不修改 OpenAPI spec 内容
- ❌ 不改变 SSE 流式输出逻辑

## 注意事项

- Rust 不允许同时存在 `src/http.rs` 和 `src/http/` 目录，
  必须先删除 `src/http.rs` 再创建 `src/http/mod.rs`
- `AppState` 中包含多种字段类型（`RateLimiter`, `AuthConfig` 等），
  定义保留在 `mod.rs`，避免循环依赖
- `SseEvent` / `SseContentBlock` 在 handlers 和 mod.rs 中都用到，
  放在 `mod.rs` 中定义，handlers.rs 用 `use super::SseEvent;`
