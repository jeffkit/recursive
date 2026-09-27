# Goal 392 — HTTP 观测：补齐密度与队列 gauge（在飞 / 等待 / 会话 / transcript 字节）

**Roadmap**: 生产化观测（ROADMAP v4 Phase 15 Observability & Monitoring 的缺口）。
密度与准入相关的任何改动都必须先可度量——本 goal 是 milestone 批次 1 的第一步。

**依赖**: 无（只动 `src/http/`）。

**Design principle check**:
- Implemented as: 在既有 `Metrics`（`src/http/mod.rs:47-66`，lock-free `AtomicU64`）上增加
  gauge，并在 `metrics_handler` 里以 Prometheus 文本暴露；不新增依赖。
- ❌ Does NOT 改变任何 agent 语义、准入逻辑或 session 生命周期。
- ❌ Does NOT 引入新 crate（RSS 若无法用 std 跨平台取得，允许 Linux-only `#[cfg]` 读
  `/proc/self/statm`，并在文档里标注 best-effort；不允许为此加依赖）。

## Why（2026-09-27 核实）

- 现有指标只有计数器与 `sessions_active`：`requests_total/active`、`agent_runs_*`、
  `agent_steps_total`、`tokens_*`、`sessions_active`、`rate_limits_rejected_total`
  （`src/http/mod.rs:47-66`）。**没有**「在飞 run」「等待准入的请求」「transcript 总量」「内存」。
- 实测：默认 `max_concurrent_runs=8` 在 200ms LLM 延迟下把吞吐压到 38 turns/s、p50 拉到 1.66 s
  （cap=64 时为 295 turns/s / 208 ms）。没有队列 gauge，线上只能看到「慢」，看不到「在排队」。
- 后续 Goal 395（宿主层）/398（准入策略）需要一个可断言的观测面，否则无法验收。

## Scope（do exactly this, no more）

### 1. `Metrics` 增加 gauge

在 `src/http/mod.rs` 的 `Metrics` 上加：

- `runs_in_flight: AtomicU64` — 已获取 `run_semaphore` permit、正在跑 turn 的数量。
- `runs_waiting: AtomicU64` — 正在 `acquire_owned().await` 等待 permit 的数量。
- `transcript_bytes_total: AtomicU64` — 所有存活会话 transcript 的字符/字节总量（见下）。

实现要求：

- `runs_in_flight` / `runs_waiting` 的增减必须用 **RAII guard**（`Drop` 里递减），
  因为 `handlers.rs:118`、`:895`、`:1620` 三处都有 `?` 提前返回路径——手写加减一定会漏。
  可以在 `src/http/handlers.rs` 里放一个小的私有 guard 结构体（或放到新的
  `src/http/metrics_guard.rs`，避免 `handlers.rs`（3358 行）继续膨胀）。
- `runs_waiting` 的语义：**进入 acquire 前 +1，拿到 permit 后 -1**（含错误/取消路径）。
- `transcript_bytes_total`：在 `metrics_handler` 里遍历 `state.sessions`（只取 `read` 锁，
  累加 `runtime.lock().try_lock()` 成功者的 transcript 字符数；失败者跳过并计入一个
  `transcript_bytes_skipped` 计数），**不要在每轮热路径上维护全局累加器**。
  若 `try_lock` 版本实现复杂，退化为「会话数 × 已知固定开销」是错误的，宁可只报
  `sessions_active`——不允许用一个假数字充数（本项允许在 journal 里记录降级决定）。

### 2. 暴露到 `/metrics`

- 在 `metrics_handler` 的输出里加：
  `recursive_runs_in_flight`、`recursive_runs_waiting`、`recursive_transcript_bytes_total`
  （可选 `recursive_process_resident_memory_bytes`，Linux-only，best-effort）。
- 保持既有输出格式与顺序稳定（已有测试断言了 metrics 描述文本，见
  `src/http/handlers.rs:2890` / `:2894`）。

### 3. 测试（agent-presence 门要求同批）

- 单测：acquire → `runs_in_flight==1`；在 acquire 阻塞时 `runs_waiting==1`；
  释放/错误返回后两者归零（用零 permit 的 `Semaphore` 复现等待态，参考
  `src/http/handlers.rs:2421` 既有测试的构造方式）。
- 单测：`/metrics` body 含三个新名字（沿用 `:2890` 的断言风格）。

## Files NOT to touch

- `src/run_core.rs`、`src/kernel.rs`、`src/runtime.rs`（内核与 line-budget）。
- 准入逻辑本身（队列上限、503 语义属于 Goal 398）——本 goal **只观测不改行为**。
- `src/http/rate_limit.rs` 的限流语义。

## Acceptance

- `cargo test --workspace`、`cargo clippy --workspace --all-targets --all-features -- -D warnings`、
  `cargo fmt --all` 全绿。
- 新测试按名可跑：`cargo test --lib metrics`（含新 gauge 用例）。
- Grep: `rg "runs_in_flight|runs_waiting|transcript_bytes_total" src/http/` 覆盖
  struct 定义 + 增减点 + `/metrics` 输出 + 测试（≥ 4 类命中）。
- 实测可跑：`curl -s localhost:3000/metrics | grep recursive_runs_`（在 journal 里贴输出）。
- Journal: `.dev/journal/manual-20260927-goal392-metrics-gauges.md`。

## Notes for the agent (traps)

- **guard 必须覆盖 `?` 早退**：`handlers.rs` 三处 permit 获取点都存在提前返回，
  用 `Drop` 而不是手工 `fetch_sub`。
- `/metrics` 是否需要鉴权取决于既有路由合并方式（Goal 272 的 route-level merge）——
  不要改鉴权结构，沿用现状。
- 不要为了「好看」把 `transcript_bytes_total` 变成每轮热路径累加：那会给所有会话
  加锁竞争，正好和本 milestone 要修的问题相反。
- `handlers.rs` 已 3358 行，优先把 guard 放独立文件而不是继续往里加。

