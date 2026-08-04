# Handoff: Host E2E 适配工作

> 创建：2026-08-04 | 作者：ZCode session
> 主题：argusai HostRuntime 已实现发版，recursive 侧适配进行中

## 一句话状态

argusai HostRuntime 已发版 0.15.3（含 host cwd 修复）。recursive 侧 host 模式 e2e **37/41 suite 通过**（2026-08-04，`e2e-host-batch.sh` 全量跑）。剩 2 个失败**全是本机端口冲突**（9093 被 Java app 占、9097 被隐藏 listener 占），换机器/杀进程即过，suite 本身没问题。本次会话还顺手修了一批 Docker 下也潜伏的 bug（HTTP 认证、过时断言、schema 不匹配、jq 点号 key、rate-limit 测错端点、SSE curl 引号）。

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

### 0. ✅ 全量 host e2e 已跑通 36/41（2026-08-04 第二轮会话）

`bash .dev/scripts/e2e-host-batch.sh` 全量跑 41 个 suite：**PASS 36 / FAIL 3 / SKIP 2**（skip 的是需真 key 的 live/deferred）。
本次会话修了以下问题（都是 host 模式暴露、但多数 Docker 下也潜伏的）：

| 修复 | 影响范围 | 类型 |
|------|---------|------|
| `find -printf` → `-exec dirname {}` | 21 个 suite | BSD/GNU 兼容（host-only） |
| `e2e-run-host.sh` 注册全部 41 suite（之前只 smoke）+ `data` 信封解包 | runner | host runner |
| `E2E_HOST_REPLACEMENTS` 映射 `/sdk/*` → repo | typescript-sdk | host runner（python-sdk 走 pip 已通） |
| `RECURSIVE_API_KEY`/`API_BASE` 全局 export | acp-init | host runner（之前只 export 了 provider type） |
| HTTP server 起 `RECURSIVE_HTTP_AUTH_KEYS` + curl 带 `X-API-Key` | 08/08b/18/19/20/21/22 共 7 个 suite | **SEC-003 在 release binary 下的潜伏 bug**（Docker 也挂） |
| `assert:` → `cases:` schema 迁移 + jq 点号 key (`fs.readTextFile`) 改 `[$k]` | acp-init | **schema 不匹配 + jq bug**（Docker 也挂） |
| `escapes workspace` → `escapes sandbox roots` | bash-tool, sandbox-security | **过时断言**（Docker 也挂） |
| http-rate-limit 改测受保护端点（`/health` 是 rate-limit-exempt） | 08b | **测试测错端点**（Docker 也挂） |
| count_lines 断言改成「serve 可见」 | utility-tools | **过时断言**（registry 重构后 count_lines 已共享，Docker 也挂） |
| session-rewind slug 断言改匹配稳定尾巴 `test-rewind` | 36 | host-only（workspace 是临时目录，slug 带前缀） |
| **HostRuntime `execInContainer` 加 `cwd: workspaceDir`** | argusai 仓 runtime.ts | host-only（镜像 Docker 的 WORKDIR；否则 agent 默认 workspace=cwd 落到 daemon 目录） |

### 1. 剩余 2 个失败（已知，纯环境）

| suite | 原因 | 性质 |
|-------|------|------|
| `http-interrupt` (port 9093) | 本机一个 Java app 占着 9093 | **环境**（端口冲突，换机器/杀进程即过） |
| `http-auth` (port 9097) | 本机一个隐藏 listener 占着 9097（lsof 看不到 PID，netstat 见 LISTEN，回 404+CORS 头，非 recursive） | **环境**（端口冲突） |

> 两个端口冲突的验证：在空闲端口上手动复现过，`/interrupt`→200、`/health`→200 都正常，suite 本身没问题。
> （之前的第 3 个失败 http-api SSE 已解决——是我做 auth-header 注入时把 `-H` 塞进了已加引号的 URL token，curl 当成无 URL → SSE 捕获为空。已把 header 移到引号外，http-api 现在 21/21。）

### 2. argusai 0.15.3 已发版

`packages/core/src/runtime.ts` 的 HostRuntime `cwd` 修复（镜像 Docker WORKDIR，让 host 进程在 workspaceDir 下跑）+ 对不存在目录的 guard，已随 **argusai-mcp@0.15.3** 发到 npm（CI 跑 1m31s 全绿）。本地已 `npm install -g argusai-mcp@0.15.3`（不再是 npm link）。

### 3. 工作树清理

recursive 工作树有一批未提交文件（之前 self-improve cycle 遗留，**非本次会话产物**）：
```
 M docs/review/REVIEW_STATUS.md
?? .dev/goals/385-invariant-size-headroom.md
?? .dev/goals/386-default-launch-tui.md
?? .dev/goals/387-acp-tools-defer-or-gate.md
?? .dev/goals/388-doc-drift-agents-arch-overview.md
?? .dev/goals/389-tui-prompt-history-persist.md
?? .dev/goals/390-cost-unknown-model-null.md
?? .dev/goals/391-packaging-trigger-criteria.md
?? docs/review/architecture-review-2026-08-04.md
```
本次会话只动了 e2e/tests/*.yaml、.dev/scripts/、.dev/handoff-host-e2e.md，没碰上面这些。

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
