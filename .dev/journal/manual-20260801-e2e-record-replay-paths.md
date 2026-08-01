# Manual edit: e2e-record-replay-paths

**Date**: 2026-08-01
**Goal**: 把 recursive 的 E2E 录制-重跑**统一为 MCP 单路径**（`.dev/scripts/e2e-run.sh`）：
录制与回放同一入口，区别只是 `E2E_RECORD=1`。废弃 CLI 路径（argusai-cli 0.12.3
有 setup-exec regression）。

## 背景 / 验证过程

### 第一轮：文档对齐（双路径）

实测复现了两个问题：

1. **RECORD_REPLAY.md / .env.example 教的 `E2E_RECORD=1 argusai -c e2e.yaml run -s <suite>` 不可用**。
   不带 `argusai setup` 直接 run 时，CLI 的 yaml-engine 在 setup 步骤把 exec 错误吞成 ✓
   （`executeStep` 返回错误数组不 throw，setup 只 catch throw），得到
   `Setup ✓ / case ✗ 文件不存在` 的误导结果。先 `argusai setup --skip-build` 再 run 才可靠。

2. **MCP 路径（e2e-run.sh）无法录制**。MCP 生命周期的 `argus-setup` 会无条件按
   e2e.yaml `mocks.aimock` 重启 aimock 容器（`rm -f` 同名后以纯回放参数 `-f /fixtures`
   重启），且其 image-mock 启动**无法注入环境变量**（`OPENAI_API_KEY` 传不进容器）。
   即使 e2e 插件先以 `--record` 模式启动 aimock，也会被 argus-setup 覆盖，录制静默失效。

### 第二轮：统一到 MCP（最终方案）

用户要求「统一用 MCP」。调研 argusai-mcp 0.14.2 内部实现后确认可行路径：
**让 e2e 插件全权拥有 aimock，e2e.yaml 不再声明 `mocks.aimock`**。

- 插件的网络名推导对齐 argusai `deriveNetworkName`（`argusai-<WORKTREE_ID>-network` /
  project slug），网络不存在时插件自己创建（argus-setup 的 `ensureNetwork` 对已存在
  网络静默吞错，已验证 setup.js:119-126）；`OrphanCleaner` 只清 `argusai.managed` label
  的资源，插件容器/网络无 label，不会被误清。
- 实测确认：e2e-run.sh 回放能过，恰恰因为 argus-setup 把 aimock 重启到了
  `argusai-{{env.WORKTREE_ID}}-network`（WORKTREE_ID unset 时变量未替换产生字面量网络名）。
  统一后 e2e-run.sh **显式设置 WORKTREE_ID**（与 e2e-gate.sh 一致），网络名干净确定；
  容器名不受影响（recursive-e2e/aimock 来自 e2e.yaml/插件，与 WORKTREE_ID 无关）。

## 改动

- **`e2e/plugins/src/index.ts`**：插件全权接管 aimock——
  - 网络：候选 `argusai-<WORKTREE_ID>-network` / `argusai-recursive-agent-network` /
    `e2e-network`，不存在则创建（并发已存在则复验后继续）；
  - record/replay 双模式启动逻辑保留（record 需 `E2E_RECORD=1` + `DEEPSEEK_API_KEY`）。
- **`e2e/e2e.yaml`**：移除 `mocks.aimock`（argus-setup 不再接管；加注释说明由插件拥有）。
- **`.dev/scripts/e2e-run.sh`**：移除 `unset WORKTREE_ID` 与 E2E_RECORD 护栏；改为
  `export WORKTREE_ID="wt-<sha>"`；头注释改为单路径说明。`E2E_RECORD`/key 经 env
  自然透传（mcp2cli 继承环境）。
- **`e2e/RECORD_REPLAY.md`**：双路径章节改为「统一路径」；录制/回放命令对齐 e2e-run.sh。
- **`e2e/.env.example`**：对齐单路径用法。
- **`AGENTS.md` / `.dev/AGENTS.md`**：E2E rules 改单路径；`.dev/AGENTS.md` 的
  E2E gate 描述修正为实际机制（`e2e-gate.sh` = MCP 路径）。
- **`e2e/scripts/promote.sh`**：Next steps 提示改为 `.dev/scripts/e2e-run.sh`。

## 验证（全部通过）

- 回放（插件自建网络）：`smoke` 3/3、`multi-agent` 2/2、`multi-agent-continue` 3/3 ✓
- 录制路径：`E2E_RECORD=1 DEEPSEEK_API_KEY=fake-... e2e-run.sh smoke` →
  aimock CMD 含 `--record --provider-openai https://api.deepseek.com`（/v1 已剥）、
  env 含 OPENAI_API_KEY、网络 `argusai-wt-<sha>-network` ✓；
  fake key 下代理 401 → 预期失败（证明真在代理而非回放旧 fixture）✓
- 网络时序：插件 t=1s 起 aimock（init 阶段），argus-setup 随后起 recursive-e2e 同网络 ✓
- flow 硬门：`.dev/scripts/e2e-gate.sh` → `smoke PASSED ✓`（mocks:[] 不再接管）✓
- `e2e.yaml` YAML 语法校验 ✓

## 第三轮：零背景子 Agent 实测 + 修掉假绿根因（2026-08-01 下午）

用零背景 general-purpose 子 Agent（35 次工具调用、7 分钟）独立跑通全流程，验证文档
自洽性。它一次通过回放、确认 record 机制正常，但撞到 4 堵墙，其中 2 堵是**真 bug**：

1. **回放/录制跑完后 aimock 容器不清理**：MCP 路径的 `argus-clean` 只清理带
   `argusai.managed` label 的容器，插件拥有的 aimock 无 label；且 argusai-mcp
   的 clean 工具**从不调用插件 teardown**（grep 确认无 teardownPlugins 调用）。
   修复：e2e-run.sh / e2e-gate.sh 的 cleanup 显式 `docker rm -f aimock`。
2. **残留 replay aimock 让 record 静默假绿（最严重）**：插件只检查残留容器的
   **网络**，不检查**模式**；更糟的是网络检查的 docker inspect 模板
   `{{range $k, $v := ...}}` 在 execSync 里被 shell 把 `$k`/`$v` 展开成空 →
   `{{range , := ...}}` → docker 模板解析错误 exit 64 → 被外层 try/catch 静默
   吞掉 → 检查整段形同虚设。修复：模板改用 `{{json .NetworkSettings.Networks}}`
   （无 `$` 变量，shell 动不了），并增加模式比对：`wantRecord !== isRecord` 即
   `rm -f` 重建。双向自愈验证通过（残留 replay + record 请求 → 重建为 record；
   残留 record + replay 请求 → 重建为 replay）。
3. **record 模式对已匹配 fixture 的请求仍回放**：`--record` 只录未匹配请求，
   smoke 全命中 fixture 时 `recorded/` 为空、不打真模型——文档已补说明（不是 bug）。
4. **recursive-e2e 容器短命**：只在 setup/run 阶段存在，`docker exec` 事后检查
   会 No such container——文档已补提示。

修复后全链路回归：回放 smoke/multi-agent/multi-agent-continue 全绿、
干净 record 模式 aimock 带 `--record`+key、e2e-gate.sh PASSED、无 aimock 残留。

## Notes

- 环境 argusai-cli 0.12.3（有 regression，已弃用）、argusai-mcp 0.14.2（含 issue #8 按 id
  归属修复）——统一后唯一入口走 MCP。
- 插件产物 `e2e/plugins/dist/` 被 .gitignore 排除，由 e2e-gate.sh 构建；本仓只改 src。
- 未提交代码；用户未要求 commit。

