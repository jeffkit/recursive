# Journal — Goal 398: HTTP 准入有界化（排队超时 503）

- **Date**: 2026-09-27
- **Goal**: [.dev/goals/398-http-admission-bounded-503.md](../../goals/398-http-admission-bounded-503.md) / issue #25
- **Branch**: `feat/goal-398-admission-bounded`（worktree `.worktrees/goal-398-admission-bounded`）

## 与 goal 文本的偏差（重要，供 Goal 395 衔接）

1. **`SessionHost` 不存在**。依赖 issue #22（Goal 395）在 main 上尚未落地，本 goal 的
   行为价值（有界排队 → 503 + Retry-After）不依赖那次搬家。因此把「有界准入」实现为
   独立单元 `src/http/admission.rs::AdmissionGate`（HTTP-transport-free、无 axum 类型），
   挂在 `AppState.admission`。**Goal 395 落地时把 `AdmissionGate` 整体搬进 `SessionHost`
   即可**（字段 + 方法签名不变，9 个单测随迁），行为零变化。
2. **Goal 392（metrics gauges）也未落地**，但本 goal 明确要求 `runs_waiting` 递增/递减。
   只实现了 392 范围里与本 goal 相交的最小切片：`Metrics.runs_waiting: Arc<AtomicU64>`
   字段 + `/metrics` 暴露 `recursive_runs_waiting`。**392 执行时应跳过 runs_waiting
   （已存在），只补 `runs_in_flight` / `transcript_bytes_total`。**
3. 超时配置未进 `Config`（issue 只要求 env；且 `Config` 字面量散布 13 处，不值得为
   一个 env 开关放大 diff）。走 `AppState.session_ttl_secs` 的既有先例：在
   `main.rs` 里从 `RECURSIVE_ADMISSION_TIMEOUT_SECS` 解析（默认 30，`0` = 无限等待）。
4. README 环境变量表：除授权的 `RECURSIVE_ADMISSION_TIMEOUT_SECS` 外，顺手补了本就
   缺失的 `RECURSIVE_MAX_CONCURRENT_RUNS` 行（准入语义两行一起才读得通，纯文档增量）。

## 实现要点

- `AdmissionGate::acquire_run()`：`tokio::time::timeout(admission_timeout, sem.acquire_owned())`；
  `admission_timeout == Duration::ZERO` 时退化为裸 acquire（旧无限等待语义，兼容开关）。
- 等待计数：私有 `WaitingGuard`（RAII，Drop 里 `fetch_sub`），进入 acquire 前 +1，
  拿到 permit / 超时 / 取消 / 出错时 guard drop → 归位。超时路径上 permit 不可能泄漏：
  `timeout` 先于 inner future 完成时 inner 被 drop（`OwnedSemaphorePermit` 的 Drop 归还）；
  pin 测试 `timeout_does_not_leak_permits`。
- `Retry-After`：`ceil(runs_waiting / max_concurrent)` 的**粗估**（注释里写明是估计），
  整数秒、下限 1、clamp 到 u32。经既有 `ApiError::with_retry_after`（Goal-313）注入，
  零新 header 管道。
- `/agui` 改走 `AdmissionGate::try_acquire_run()`（同一信号量、同一 try 语义、同一 503
  响应形状）——「不等待」档保持不变，既有测试 `agui_run_respects_run_semaphore` pin 住。
- `max_concurrent_runs` 默认值（8）与语义（0=无限）未动；`rate_limit.rs` 未动。

## Files touched

| 文件 | 变更 |
|---|---|
| `src/http/admission.rs` | 新增：`AdmissionGate` / `AcquireError` / `RunPermit` / `WaitingGuard` + 9 单测 |
| `src/http/mod.rs` | `mod admission` + 导出；`Metrics.runs_waiting`；`AppState.run_semaphore` → `AppState.admission` |
| `src/http/handlers.rs` | `/run`、`/sessions/:id/messages` 改走 `acquire_run`（503+Retry-After）；`/agui` 改走 `try_acquire_run`；`/metrics` 增 `recursive_runs_waiting`；5 处测试 AppState 字面量 |
| `crates/recursive-cli/src/main.rs` | env 解析（默认 30）+ gate 构造 + 启动日志一行 |
| `tests/http.rs` | 字面量迁移 + 新集成测试 `messages_returns_503_with_retry_after_when_admission_saturated` |
| `tests/http_common/mod.rs` / `tests/agui_e2e.rs` / `tests/v050_integration.rs` | 字面量迁移 |
| `README.md` | HTTP server env 表补两行（见偏差 #4） |

## Tests added

