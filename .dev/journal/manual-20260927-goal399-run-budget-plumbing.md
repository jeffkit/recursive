# Journal — Goal 399 — RECURSIVE_WALL_TIMEOUT_SECS 接线 + HTTP 会话安全默认

- **Date**: 2026-09-27
- **Goal**: `.dev/goals/399-run-budget-plumbing-wall-timeout.md`（issue #26）
- **Branch**: `feat/goal-399-wall-timeout`
- **前置**: Goal 385（kernel/runtime 两项）已在本分支 commit 1 先行落地，
  见 `manual-20260927-goal385-size-headroom.md`。

## Files touched

| 文件 | 改动 |
|------|------|
| `src/kernel.rs` | `AgentKernel` + `AgentKernelBuilder` 增加 `wall_timeout_secs`（默认 0=无界）；`AgentKernel::run` 解析有效预算（ctx 显式值 > kernel 默认）并填 `RunCore` |
| `src/runtime.rs` | `execute_kernel_turn` 的 `TurnContext` 硬编码 0 → `self.kernel.wall_timeout_secs` 字段读取 |
| `src/runtime/builder.rs` | `AgentRuntimeBuilder::wall_timeout_secs(u64)` 转发（对齐 `max_steps` 写法） |
| `src/multi.rs` | `AgentPool` 从父会话 `Config` 继承 `wall_timeout_secs`；`run_with_role` 的 kernel+ctx 使用继承值 |
| `crates/recursive-cli/src/cli/builder.rs` | CLI 装配传入 `config.wall_timeout_secs` |
| `crates/recursive-cli/src/main.rs` | 单发 run 路径同上；HTTP `serve` 路径把解析出的 HTTP 默认写进 server 的 config 副本 |
| `src/http/mod.rs` | `http_session_budget_from_env()`：`RECURSIVE_HTTP_MAX_STEPS`（默认 100）/ `RECURSIVE_HTTP_WALL_TIMEOUT_SECS`（默认 1800），显式 `0` = 无界，非法值回退默认 |
| `src/http/handlers.rs` | 4 个 `AgentRuntimeBuilder` 装配点全部加 `.wall_timeout_secs(state.config.wall_timeout_secs)` |
| `README.md` | 快速开始表 + LLM provider 表补 `RECURSIVE_WALL_TIMEOUT_SECS`、`RECURSIVE_HARD_STEP_CAP`（与 per-session `max_steps` 的 min 关系）；HTTP server 表补两个新 env |
| `docs/INTERNALS.md` | §5 `AgentKernel` 状态表补 `wall_timeout_secs`。**未加** `WallClockExceeded` 触发条件句 —— 该文档不逐条描述 turn 终止原因（issue 的「若…」条件不成立） |

## 子代理继承：选择与理由

`AgentPool::new` 的 `_config` 参数本来就是父会话 `Config`（此前带下划线未用）。
选择**从 config 继承**而非新增独立 env：CLI 路径 config 即父预算；HTTP 路径
`serve` 启动时把解析出的安全默认写进同一份 config，因此注册 sub-agent 工具
（`register_subagent_if_enabled`）发生在写入之后，池里继承到的就是会话的实际预算。
父无界（0）→ 子仍无界（保持既有语义）；父有界 → 子必有界，一个宽 manifest 不能
再让 permit 被无界子代理长期占用。独立子代理预算（单独 env）本次不做、文档未承诺。

## 受影响面（HTTP 默认从无界 → 有界）

- `POST /run`、`POST /sessions`、`POST /sessions/:id/messages`、session fork/resume
  四条路径：max_steps 兜底从 `RECURSIVE_MAX_STEPS`（默认无界）变为 100（请求体
  `max_steps` 仍优先生效）；wall 预算从无界变为 1800s。
- `RECURSIVE_MAX_STEPS` 在 HTTP server 进程内不再直接生效（被
  `RECURSIVE_HTTP_MAX_STEPS` 取代）——README 已写明；CLI/TUI 完全不受影响。
- 恢复旧行为：两个 env 任一显式设 `0`。

## 四个 guard 的行数变化（385 headroom 使用情况）

