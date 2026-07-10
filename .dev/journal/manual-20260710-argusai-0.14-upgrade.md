# Manual edit: argusai-0.14-upgrade

**Date**: 2026-07-10
**Goal**: 升级 E2E 到 argusai 0.14.1（用户告知此前提的 #5/#6/#7 issue 已修），让 Recursive 的 E2E 门与套件在 0.14.1 下真正可用。

## 背景

此前向 argusai 提的 3 个 issue（#5 auto-setup、#6 退出码、#7 output.length）已在 0.14.1 修复。0.14.1 只发布 `argusai-mcp` / `argusai-core`，`argusai-cli` umbrella 冻在 0.12.3。

## 关键发现

1. **CLI 0.12.3 有独立 regression**：`argusai run` 不执行 setup 的 `exec` command（`sleep 8` 探针 → 墙钟仅 3s，setup 仍报 ✓）。Recursive 的门走 MCP 路径，不受此影响，但手动 `argusai` CLI 已不可靠。
2. **0.14.1 生命周期变 5 步**：`init → build → setup → run → clean`，新增 `argus-build`（构建 service 镜像，必须在 setup 之前，否则 `INVALID_STATE`）。
3. **假绿灯根因（最严重）**：0.14.1 的 `argus_run` 按 suite **`name`**（自由文本）归属 case 事件，而事件上的 `suite` 恒为 yaml 文件的 `name:`。`e2e.yaml` 条目 `name` 与 yaml 文件 `name:` 不一致时，该套件所有 case 事件被丢弃 → `total:0`/`failed:0`/`status:passed` 假绿。`argus_run` 又会省略 passed case，使 `total:0` 的空跑与真实全过无法区分。仓库 38 套件中 20 个不一致，全中招。已提 issue #8：按 `id` 而非 `name` 归属。

## 改动

- **`e2e/e2e.yaml`**：对齐 20 个套件条目的 `name` 与各自 yaml 文件 `name:` 一致（集中改一个文件）。这是 0.14.1 下让 case 真正执行的必要条件，直至上游按 id 归属。
- **`.dev/scripts/e2e-gate.sh`**：
  - 成功判定从 `grep '"passed"'` 改为 `status=="passed" && totals.total>0 && totals.failed==0`——`total>0` 把"空跑假绿"变红灯，与版本无关的兜底。
  - 在 `argus-setup` 之前补 `argus-build`（0.14.1 五步生命周期）。
- **`.dev/scripts/e2e-run.sh`**：从有 regression 的 `argusai` CLI 改写为 MCP 路径（mcp2cli → argusai-mcp），复用五步生命周期与硬化判定，行为与 e2e-gate.sh 对齐。
- **`e2e/tests/12-hooks.yaml` / `13-apply-patch.yaml` / `15-search-files.yaml`**：setup 里 `RECURSIVE_HOME=... recursive run` 之前补 `unset RECURSIVE_SESSIONS_DIR`。这是 CLAUDE.md 记载的既有陷阱——容器级 `RECURSIVE_SESSIONS_DIR=/workspace/sessions` 会把 session 写到 override 路径，`find /tmp/rh-*` 扑空，`recursive-session` 断言失败。此前被假绿灯掩盖，0.14.1 真跑后才暴露。

## 验证

- `argusai-mcp` 全局升到 0.14.1；`argusai-cli` 0.12.3。
- 对齐 name 后：smoke 3 case 真跑全过；claude-json-stream 12 case 真跑全过。
- 12 套件回放回归：`total=45, passed=44, failed=1`。修 3 个 unset bug 后，11/12 套件全绿。
- 唯一剩余：`compaction`（HTTP 套件，setup 用 `nohup ... &` 起后台服务跨 exec 步骤，疑似进程不存活）——其 name 之前也不匹配（一直假绿），属既有隐藏问题，非 0.14.1 回归，不在 smoke gate 内，留作后续。
- `.dev/scripts/e2e-gate.sh` 端到端：`GATE_EXIT=0`，`smoke PASSED ✓`，无 `INVALID_STATE`。
- `.dev/scripts/e2e-run.sh claude-json-stream`：`passed=12 failed=0`，退出 0。

## Notes

- **不变量**：在 argusai 按 `id` 归属（issue #8）修复前，新增/修改 E2E 套件时，`e2e.yaml` 条目 `name` 必须与套件 yaml 文件 `name:` 逐字一致，否则该套件静默不跑 case。`total>0` 硬化判定会在门上抓住这种漂移（红灯），但单套件手动跑时仍需自查。
- `argusai run` CLI（0.12.3）已不可靠（setup 不执行），E2E 一律走 MCP 路径（e2e-gate.sh / e2e-run.sh）。
- 未动 `e2e/plugins` 的 `argusai-core: file:...` 本地依赖；插件在 0.14.1 下 `recursive-session` 断言可用（smoke 验证通过）。
- 未提交代码；用户未要求 commit。
