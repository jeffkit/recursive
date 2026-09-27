# Goal 398 — HTTP 准入有界化：排队超时即 503（不再无限等待）

**Roadmap**: Phase 17 Production Hardening。默认 8 并发的闸门本身是合理设计；
问题是被拒时**无限排队**而不是快速失败。

**依赖**: Goal 395（准入逻辑迁入 `SessionHost`）。

**Design principle check**:
- Implemented as: 在宿主层的 `acquire_run` 上包一层 `tokio::time::timeout`，超时返回
  `503 Service Unavailable` + `Retry-After`；等待计数接 Goal 392 的 gauge。
- ❌ Does NOT 改 `max_concurrent_runs` 的默认值（8）与其语义（0 = 无限）。
- ❌ Does NOT 改 `/agui` 现有的快速 503 语义（`handlers.rs:1620` 用
  `try_acquire_owned()`）——保持为「不等待」档。
- ❌ Does NOT 引入新依赖。

## Why（2026-09-27 核实）

- 两处准入是无界等待：`/run`（`src/http/handlers.rs:118-128`）与
  `/sessions/:id/messages`（`:895-903`）都是 `acquire_owned().await`，注释自己写着
  「block until a permit is free」。
- `/agui`（`:1620-1633`）是 `try_acquire_owned()` → 立即 503——**同一个服务里两种语义不一致**。
- 实测（mock LLM 200ms）：cap=8 时 64 客户端 p50 延迟 1.66 s、吞吐 38 turns/s；
  cap=64 时 p50 208 ms、295 turns/s。等待是**真实且可观测**的，但客户端拿不到
  「你在排队」的信号，只会看到请求挂着。
- 叠加 `RECURSIVE_WALL_TIMEOUT_SECS` 目前**完全无效**（`Config::wall_timeout_secs`
  在 `src/config.rs:426` 被解析，但 `src/runtime.rs:665` 硬编码
  `wall_timeout_secs: 0`——Goal 399 才接线），所以一个卡住的 turn 可以无限期占住 permit：
  无界排队 + 无限执行时长 = 全服饿死。

## Scope（do exactly this, no more）

### 1. 有界准入

- 在 `SessionHost`（Goal 395）里提供：

  ```rust
  pub async fn acquire_run(&self) -> Result<RunPermit, AcquireError>;
  // AcquireError::Timeout { waited: Duration } | AcquireError::Closed
  ```

  内部：`tokio::time::timeout(admission_timeout, semaphore.clone().acquire_owned())`。
- 超时时间来自 env `RECURSIVE_ADMISSION_TIMEOUT_SECS`：
  - 未设置 → 默认 **30 秒**（现状是无限；30 s 是「明确失败好过静默悬挂」的取舍）。
  - `0` → 保持无限等待（**兼容开关**，给依赖旧行为的部署留退路）。
- 两个无界等待点（`/run`、`/sessions/:id/messages`）改为调用 `acquire_run`；
  超时映射为 `ApiError::new(StatusCode::SERVICE_UNAVAILABLE, ...)` 并带
  `Retry-After: <estimated seconds>`（估计值可用 `runs_waiting / max_concurrent` 粗算，
  在注释里写明是估计）。
- 等待期间 `runs_waiting`（Goal 392）递增、结束递减——用 RAII guard，覆盖超时/取消/错误路径。

### 2. 启动日志与文档

- 启动时打印一行实际生效的准入参数（`max_concurrent_runs`、`admission_timeout`），
  便于运维核对（现有启动日志风格为准）。
- README 环境变量表补 `RECURSIVE_ADMISSION_TIMEOUT_SECS`（本 goal 授权这一行最小改动）。

### 3. 测试（agent-presence / agent-mutants 门）

- 单测：0 permit 的 semaphore + `admission_timeout = 50ms` → `acquire_run` 在 ~50ms 后
  返回 `Timeout`，`runs_waiting` 归零。
- 单测：`admission_timeout = 0` → 行为等价于无限等待（用一个稍后释放 permit 的任务验证）。
- 集成（`tests/http.rs` 风格）：占满 permit 后 `POST /sessions/:id/messages` 返回 503
  且响应体/头符合既有 `ApiError` 形状；释放 permit 后请求成功。
- 回归：`/agui` 的 503 语义不变（既有测试 `agui_run_respects_run_semaphore`，
  `src/http/handlers.rs:2421`）。

## Files NOT to touch

- `src/runtime.rs`、`src/kernel.rs`、`src/run_core.rs`（执行时长接线是 Goal 399）。
- `src/config.rs` 中 `max_concurrent_runs` 的解析与默认值。
- `src/http/rate_limit.rs`（限流是另一层；不要把准入超时和限流混在一个计数器里）。

## Acceptance

- `cargo test --workspace`、`cargo clippy --workspace --all-targets --all-features -- -D warnings`、
  `cargo fmt --all` 全绿。
- 新测试按名可跑：`cargo test --lib admission` 与 `cargo test --test http` 新用例。
- 手工实测（journal 贴输出）：`RECURSIVE_MAX_CONCURRENT_RUNS=1` +
  两个并发请求 → 第二个在 30 s（或配置值）后返回 `503` 并带 `Retry-After`。
- e2e 回归：`sh .dev/scripts/e2e-run.sh 08-http-api`、`08b-http-rate-limit`、
  `19-http-interrupt` 通过。
- Journal: `.dev/journal/manual-20260927-goal398-admission-bounded.md`。

## Notes for the agent (traps)

- **不要顺手把 `max_concurrent_runs` 默认值调大**：它默认 8 是因为执行时长无界
  （Goal 399 才修）。调大闸门而不设执行上限只会把内存与配额压力放大。
- **503 与 429 不要混**：429 是限流（`rate_limit.rs`），503 是容量不足。客户端重试策略不同。
- `Retry-After` 要能被解析为整数秒；不要输出小数或区间字符串
  （`.dev/ROADMAP` 里有过 Retry-After 解析相关的未完成项，别引入同类问题）。
- 超时后的 permit **必须不泄漏**：`timeout` 放弃的 future 若已持有 permit，需要用
  `OwnedSemaphorePermit` 的 drop 语义保证归还——写测试卡住这条路。

