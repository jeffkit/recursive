# Handoff: Host E2E 适配工作

> 创建：2026-08-04 | 作者：ZCode session
> 主题：argusai HostRuntime 已实现发版，recursive 侧适配进行中

## 一句话状态

argusai HostRuntime 已实现并通过 npm 发版（0.15.0-0.15.2）。recursive 侧的 host 模式 e2e smoke 验证了 **case 1 (write_file) 完全通过**——证明从 config 到 recursive binary 到 file 断言的完整链路 work。剩余工作是 smoke case 2/3 的 session 断言路径适配（机械工作，非架构问题）。

## 两个仓的最新 commit

**recursive** (`/Users/kongjie/projects/infra4agent/recursive`)：
```
3984dd3 fix(e2e-host): set RECURSIVE_PROVIDER_TYPE=openai for aimock compat
73739b5 feat(e2e): host-mode runner + aimock plugin host branch
884e710 self-improve: Goal 384 — Todo panel keeps the in-progress task visible
b14f497 self-improve: Goal 383 — TUI graceful cancel via CancellationToken
3f8d302 self-improve: Goal 382 — Persist partial content on stream interrupt
```

**argusai** (`/Users/kongjie/projects/infra4agent/argusai`)：
```
b1d9d0a chore: apply changeset v0.15.2 (host aimock mapping)
7112661 chore: apply changeset v0.15.1 (host workspaceDir)
93623f3 chore: apply changeset v0.15.0 (HostRuntime)
```
npm 已发版：`argusai-mcp@0.15.2`（含 argusai-core/core-storage/dashboard 同版本）。

## 已完成的工作（完整清单）

### A. 三个 self-improve goal（都已 committed 到 recursive main）

| Goal | commit | 内容 |
|------|--------|------|
| 382 | `3f8d302` | LLM 流中断时持久化部分内容（provider 流式层返回部分 Completion + run_core 路由到 Cancelled） |
| 383 | `b14f497` | TUI Ctrl+C 改用 CancellationToken 优雅取消（替代 handle.abort 硬杀）+ 删 truncate_transcript 磁盘/内存不一致 |
| 384 | `884e710` | TUI todo 面板自动跟踪 InProgress 任务（去掉 .take(6) 硬截断，改成窗口式渲染） |

三个 goal 的 headline tests 都验证通过。

### B. 开发流程优化（都已 committed 到 recursive main）

| commit | 内容 |
|--------|------|
| `06aa17a` | mutants gate 改函数级 --in-diff（296→26 变异点，40min→5min） |
| `1b6af8a` | 本地 smoke runner e2e-local.sh（旁路 argusai，10-40min→4s） |
| `d434994` | 清理 goal-383 误提交的临时日志 |

### C. argusai HostRuntime（已发版 0.15.0-0.15.2）

这是本次会话的主要工作。给 argusai 加了 `HostRuntime`——让 e2e 测试不依赖 Docker 容器，直接在 host 上跑。

**argusai 改的文件**（3 个 commit，已在 main + npm）：
- `packages/core/src/runtime.ts` — HostRuntime 类 + execInContainer 返回 RuntimeExecResult + workspaceDir/aimock 路径映射
- `packages/core/src/yaml-engine.ts` — executeExecStep/FileStep/ProcessStep/PortStep 接线 runtime
- `packages/core/src/docker-engine.ts` — execInContainer 返回 {stdout, exitCode} 不抛异常
- `packages/core/src/types.ts` — E2EConfig.runtime 字段 + ServiceConfig.build 可选
- `packages/core/src/config-loader.ts` — zod schema 加 runtime 字段（关键修复！zod 默认 strip unknown keys）
- `packages/mcp/src/session.ts` — session 持有 ContainerRuntime 实例
- `packages/mcp/src/tools/run.ts` — 注入 session.runtime 到 executeYAMLSuite
- `packages/mcp/src/tools/setup.ts` — host 模式跳过容器启动
- `packages/mcp/src/tools/build.ts`/`init.ts` — build 可选守卫
- `packages/mcp/src/index.ts` — isMainModule symlink-aware（修复 npm link 不启动的问题）
- `packages/core/src/index.ts` — 导出 HostRuntime
- `packages/dashboard/server/routes/docker.ts` — 可选链修复
- 测试：runtime.test.ts（36 tests）+ yaml-engine.test.ts 向后兼容

