# E2E 录制-重跑：为 recursive 生产 aimock fixture

> 这份文档记录 recursive 的 e2e 测试如何用「真模型录制 → mock 回归」生产 fixture。
> 它是 recursive 项目内部的手段，不是跨仓协议。argusai 只负责跑容器和测试，
> 不关心 fixture 怎么来；录制完全由 recursive 的 e2e 插件 + aimock 组装。

## 为什么需要

recursive 是跑 LLM agent 的运行时。单测要么 mock 掉 LLM（只测了胶水代码），
要么打真模型（慢、贵、不可重复）。录制-重跑是中间道路：

1. **录制（一次性，手动）**：用真模型跑通一个场景，aimock 把每次 LLM 请求-响应
   存成 fixture。
2. **重跑（每次 CI，自动）**：aimock 按 fixture 回放，agent 跑完整流程但 LLM 响应
   确定。这样 agent 的端到端行为（调了哪些工具、产出什么、transcript 是否正确）
   变成可回归的。

## ✅ 统一路径：录制与回放都走 MCP（e2e-run.sh）

recursive 的 E2E **只有一条路径**：`.dev/scripts/e2e-run.sh <suite-id>`（MCP 路径，
mcp2cli → argusai-mcp）。录制与回放是同一个入口，区别只是一个环境变量：

```bash
# 回放（每次 CI，默认，不需要 key）
.dev/scripts/e2e-run.sh <suite-id>

# 录制（一次性，手动，需要真 key）
E2E_RECORD=1 DEEPSEEK_API_KEY=sk-... .dev/scripts/e2e-run.sh <suite-id>
```

**aimock 由 e2e 插件全权拥有**（`e2e/plugins/src/index.ts`），不在 e2e.yaml 的
`mocks` 里声明：

- 插件在 argus-init 阶段启动 aimock，按 `E2E_RECORD` 选择模式：
  - 回放：`-f /fixtures`（纯 fixture 服务）
  - 录制：`--record --provider-openai <真模型> -e OPENAI_API_KEY=…`（代理真模型并记录）
- 插件自己创建/复用 Docker 网络（`argusai-<WORKTREE_ID>-network`，与 argus-setup
  给 recursive-e2e 容器用的网络一致），所以 record 和 replay 在 MCP 路径下行为完全相同。
- 插件对残留 aimock **做模式自愈**：已有 aimock 的网络或模式（CMD 是否含 `--record`）
  与本次请求不符时自动 `rm -f` 重建，杜绝「残留 replay 容器吞掉 record 的假绿」。
- 不要把它放回 `e2e.yaml` 的 `mocks:`：argus-setup 会无条件以回放参数重启同名
  容器，且无法注入 `OPENAI_API_KEY`，录制会静默失效（这是历史教训，2026-08 统一时
  移除）。

**为什么不用 CLI（`argusai -c e2e.yaml run`）：** argusai-cli 0.12.3 有 regression，
`argusai run` 的 yaml-engine 在 setup 步骤把 exec 错误吞成 ✓（容器没起时「假绿」），
会得到 `Setup ✓ / case ✗ 文件不存在` 的误导结果。所有 E2E 一律走 MCP 路径。

## 什么时候需要录制（判定标准）

录制有成本（要跑真模型、要 promote、要维护 fixture）。不是每个改动都值得录。
**核心判定：这个改动的正确性是否依赖 LLM 的行为决策？**

### 该录制（满足任一即触发）

- **新增 / 改变 agent 的行为路径**：新工具、新 agent 工具参数（如 `background`）、
  改 coordinator 教学文案、改 worker 生命周期、改 send_message / task_* 语义。
  这类改动单测 mock 掉了 LLM，等于没测；只有真模型跑一遍才能证明「LLM 确实会按
  新设计行动」。
- **修了一个「LLM 没按预期行动」的 bug**：bug 本身就是行为问题，修完要锁住
  正确行为，防回归。
- **引入新的多 agent 协作模式**：委派、续聊、并发、嵌套——这些是 LLM 编排行为，
  最容易出 subtle bug，最该有回归 fixture。

### 不需要录制

- 纯代码逻辑（解析、序列化、配置加载、数学计算）——单测够。
- 工具的内部实现重写但 LLM 调用它的方式不变（如 Write 工具从 syscall 换成
  async fs，agent 调 Write 的行为没变）——已有的 e2e fixture 仍适用，不需要重录。