| Guard | 限制 | 385 后 | 399 后 |
|-------|------|--------|--------|
| `src/kernel.rs` | 1000 | 589 | **615**（余 385） |
| `src/runtime.rs` | 3700 | 1476 | **1476**（改的是既有行，净 0） |
| `src/run_core.rs` production | 1500 | ~1467 | ~1467（本 goal 未触碰） |
| `run_inner` body | 150 | 147 | 147（本 goal 未触碰） |

没有抬任何 invariant 上限；kernel.rs 的增量是字段 + setter + 解析逻辑（~26 行），
远未顶到上限。

## Tests added

- `tests/invariants/finish_reason_data.rs`（`--test invariants`，39→41）：
  `WallClockExceeded` 补进 serde roundtrip / Display 断言；
  `wall_clock_exceeded_is_data_not_error_and_transcript_is_kept` —— 1s 预算 +
  首响应前 sleep 2s 的假 provider，断言 `Ok(outcome)` + `WallClockExceeded { secs: 1 }`
  **不是 Err**、transcript 未丢（invariant #7）；
  `wall_timeout_zero_keeps_legacy_unlimited_behaviour` —— 0 预算同样脚本正常跑完。
- `src/kernel/tests.rs`（+3）：kernel 级预算作为 ctx=0 时的默认生效；ctx 显式值
  优先于 kernel 默认；builder 默认值为 0。
- `src/runtime/tests.rs`（+2）：`AgentRuntimeBuilder::wall_timeout_secs` 转发到
  kernel 字段；默认 0。
- `src/multi.rs::wall_budget_tests`（+2）：父 1s → 子 1s 命中
  `WallClockExceeded`（子不允许无界）；父 0 → 子照旧无界跑完。
- `src/http/mod.rs::budget_tests`（+1，env 断言按 issue 要求合并为单测）：默认
  (100,1800)、显式覆盖、显式 0、垃圾值回退默认。
- 备注：issue 写的 `cargo test --test finish_reason_data` 实际 target 名是
  `--test invariants`（finish_reason_data 是其中的 mod），与 385 的备注一致。

## Gates

- `cargo test --workspace`：38 个 target 全部 ok（lib 2289 通过）
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`：干净
- `cargo fmt --all`：干净
- Grep 验收：`rg "wall_timeout_secs" src/runtime.rs src/multi.rs` 生产路径全部为
  字段读取（`src/multi.rs:706` 的 `: 0` 是 `#[cfg(test)]` fixture 的 Config 默认值，
  非子代理路径）；`rg "RECURSIVE_WALL_TIMEOUT_SECS"` 命中解析点（config.rs）、
  消费点（cli builder.rs / main.rs）、文档（README ×2）。
- e2e 回归：见下节。

## e2e 回归（Docker 模式，image `recursive:e2e-wt-937828b`）

| Suite（e2e.yaml id） | 结果 |
|------|------|
| `http-api`（08） | **passed 21/21** |
| `goal-loop`（18） | **passed 7/7** |
| `http-interrupt`（19） | **passed 3/3** |

三个套件在新默认（HTTP 会话 max_steps 兜底 100 / wall 1800s）下全部通过；
08 用例自身带显式 `"max_steps": 5`，18/19 为短交互，均不受兜底值影响。

环境备注（本次踩坑，均已记入 `.dev/AGENTS.md` e2e 规则）：
1. buildkit 对 base image 的 registry metadata 解析走 `auth.docker.io`（本机 DNS
   污染 → i/o timeout），而 daemon `docker pull` 正常。先把 `rust:1.88-slim` /
   `debian:bookworm-slim` / `docker/dockerfile:1.4` pull 到本地，再
   `docker build -f e2e/Dockerfile -t recursive:e2e-wt-<HEAD> .` 即可。
2. `argus-run --filter` 匹配的是 e2e.yaml 的 suite **id**（`http-api` /
   `goal-loop` / `http-interrupt`），不是文件名风格（`08-http-api` 会得到
   `SUITE_NOT_FOUND`）；e2e-run.sh 会把原始错误吞成 `status=None totals={}`。
   排查方式：手动走 mcp2cli 生命周期看 argus-run 的原始 JSON。
