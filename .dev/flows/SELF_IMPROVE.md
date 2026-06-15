# self-improve 指引（给 AI）

本文件指导 AI 在 **recursive 仓**里跑一次自改（self-improve）循环。新的 self-improve
用 [flowx](https://github.com/jeffkit/flowx) flow 编排，**等价替代**老的 `.dev/scripts/self-improve.sh`
（49KB bash 自改循环），但**可审计、可观测、可断点续跑**。

> recursive 的 Rust kernel 一行都不改。recursive 二进制只是被 flow 调度的「执行器」。

---

## 这是什么

`.dev/flows/self-improve.flow.js` 把一次自改拆成可断点的步骤链：

```
baseline/clean 预检
  → 构建 system prompt（注入 AGENTS.md 契约 + 最近 journal + 上次失败上下文）
  → 跑 recursive 二进制（在自改安全沙箱内）
  → BudgetExceeded 自动 resume（一次）
  → 质量门：cargo test / clippy / fmt（各带一次 resume-fix）
  → 跨 provider self-review
  → verdict：committed / rolled-back / skip-commit / panic-preserved
```

每一步都写进 `.flowcast/runs/<run-id>/`（state.json + run.log.jsonl + report.md +
transcript.json），中断后用同一个 `--run-id` 即可从断点续跑。
（目录名跟随 flowcast 约定：新仓 `.flowcast/`，已有 `.flowx/` 的旧仓自动兼容。）

---

## 前置准备（首次）

1. **链好 flowx**（flowx 仓在 `~/projects/flowx`，通过 `file:` 本地依赖软链）：
   ```bash
   cd .dev/flows && npm install && cd -
   ```
2. **编译 recursive 二进制**（flow 默认调用 `target/release/recursive`）：
   ```bash
   cargo build --release
   ```
3. **配置 provider**（机器级，一次即可）：`~/.flowcast/providers.json`（向后兼容 `~/.flowx/`）已含
   `deepseek` / `minimax` / `glm` 等 profile，API Key 走 `${ENV_VAR}` 插值。
   确认对应环境变量已导出（如 `DEEPSEEK_API_KEY`）。

---

## 怎么跑

**在 recursive 仓根目录执行**（cwd=仓根，`.flowcast/runs/` 产物落在仓根并被自动本地排除）：

```bash
# 1) 直接给目标
node .dev/flows/self-improve.flow.js \
  --goal "给 count_lines 工具加上对二进制文件的跳过逻辑" \
  --provider deepseek

# 2) 从文件读目标（推荐：复杂目标写成 .dev/goals/NN-xxx.md）
node .dev/flows/self-improve.flow.js \
  --goal-file .dev/goals/01-count-lines-tool.md \
  --provider deepseek --reviewer-provider minimax

# 3) 断点续跑（中断后用同一个 run-id）
node .dev/flows/self-improve.flow.js --run-id <id>

# 4) 查看历史 run
node .dev/flows/self-improve.flow.js --list
```

### 常用参数

| 参数 | 说明 | 默认 |
|------|------|------|
| `--goal "<文本>"` | 自改目标 | — |
| `--goal-file <path>` | 从文件读目标 | — |
| `--provider <name>` | recursive 用的 provider profile（`~/.flowcast/providers.json` 里的名字） | 无（不注入） |
| `--reviewer-provider <name>` | 跨 provider self-review 用的 profile（建议与 `--provider` 不同） | 同 `--provider` |
| `--model <name>` | 覆盖 profile 默认模型 | profile 默认 |
| `--run-id <id>` | 指定 run id；续跑用同一个 | `selfimprove-<ts>` |
| `--repo <path>` | 目标仓路径 | 当前目录 |
| `--bin <path>` | recursive 二进制路径 | `<repo>/target/release/recursive` |
| `--budget <N>` | 步数预算（RECURSIVE_MAX_STEPS） | recursive 默认 |
| `--hitl terminal\|wecom` | Human-in-the-loop 后端 | `terminal` |
| `--no-review` | 跳过 self-review | 关 |
| `--no-commit` | 全绿也不提交（只保留改动） | 关 |
| `--list` | 列出历史 run | — |

---

## verdict 含义（决定提交 / 回滚）

| verdict | 含义 | 工作树结果 |
|---------|------|-----------|
| `committed` | 质量门全绿 + review 通过 → 已提交 | 新 commit |
| `rolled-back` | 质量门红灯，或 review 明确 NEEDS_FIX | **硬回滚到 baseline**（含 untracked） |
| `skip-commit` | 无改动 / budget 超限 / `--no-commit` / reviewer 不可用 | 改动保留，未提交 |
| `panic-preserved` | recursive 进程 panic | 保留现场待诊断，不回滚 |

> 关键安全性：**质量门红灯一定回滚**；但 reviewer「调用出错/不可用」时**不丢弃成果**，
> 改为 `skip-commit` + HITL 通知，由人复核。

成功（committed）后 flow 会通过 HITL 通知给出 land 指引：
```bash
git -C <repo> log --oneline <baseline>..HEAD
git -C <repo> checkout main && git merge <branch>
```

---

## 质量门

门链 = **内置默认门**（flow 里 `qualityGatesFor`，语言相关）+ **项目自定义门**
（`<repo>/.flowcast/gates.json`，committed），两者经 `mergeGates` 合并：项目门同名覆盖内置、
新增门追加在后。

内置默认门：

- `cargo test --quiet` — 失败 → resume-fix（把失败输出喂回 recursive 修一次）
- `cargo clippy --all-targets --all-features -- -D warnings` — 失败 → resume-fix
- `cargo fmt --all -- --check` — 失败 → autofix（`cargo fmt --all`）

项目自定义门（`.dev/../.flowcast/gates.json`，与 provider/agent 配置同属项目仓）：

- `e2e` — `sh .dev/scripts/e2e-gate.sh`（argusai smoke），失败 → resume-fix。
  这是 AGENTS.md 列为强制的 E2E 门：脚本封装 argusai 的 init/setup/run-smoke 判定，
  前置缺失（mcp2cli / argusai-mcp / `e2e/e2e.yaml`）一律 HARD-FAIL（红灯），不静默跳过。
  需在含 Docker + argusai 的环境运行。

> 配置形态（map by name，门字段与 `runGate` 一致）：
> ```json
> { "gates": { "e2e": { "cmd": "sh .dev/scripts/e2e-gate.sh", "onFail": "resume-fix", "timeout": 600000 } } }
> ```
> 新增门 / 覆盖同名内置门都在 `gates.json` 里声明，**不改 flow 代码**。

pricing 文件自动探测 `.dev/pricing.yaml` → `pricing.yaml` → `pricing.json`。

---

## 产物与审计

每次 run 在 `<repo>/.flowcast/runs/<run-id>/`（旧仓兼容 `.flowx/`）：

- `state.json` — 步骤状态机（续跑入口）
- `run.log.jsonl` — 结构化事件日志
- `report.md` — 人读报告
- `system-prompt.md` — 本次注入的契约/journal/失败上下文
- `transcript.json` — recursive 的完整 transcript（支持 replay/resume）

`.flowcast/runs/` 会被 flow 自动写入 `.git/info/exclude`（本地排除运行产物，不污染 clean
检查）——只排除 `runs/`，**不排除整个 `.flowcast/`**，这样项目配置 `.flowcast/gates.json`
（以及 providers/agents）仍可正常 committed。

---

## 验证 flow 本身（不烧 API）

flow 自带结构化 E2E（假 recursive 二进制 + 假 cargo，临时 git 仓跑全链路）：

```bash
cd .dev/flows && npm test
```

应得到 2 个用例全绿：成功路径（committed）+ 回滚路径（cargo test 红灯 → rolled-back）。

---

## 与老 self-improve.sh 的关系

- 行为对齐：baseline 预检、system prompt 注入、budget resume、质量门 resume-fix、
  跨 provider review、verdict 语义，都忠实迁移自 `.dev/scripts/self-improve.sh`。
- 新增：可断点续跑、结构化审计产物、可插拔 HITL、provider 配置外置到 `~/.flowcast/`、
  质量门可经 `.flowcast/gates.json` 项目级自定义（含强制 E2E 门）。
- 老脚本保留作参考，新自改一律走本 flow。

## 待办（改进本 flow 自身）

`.dev/flows/goals/` 下是针对 self-improve flow 的改进目标，例如
`001-self-review-structured-verdict.md`（让 reviewer 限步数 + 强制结构化 verdict，
避免误回滚）。这些是**改 flow（JS）**的目标，区别于 `.dev/goals/` 下**改 recursive
kernel（Rust）**的自改目标。