**1028 个 argusai 测试全绿，0 回归。**

### D. recursive 侧适配（进行中）

**已完成**（`73739b5` + `3984dd3`）：
- `e2e/plugins/src/index.ts` — aimock plugin 的 host 分支（`E2E_HOST_MODE=1` 时用 `docker run -p` 替代 `--network`）
- `.dev/scripts/e2e-run-host.sh` — host 模式 e2e runner（动态生成 host e2e.yaml + 设环境变量 + 调 mcp2cli/argusai-mcp）

## 下一个会话需要继续做的事

### 1.（优先）让 smoke 套件 3/3 全过

**当前状态**：
```
smoke 套件（3 个 case）：
  ✅ case 1: write_file produced smoke.txt    PASSED
  ❌ case 2: session recorded write_file       FAILED（"No session directory found under /tmp/sessions-smoke-01"）
  ⏭️ case 3: session recorded read_file         SKIPPED（sequential fail-fast）
```

**case 1 通过证明完整链路 work**（config-loader → HostRuntime → execInContainer → recursive binary → aimock → write_file → file 断言）。

**case 2 失败原因**：smoke YAML (`e2e/tests/00-smoke.yaml`) 的 setup step 里有这段逻辑：
```bash
SESSION=$(find /tmp/rh-smoke-01 -name '.meta.json' -printf '%h\n' 2>/dev/null | head -1)
if [ -n "$SESSION" ]; then
  mkdir -p /tmp/sessions-smoke-01
  cp -r "$SESSION/." /tmp/sessions-smoke-01/
fi
```
这段把 session cp 到 `/tmp/sessions-smoke-01`。在 host 模式下：
- recursive 用 `RECURSIVE_HOME=/tmp/rh-smoke-01` 跑，session 落在 `/tmp/rh-smoke-01/workspaces/<hash>/sessions/...`
- `find /tmp/rh-smoke-01 -name .meta.json` 应该能找到——但 macOS 的 `find` 不支持 `-printf`（GNU 专有）！这是和 e2e-local.sh 一样的 BSD find 兼容性问题
- 即使 find 成功，`/tmp/sessions-smoke-01` 的 cp 也需要执行成功

**修复方向**：
- 最小方案：在 e2e-run-host.sh 里，跑完 setup 后手动 cp session 到断言期望的路径
- 或：改 recursive-session assertion plugin 的 input 路径（让它直接读 `RECURSIVE_HOME` 下的 session，不依赖 cp）

### 2. 扩展到全部 41 个 suite

smoke 全过后，其余 40 个 suite 的主要障碍是 `/workspace` 路径（已被 HostRuntime 的 workspaceDir 映射解决）和各种容器环境变量（如 `RECURSIVE_PROVIDER_TYPE`）。逐个 suite 跑 `e2e-run-host.sh <suite-id>`，修暴露的问题。

### 3. 工作树清理

recursive 工作树有未提交的文件（可能是之前 self-improve cycle 的遗留）：
```
 M docs/review/REVIEW_STATUS.md
?? .dev/goals/385-invariant-size-headroom.md
?? .dev/goals/386-default-launch-tui.md
?? .dev/goals/387-acp-tools-defer-or-gate.md
?? .dev/goals/388-doc-drift-agents-arch-overview.md
```
确认这些是否要保留/提交/删除。

## 关键技术知识（避免踩坑）

### HostRuntime 路径映射机制（argusai 0.15.2）

HostRuntime 的 `execInContainer(name, command)` 在执行命令前做三层映射：
1. `/workspace` → `E2E_WORKSPACE_DIR`（path token 替换，不碰 `/workspace-foo`）
2. `aimock:PORT` → `localhost:PORT`（Docker DNS → host port）
3. `E2E_HOST_REPLACEMENTS` env 的 `old=new` 对（通用自定义替换）

### RECURSIVE_PROVIDER_TYPE（关键坑）

smoke YAML 的 recursive 命令**没有 `--provider openai`**。Docker 模式靠 e2e.yaml 的 `container.environment.RECURSIVE_PROVIDER_TYPE=openai`。host 模式必须在 e2e-run-host.sh 里 `export RECURSIVE_PROVIDER_TYPE=openai`，否则 recursive 默认用 Anthropic provider 发 `/v1/messages`（而非 `/v1/chat/completions`）→ aimock 返回 404。

