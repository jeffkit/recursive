# Manual — 20261009 — CI: E2E 无 Docker 化 + 变异测试/定期回归 workflow

- **Date**: 2026-10-09
- **Goal**: 把 argusai E2E 与 cargo-mutants 变异测试接上 GitHub Actions；
  E2E 在 CI 上零 Docker（aimock 走 npm 原生进程，不用 ghcr 容器）；
  变异测试 PR 增量（diff 作用域）+ 双周全量基线 + 双周完整编译回归。

## Files touched

- `.github/workflows/e2e.yml`（新增）— PR smoke 门（LOCAL_ONLY，无 Docker）+
  双周 full-sweep（HostRuntime 逐 suite，advisory）。
- `.github/workflows/mutants.yml`（新增）— PR 增量变异（三脚本无参，--in-diff
  函数级/文件级）+ 双周全量基线（3 crate matrix）+ 双周完整编译回归
  （all-features build + 全测 + all-features clippy + fmt）。均 advisory 起步。
- `.dev/scripts/e2e-local.sh` — 新增 `E2E_AIMOCK_ENGINE=npx` 引擎：aimock 以
  `npx -p @copilotkit/aimock@1.44.0 llmock` 原生进程跑（默认 docker 不变）；
  readiness 轮询 90s（npx 首次下载）；cleanup 杀子进程再杀 npx。
- `.dev/scripts/e2e-gate.sh` — 新增 `RECURSIVE_E2E_LOCAL_ONLY=1`：docker
  daemon / mcp2cli / argusai-mcp 不再是前置，本地红不再 fallback Docker。
- `.dev/scripts/e2e-run-host.sh` — 透传 `E2E_AIMOCK_ENGINE` /
  `E2E_AIMOCK_PID_FILE`；cleanup 按 PID 文件整组收割插件 spawn 的 aimock。
- `e2e/plugins/src/index.ts` — host 模式新增 npx 引擎分支（detached spawn
  llmock、readiness 轮询、PID 落盘、日志进 tmp）；docker 路径原样保留。
  版本 pin 三处同步：本文件 aimockNpxSpec / e2e-local.sh AIMOCK_NPX_SPEC /
  e2e.yml Install 步。

## Tests added / verification

- 本地实测（colima 关闭，证明零 Docker）：
  - `E2E_AIMOCK_ENGINE=npx e2e-local.sh` → smoke PASS（首跑 27s 含 npx 下载，
    复跑 2s）。
  - `RECURSIVE_E2E_LOCAL_ONLY=1 E2E_AIMOCK_ENGINE=npx e2e-gate.sh` → PASS；
    docker daemon 缺席时前置检查正确放行（默认模式依旧 HARD-FAIL）。
  - 插件 npx 分支直调 setup() → REPLAY 启动、readiness 轮询、PID 落盘、
    /v1/models 探活全过；`kill -- -PGID` 进程组收割后端口释放。
- actionlint（含 shellcheck）+ 全部 inline run 脚本 bash -n 通过。
- 已知本地验证盲区：mcp2cli session daemon 在 DSH 沙箱里起不来
  （`session daemon exited with code 1`），argusai host 全链路未在沙箱内
  走通；该机制与既有 docker 模式 gate 共用、非本次改动面，CI 无沙箱不受影响。

## Notes

- npm 上 `aimock@0.2.9`（towyuan）是同名不同项目，别装错；要装
  `@copilotkit/aimock`。flag 式用法用其 `llmock` bin（`aimock` bin 无
  `--fixtures`）。
- workflow 的 diff 基准陷阱：actions/checkout 只建 `origin/*`，两个 workflow
  都先 `git branch main origin/main` 再跑，否则 diff 恒空 → 门假绿。
- schedule：`37 18 1,15 * *`（北京 2/16 日 01:37，避开整点高峰）；只在默认
  分支生效；60 天无提交自动停用。