- `cargo test --lib admission`：9 个单测全绿
  - `acquire_run_times_out_on_saturated_pool`（0-permit + 50ms → Timeout，runs_waiting 归零）
  - `acquire_run_zero_timeout_waits_indefinitely`（0 → 150ms 后拿到释放的 permit）
  - `runs_waiting_counts_while_blocked` / `acquire_run_success_leaves_runs_waiting_at_zero`
  - `try_acquire_run_never_blocks`
  - `timeout_does_not_leak_permits`（3 次超时后 release，permit 立即可取——卡泄漏路径）
  - `closed_semaphore_maps_to_closed_error`
  - `estimate_retry_after_scales_with_queue_depth`（ceil 语义）/ `estimate_retry_after_unlimited_pool_floors_at_one`
- `cargo test --test http`：`messages_returns_503_with_retry_after_when_admission_saturated`
  （占满 1 permit → 150ms 后 503 + Retry-After 整数 ≥1 + `{"error":...}` 形状 + runs_waiting 归零
  → drop permit → 同请求 200）
- 回归：`agui_run_respects_run_semaphore` 不变通过；全 workspace `cargo test --workspace`
  38 个 suite 全绿（含 recursive-tui 818 例）。

## 手工实测（acceptance 第 3 条）

mock OpenAI 后端（`/chat/completions` sleep 8s）+ 真实二进制：

```
RECURSIVE_MAX_CONCURRENT_RUNS=1 RECURSIVE_ADMISSION_TIMEOUT_SECS=3 \
  ./target/debug/recursive http --addr 127.0.0.1:3998
# 启动日志（新增行）：
admission: max_concurrent_runs=1 (0 = unlimited), admission_timeout=3s (0 = wait indefinitely)
```

请求 #1 先发（占住唯一 permit），1s 后请求 #2：

```
$ time curl -X POST /run -d '{"goal":"second request should get 503"}'
{"error":"server at capacity: no run slot after waiting 3s, try again later"}
HTTP_STATUS:503
        3.033 total        ← 精确等满 3s 配置窗口

retry-after: 1          ← 整数秒
```

请求 #1 在 mock 延迟后正常完成（`"status":"success"`），permit 释放无泄漏。

## 质量门

- `cargo test --workspace` ✅（38 suite，0 failed）
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` ✅
- `cargo fmt --all` ✅
- e2e 回归（08-http-api / 08b-http-rate-limit / 19-http-interrupt）：**未能在本机执行——
  环境阻塞，非本变更问题**。过程记录：
  1. 首跑 `argus-init` 失败（exit 5）＝新 worktree 未构建 `e2e/plugins/dist`，
     `cd e2e/plugins && npm install && npm run build` 后 init 通过。
  2. `argus-build` 连续两次在同一层失败（17s 处）：
     `RUN pip3 install --break-system-packages /sdk/python` →
     `ERROR: Could not find a version that satisfies the requirement setuptools>=61.0 (from versions: none)`。
  3. 定性探针：`docker run --rm python:3.11-slim pip download setuptools` 同样报
     `from versions: none`——**Docker VM 当前完全无法访问 PyPI**（同期 deb.debian.org
     的 apt 下载也退化到 Ign 重试）。约 30 分钟前另一个 worktree（937828b，Goal 399 分支）
     曾成功构建同一镜像，说明是本机容器网络在近期退化，与本次 diff 无关。
  4. 替代路径 `e2e-run-host.sh` 按脚本自述目前只覆盖 smoke suite，08/08b/19 不适用。
  - 补偿验证：`tests/http.rs` 全量 HTTP 集成测试（98 例）+ 本 goal 新增集成用例 +
    上述真实二进制手工饱和实测，覆盖三个套件回归的核心面（/run、/messages、限流层
    未动、/agui 语义未动）。环境恢复后可直接补跑：
    `sh .dev/scripts/e2e-run.sh 08-http-api && sh .dev/scripts/e2e-run.sh 08b-http-rate-limit && sh .dev/scripts/e2e-run.sh 19-http-interrupt`
    （08 的镜像构建已到最后一层，只差 pip 步骤）。

## Notes

- 影响面（GitNexus MCP 本会话不可用，改为手工 grep 影响分析）：`AppState.run_semaphore`
  的全部 12 处构造点（1 生产 + 11 测试）已迁移；除 `handlers.rs` 三个准入点外无其他
  `acquire_owned/try_acquire_owned` 调用者；`Config` 零改动。
- 503（容量不足）与 429（限流）保持分离：准入超时只出 503，`rate_limit.rs` 未动。
- `/metrics` 新增 `recursive_runs_waiting`（gauge）；既有 metrics 文本断言是
  contains 式，已加一行 `recursive_runs_waiting 0` 断言 pin。