- 文档、注释、重构不改变行为。

### 由谁来做

**写这个功能的开发者**，在功能开发完成、准备提 PR 时录制。录制是「开发完成的
验收动作」之一——不是 QA 的事，不是事后补的。理由：只有开发者最清楚这个功能
「该呈现什么行为」，也只有开发者本地有真模型 key 能跑录制。PR 里应附上录制的
fixture（`e2e/fixtures/<suite>.json`），reviewer 在 replay 模式下验证。

### 简易决策流程

```
改动是否涉及 LLM 决策 / agent 行为路径？
├─ 否 → 单测即可，跳过录制
└─ 是 → 有无已有 e2e fixture 覆盖这个行为？
        ├─ 有 → 改 fixture 适配（通常手改即可），replay 验证
        └─ 无 → 录制新 fixture（见下方三步工作流）
```

## 三步工作流

> 这是录制的**手动**三步（写套件 → 录制 → promote）。如果想「录制 + agent 自验收 +
> promote + 回放」一条命令搞定，跳到 [验收即录制](#验收即录制accept-on-verify) 章节，
> 用 `.dev/scripts/e2e-gate.sh --accept <suite-id>`。下面的手动流程适合需要逐步控制
> 或排查问题时用。

### 1. 写套件骨架

在 `e2e/tests/<NN>-<name>.yaml` 写 argusai 套件：setup 跑 agent、cases 断言
transcript/文件产出。fixture 路径约定 `e2e/fixtures/<suite-id>.json`。

### 2. 录制（真模型，MCP 路径）

```bash
export DEEPSEEK_API_KEY=sk-...
export DEEPSEEK_API_BASE=https://api.deepseek.com/v1   # 带 /v1，插件自动剥
E2E_RECORD=1 .dev/scripts/e2e-run.sh <suite-id>
```

e2e 插件以 `--record` 启动 aimock，代理到真模型，把每次未匹配的请求-响应录到
`e2e/fixtures/recorded/`。

**三个必须知道的坑（零背景新人实测撞墙总结）：**

1. **残留 aimock 不会让 record 静默假绿——插件会自动重建**。每次 run 结束
   e2e-run.sh 都会删掉 aimock（argus-clean 不管插件容器，脚本显式删）；即便有
   残留（比如上次运行中断），插件 setup 也会比对现有 aimock 的**模式**（CMD 是否
   含 `--record`）与本次请求，不匹配就 `docker rm -f` 重建。所以「上次跑过回放、
   这次直接 E2E_RECORD=1」是安全的，不要手动 `docker rm -f aimock` 前置清理。
2. **record 模式对已匹配 fixture 的请求仍走回放**。`--record` 只把**未匹配**的
   请求代理到真模型并记录；smoke 这类已有完整 fixture 的套件跑 record 不会打真
   模型、`recorded/` 也是空的——这不是 bug。想验证「代理 → 真模型 → 401」路径
   或录制新行为，必须用一个**没有对应 fixture** 的新套件/新 prompt。
3. **别用 `docker exec recursive-e2e` 做运行中检查**。recursive-e2e 容器只在
   setup/run 阶段短暂存在（argus-clean 后即销毁），想确认二进制/工具名请在
   run 过程中做（或直接信 e2e.yaml 的容器内配置）。

> 如果报「容器已存在 / port 占用」，先 `docker rm -f recursive-e2e aimock` 再重试。
> 录制跑完必须人工核对：`cat e2e/fixtures/recorded/*.json`，确认录制到的请求序列
> 与设计的行为路径一致（工具调用顺序、turn 数），再 promote。

### 3. promote + 回归

```bash
cd e2e
./scripts/promote.sh <suite-id>          # 合并 recorded/*.json → fixtures/<suite-id>.json
cd .. && .dev/scripts/e2e-run.sh <suite-id>   # 纯 mock 回放验证
git add e2e/fixtures/<suite-id>.json      # 提交 fixture，CI 即可回归
```

回放与录制同一入口（`.dev/scripts/e2e-run.sh`），不需要 API key，CI 用同一条路。


## 已知陷阱（都踩过）

### 1. aimock 多轮匹配：`userMessage` 只匹配最后一条 user 消息

最常见的 fixture bug。`match.userMessage` 匹配请求里**最新**的 `role:user` 消息，
不是原始 goal。多 turn / 续聊场景每条最新 user 消息不同的请求，都要单独一条
fixture。详见 `e2e/fixtures/README.md` + aimock 官方文档
`https://aimock.copilotkit.dev/multi-turn`。

### 2. 用 `turnIndex` + `hasToolResult` 区分多轮

- `turnIndex` = 请求里已有的 assistant 消息数（从 0 开始，**无状态推导**）。
- `hasToolResult` = 历史里是否已有 tool result。
- 隔离 transcript 的子 agent（如 `agent` 工具 spawn 的 worker），turnIndex 从 0
  重新计数——用 `userMessage`（goal 子串 vs worker prompt 子串）区分父子请求。

### 3. 动态 ID 不可预测

agent 运行时生成的 ID（如 `task-<uuid>`）写不进 fixture。解法：用固定标识寻址。
如 `send_message` 支持 `worker_id`（manifest 里固定的 key），而非动态 `task_id`。

### 4. 录制时 aimock 转发请求里的 key 和 model（非容器环境变量）

⚠️ **本节 2026-08 实测订正**——旧描述（「aimock 用容器 `OPENAI_API_KEY` 代理」）
对当前 aimock 版本是错的。

aimock `--record` 代理到上游时，转发的是**请求里的 Authorization key 和 body 里的
model**，不是 aimock 容器的 `OPENAI_API_KEY`。实测：用真 key 当 Authorization
直 curl aimock → 代理成功；用 `mock-key` → 上游 401。同理，请求 body 里 `model:
mock-chat` 会被原样转发给上游 → DeepSeek 回 400（不认识的 model 名）。

**因此录制时被测 agent 必须发真 key + 真 model**，而不是 `mock-key` / `mock-chat`。
回放时 aimock 不校验 key/model，`mock-key` / `mock-chat` 照常工作。

实现：套件命令行的 `--api-key` / `-m` 已统一改成读环境变量
`"$RECURSIVE_API_KEY"` / `"$RECURSIVE_MODEL"`（不再写死 mock-key/mock-chat）；
`e2e.yaml` 把这两个变量声明为模板 `{{env.E2E_RECORD_API_KEY}}` /
`{{env.E2E_RECORD_MODEL}}`。`e2e-run.sh` 按 `E2E_RECORD` 自动设值：录制时填真 key +
真 model（`DEEPSEEK_MODEL`，默认 deepseek-chat），回放时填 mock-key + mock-chat。

### 5. `--provider-openai` 的 URL 不要带 `/v1`

aimock 自己追加 `/v1/chat/completions`。base 带 `/v1` 会变成 `/v1/v1/` → 401。
插件自动 strip 末尾 `/v1`（见 `plugins/src/index.ts`）。

### 6. 录制模式 `-f` 顺序

aimock `--record` 把新 fixture 写到**第一个** `-f` 路径。写成
`-f /fixtures/recorded -f /fixtures`：录制目录在前（可写），已有 fixture 在后。

### 7. Docker 网络

aimock 容器要和 recursive-e2e 容器在同一 Docker 网络。worktree 隔离环境要动态
探测网络名。Docker Desktop 可能注入 `http_proxy` 导致 502。见 `plugins/src/index.ts`
的网络处理。

## 录制 vs 手写 fixture

- **简单串行场景**（如 `41-multi-agent` 前台委派）：手写 fixture 直接、稳定。
- **复杂并发场景**（如 `42-multi-agent-continue` background 续聊）：coordinator
  和 worker 的 LLM 请求交错，手写 fixture 要精确推演每个请求的
  (userMessage, turnIndex, hasToolResult) 组合，易出错。优先录制；或基于真模型
  实跑的 transcript 手工构造（42 就是这样做的）。

## 验收即录制（accept-on-verify）

录制不是「额外动作」，而是**验收的副产品**。判据：**改动影响端到端 agent 产出 →
验收本来就要跑真模型 → 把那次真实跑捕获成 fixture → 由开发 agent 自验符合预期
才 promote → CI 无 key 回放。**「人」从验证环节退出，换成开发该功能的 agent 自己
判（它最懂本次改动的意图）。

### 工作流（一条命令）

```bash
# 开发完成、提 PR 前：
export DEEPSEEK_API_KEY=sk-...
.dev/scripts/e2e-gate.sh --accept <suite-id>
# 或直接：e2e/scripts/accept-fixture.sh <suite-id>
```

`accept-fixture.sh` 自动跑 4 步：

1. **录制**：`E2E_RECORD=1` 跑 argusai lifecycle，aimock 代理真模型，落
   `e2e/fixtures/recorded/`。
2. **验收**：录制跑完、容器销毁前 docker cp 出 session transcript，spawn 一个只读
   recursive agent 当 judge，对照 `e2e/expectations/<suite-id>.md` 判
   `{completed, score}`。要求 `completed=true && score>=阈值`（默认 4）。
3. **promote**：验收通过才调 `promote.sh` 合并 recorded → `fixtures/<suite-id>.json`。
   不通过则保留 `recorded/` 待人介入，**不污染回归 fixture**。
4. **回放**：无 key 重跑 `e2e-run.sh <suite-id>`，确认新 fixture 回归绿。

### expectation 文件

`e2e/expectations/<suite-id>.md` 是套件的「预期行为」契约（goal / 预期行为 /
anti-patterns / 验收阈值）。judge 据此判 PASS/FAIL。**没配 expectation 的套件退回
判完整性**（工具调用是否合理、产出是否符合 goal）。首个范例：
`e2e/expectations/loop-schedule.md`。

### 该捕获 vs 该手写（判据）

录制的前提是「环路里有真模型」。三类：

- **该捕获**：agent 端到端跑的套件——验收跑真模型时顺手录。录到的真实行为比手写
  桩更可信（手写是人脑假设的路径，真模型未必走；见下方 loop-schedule 实测）。
- **必须手写**：① 环路里没有 LLM 的 case（直接走 MCP 调工具，如 `24-bash-tool`
  的 C/D、`33-utility-tools` 的 B/C）——无真模型可捕获；② 需要确定性边界刺激的
  套件（如 `22-compaction` 要精确撞 `RECURSIVE_COMPACT_THRESHOLD`）——真模型每次
  给不同文本可能不触发。
- **默认手写**：测运行时/协议而非 agent 行为的（cost/export/sdk），LLM 只是触发器。

### ⚠️ 录制揭示的真实行为可能与手写 fixture 不符

手写 fixture 是「人脑假设 agent 会这么走」，真模型未必。实测 loop-schedule：
套件假设 agent「turn1 先 schedule_wakeup、turn2 再 write_file」（4-turn 结构），
但真模型看到明确的「Write file」任务时，**turn1 直接 write_file 完成**，根本没调
schedule_wakeup。这正是录制的价值——暴露手写 fixture 的假设偏差。遇到这种分歧时：
若真模型行为合理 → 改套件 goal/case 适配真实行为后重录；若真模型行为是 bug →
保留 expectation 作为期望，修 agent 后重录锁住。

### 触发清单（什么改动触发对应套件的录制）

当以下任一发生时，对应套件进录制流程（`e2e-gate.sh --accept <suite-id>`）：

| 触发条件 | 套件 |
|----------|------|
| 改 `schedule_wakeup` 决策点 / loop 演进逻辑 | loop-schedule |
| 改 TodoWrite todos schema 或教学文案 | todo |
| 改 facts remember/recall 参数语义 | facts |
| 改 skill 自动触发/匹配逻辑 | skill-lazy-load |
| 给 Bash/Glob/Write 加新语义参数 | glob / bash-tool |
| 把 41 从前台委派改成并发委派 | multi-agent |
| 改 coordinator/worker 教学文案、worker 生命周期、send_message/task_* 语义 | multi-agent-continue |
| 任何「修复 LLM 没按预期行动」的 bug | 对应行为套件 |

## 相关文件

| 文件 | 说明 |
|------|------|
| `e2e/plugins/src/index.ts` | 录制插件（setup 启动 aimock） |
| `e2e/scripts/accept-fixture.sh` | **验收即录制**：录制→judge→promote→回放 4 步封装 |
| `e2e/scripts/promote.sh` | 录制产物合并脚本 |
| `e2e/expectations/<suite-id>.md` | 套件预期行为契约（judge 判据） |
| `e2e/fixtures/README.md` | aimock fixture 格式与多轮陷阱 |
| `e2e/.env.example` | `E2E_RECORD` 等录制变量 |
| `e2e/tests/41-multi-agent.yaml` | 前台委派回归套件（手写 fixture） |
| `e2e/tests/42-multi-agent-continue.yaml` | background 续聊回归套件（基于真模型录制构造） |
