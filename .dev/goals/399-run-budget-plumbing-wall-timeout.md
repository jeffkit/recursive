# Goal 399 — 执行预算接线：`RECURSIVE_WALL_TIMEOUT_SECS` 从死配置变为生效 + HTTP 会话安全默认

**Roadmap**: Phase 17 Production Hardening。没有默认执行上限，任何卡住的 turn 都会长期
占住稀缺的准入 permit（Goal 398 让排队有界，本 goal 让执行有界）。

**依赖**: **Goal 385（硬前置）**——本 goal 必须给 `AgentKernel`/`AgentRuntime` 加字段与
setter，而 `src/kernel.rs` 是 998/1000 行、`src/runtime.rs` 是 3692/3700 行。
385 未落地前不要启动本 goal。

**Design principle check**:
- Implemented as: 只做**接线**——`wall_timeout_secs` 已存在于 `TurnContext`
  （`src/kernel.rs:111`）与 `RunCore`，检查逻辑与 `FinishReason::WallClockExceeded`
  （`src/run_core.rs:750-775`）都已实现；缺的是 config → builder → runtime → TurnContext 这一段。
- ❌ Does NOT 新增 finish reason（invariant #7：`WallClockExceeded` 已存在）。
- ❌ Does NOT 把超时变成 `Err`——必须仍是 `Ok(RuntimeOutcome { finish_reason })`。
- ❌ Does NOT 改 `max_steps=0 表示无限` 的既有契约（`src/run_core.rs:1450-1463`）。

## Why（2026-09-27 核实）

- `Config::wall_timeout_secs` 在 `src/config.rs:426` 从 `RECURSIVE_WALL_TIMEOUT_SECS`
  解析（默认 0），`src/config.rs:55/595` 存进结构体——**但全仓没有任何消费点**。
- `src/runtime.rs:665` 硬编码 `wall_timeout_secs: 0`；`src/multi.rs:390` 与 `:696`
  （子代理路径）同样硬编码 0。于是：**`RECURSIVE_WALL_TIMEOUT_SECS` 今天是一个完全无效的
  环境变量**，CLI 与 HTTP 都一样。
- `RunCore` 侧是完备的：`check_wall_deadline`（`src/run_core.rs:750-775`）在
  `wall_timeout_secs == 0` 时短路，否则产出
  `FinishReason::WallClockExceeded { secs }` 并 emit `TurnFinished`——符合 invariant #7。
- 另外 `RECURSIVE_HARD_STEP_CAP`（`src/run_core.rs:1467+`）是**进程级**的步数天花板，
  今天就能用（不依赖 builder）；本 goal 要在文档里把它与 per-session `max_steps` 的
  关系写清。
- 默认无界 + 准入无界（Goal 398 前）= 全服饿死的组合；实测 cap=8 时 p50 延迟已达 1.66 s。

## Scope（do exactly this, no more）

### 1. builder → runtime → TurnContext 接线

- `AgentKernelBuilder::wall_timeout_secs(u64)`（与既有 `max_steps` 同风格）。
- `AgentRuntimeBuilder::wall_timeout_secs(u64)` 转发（对齐 `max_steps` 的转发写法，
  `src/runtime/builder.rs:157-161`）。
- `src/runtime.rs:665` 用运行时字段替换硬编码 0；`AgentKernel` 存该字段并在
  `AgentKernel::run` 填 `TurnContext`（`src/kernel.rs:337-342` 附近已有 wall 相关代码）。
- **子代理**（`src/multi.rs:390`、`:696`）：默认**继承父会话**的预算（而不是各自无界），
  并在 journal 写明选择理由。若要给子代理独立预算，用单独的 env，且必须在文档里说明
  与父预算的关系（取小/独立）。

### 2. 前端接入

- CLI：把 `config.wall_timeout_secs` 传给 builder（`crates/recursive-cli/src/cli/builder.rs`
  的装配处），使 README/配置里承诺的变量真正生效。