### mcp2cli 环境

mcp2cli 3.3.1（刚从 3.0.2 升级，修了 pydantic_core native 扩展问题）。它的 session daemon 是 detached subprocess，继承父进程 env。HostRuntime 的 execInContainer 用 `env: { ...process.env }` 继承 daemon 的环境变量。

验证 mcp2cli + argusai-mcp 链路：
```bash
MCP2CLI=$(command -v mcp2cli)
ARGUSAI_MCP_BIN="$(npm root -g)/argusai-mcp/dist/index.js"
$MCP2CLI --mcp-stdio "node $ARGUSAI_MCP_BIN" --session-start test
$MCP2CLI --session test tools  # 应列出 argus-init/build/setup/run 等
$MCP2CLI --session-stop test
```

### 跑 host 模式 smoke 的命令

```bash
cd /Users/kongjie/projects/infra4agent/recursive
# 确保 aimock 在 localhost:4010（plugin setup 会自动起，也可手动）
docker run -d --rm --name smoke-aimock -p 4010:4010 \
  -v "$(pwd)/e2e/fixtures:/fixtures:ro" \
  ghcr.io/copilotkit/aimock -f /fixtures -h 0.0.0.0
# 确保 recursive binary 已编译
cargo build --release -p recursive-cli
# 跑 host smoke
bash .dev/scripts/e2e-run-host.sh smoke
```

### argusai 仓的 link 方式（本地开发）

```bash
cd /Users/kongjie/projects/infra4agent/argusai/packages/mcp
pnpm -r build        # 确保 dist 最新
npm link             # 全局 link
# 验证
node -e "console.log(require('$(npm root -g)/argusai-mcp/package.json').version)"
```

恢复正式版：`npm install -g argusai-mcp@0.15.2`

## 文件索引

### recursive 侧（本次改动）
- `.dev/scripts/e2e-run-host.sh` — host 模式 e2e runner
- `.dev/scripts/e2e-local.sh` — 旁路 argusai 的秒级 smoke runner（之前的成果）
- `.dev/scripts/e2e-gate.sh` — 加了本地快路径（e2e-local.sh 先跑，过了秒级绿灯）
- `.dev/scripts/agent-mutants.sh` — 改成函数级 --in-diff
- `.dev/scripts/tui-mutants.sh` — 同上（镜像改造）
- `e2e/plugins/src/index.ts` — aimock plugin 的 host 分支
- `e2e/tests/00-smoke.yaml` — smoke 套件定义（未改，通过 HostRuntime 适配）
- `e2e/fixtures/00-smoke.json` — smoke replay fixture

### argusai 侧（已发版）
- `packages/core/src/runtime.ts` — HostRuntime + DockerRuntime + KubernetesRuntime + createRuntime
- `packages/core/src/yaml-engine.ts` — 4 个 execute 函数的 runtime 接线
- `packages/core/src/config-loader.ts` — zod schema runtime 字段
- `packages/mcp/src/index.ts` — isMainModule symlink 修复
- `packages/core/tests/unit/runtime.test.ts` — 36 个测试（含 HostRuntime 9 个）

### self-improve skill 参考
- `/Users/kongjie/projects/infra4agent/recursive/.zcode/skills/self-improve-supervise/SKILL.md` — supervisor 操作手册
- `/Users/kongjie/projects/infra4agent/recursive/.dev/goals/382-*.md` / `383-*.md` / `384-*.md` — 已完成的 goal

## 整体工作脉络（给下一个会话的上下文）

用户最初报了两个 bug：
1. TUI resume 只显示 9 条消息（compact 后的历史不可见）→ 已修（session full-history display）
2. 网络中断后 agent 忘记上一 turn → 已修（Goal 382 流中断持久化 + Goal 383 TUI 优雅取消）

然后扩展到开发流程优化：
3. mutants gate 太慢 → 改函数级 --in-diff（40min→5min）
4. e2e gate 太慢 → 本地 smoke runner（旁路 argusai，4s）

然后用户提出要支持全部 suite 在本地跑（不通过 Docker）：
5. argusai HostRuntime 实现 + 发版（0.15.0-0.15.2）
6. recursive 侧适配（进行中——case 1 通过，case 2/3 待适配）