- HTTP：为 HTTP 创建的会话提供**安全默认**（可用 env 覆盖）：
  - `RECURSIVE_HTTP_WALL_TIMEOUT_SECS`，默认 **1800**（30 分钟）。
  - `RECURSIVE_HTTP_MAX_STEPS`，默认 **100**。
  - 显式设为 `0` → 回到无界（兼容开关）。
  这两个值只在 HTTP 建 runtime 时应用，不影响 CLI/TUI。
  与 Goal 393 的 helper 放在一起（同一个前后端无关的装配函数），避免再次漂移。

### 3. 文档

- README 环境变量表：补 `RECURSIVE_HTTP_WALL_TIMEOUT_SECS` / `RECURSIVE_HTTP_MAX_STEPS`，
  并给 `RECURSIVE_WALL_TIMEOUT_SECS` 加一句「本 goal 起真正生效」（本 goal 授权
  这几行最小修改）。
- 若 `docs/INTERNALS.md` 描述了 turn 终止原因，补 `WallClockExceeded` 的触发条件（一句话）。

### 4. 测试（agent-presence / agent-mutants 门）

- **核心测试（invariant #7）**：用一个「首次响应前 sleep」的假 provider（或
  `MockProvider` + 1 秒 wall timeout），断言 `AgentRuntime::run` 返回
  `Ok(outcome)` 且 `outcome.finish_reason == FinishReason::WallClockExceeded { secs }`，
  **不是 `Err`**，且 transcript 仍被保存（沿用既有 finish-reason 测试的模式，
  参考 `tests/invariants/finish_reason_data.rs`）。
- 单测：`wall_timeout_secs(0)` → 行为与今天完全一致（不触发）。
- 单测：HTTP 会话装配应用了 `RECURSIVE_HTTP_MAX_STEPS` / `..._WALL_TIMEOUT_SECS`
  默认值，且 `=0` 时不设限（env 测试必须合并为一个测试）。
- 子代理继承预算的单测（父 1 秒 → 子也是 1 秒；不允许子代理无界）。

## Files NOT to touch

- `src/run_core.rs` 的 deadline 检查逻辑（已正确）。
- `src/config.rs` 中 `wall_timeout_secs` 的解析与默认值（只增加消费点）。
- compaction / transcript 上限（属 Goal 393）。
- `.dev/flows/`、`.flowcast/`。

## Acceptance

- `cargo test --workspace`、`cargo clippy --workspace --all-targets --all-features -- -D warnings`、
  `cargo fmt --all` 全绿。
- `cargo test --test finish_reason_data` 绿，且新增的 `WallClockExceeded` 用例命中。
- Grep: `rg "wall_timeout_secs" src/runtime.rs src/multi.rs` 不再出现硬编码 `: 0`
  （应为字段读取）。
- Grep: `rg "RECURSIVE_WALL_TIMEOUT_SECS" src/ crates/ README.md` 出现解析点 + 消费点 + 文档。
- e2e 回归：`sh .dev/scripts/e2e-run.sh http-api`、`goal-loop`、`http-interrupt` 通过
  （HTTP 默认加了步数上限，长 goal 用例必须仍然通过——若某个用例需要更长预算，
  在该 e2e 里显式设置 env 并在 journal 说明，**不要**为了过测试把默认值调大）。
- Journal: `.dev/journal/manual-20260927-goal399-run-budget-plumbing.md`，记录四个 guard 的
  行数变化（证明 385 的 headroom 被正确使用、没有把文件顶到上限）。

## Notes for the agent (traps)

- **本 goal 是 385 之后最典型的「提取式改动」**：如果发现 `runtime.rs` headroom 又不够，
  不要顺手抬 invariant 上限（385 的规则：先提取，抬限是例外且需 ≥50 行余量）。
- **超时必须是 finish reason**：任何把 `WallClockExceeded` 变成 `Error::...` 的实现都会被
  `finish_reason_data` 测试打回，也会破坏自迭代的 auto-resume（依赖 transcript 一定落盘）。
- **子代理预算不要留空**：今天的 0 = 无界，一个宽 manifest 就能让 8 个 permit 里的
  几个被无界子代理吃掉；默认继承父预算是本 goal 的安全语义。
- HTTP 默认 `max_steps=100` 会改变长 goal 的行为——这是有意的，但必须在 journal 里
  列清受影响面，并确认 e2e 与既有 HTTP 集成测试全绿。

