---
type: Architecture
title: Channel Tiering & Downgrade Surfaces — the six channels
description: Production / secondary-production / experimental tiers for the six consumption channels (owner ruling 2026-10-18, issue #200) — per-channel promises, downgrade surfaces with file:line anchors, unsupported lists, the MCP client/server distinction, and the CancelKind::McpServer parked-variant disposition.
tags: [channels, tiering, downgrade, cli, http, agui, acp, mcp, tui, issue-200]
timestamp: 2026-10-24T00:00:00Z
---

# Channel Tiering — the six channels (issue #200)

Source: the owner's tiering ruling in recursive#200 (comment 2026-10-18) plus a
consumption-side evidence sweep (below). This page is the **single
authoritative doc** for which channels carry what kind of promise — README
claims are aligned to it (issue #200). It documents a decided ruling; it is
not a re-grading. If code and page ever disagree, code plus tests win: file
an issue, do not patch either side casually.

The ruling in one line: **CLI/内核 = 生产面；HTTP + AG-UI = 次生产；ACP /
MCP-server / TUI = 实验；安全底线（沙箱 / 权限默认）对所有通道同规、门不撤。**

## 1. The tiering table

| 通道 | 分级 | 一句话依据（owner #200 裁决 2026-10-18 语义） |
|---|---|---|
| CLI / 内核 | **生产面** | 唯一生产消费者 = self-improve 管线自身；管线的 SLA/契约周投入都压在这条通道上 |
| HTTP | **次生产** | C 端方向继续投入（#146 边界装配、#132/#147 已投），但 owner 实证「没有真实外部业务在调用」 |
| AG-UI | **次生产**（随 HTTP） | 机制成熟（#52 系 9 子单全 CLOSED），消费面仍只有仓内 e2e + 自家 `agui-tui` |
| ACP | **实验** | 机制面已修（#183/#179/#180 全 CLOSED），外部 client（Zed 等）零证据、README 零宣称 |
| MCP（server 面） | **实验** | 仓内仅 e2e 形状覆盖，外部 MCP host 零证据；无 SLA |
| TUI | **实验** | 人类交互面，无 headless 消费方；**降的是 SLA 承诺，门不撤**（§7） |

**复裁条款**：消费面证据可推翻分级——新外部消费者出现、或既有消费消失，
回评 #200 由 owner 复裁；复裁落地前本页维持现级，任何一页（含 README）
不得自行升降级。

## 2. Consumption-side evidence (expiry: baseline `47fbc85c`, 2026-10-24)

Every claim below was verified against baseline `47fbc85c` (origin/main tip,
2026-10-24). That is this table's **expiry semantics**: code moves on, so
re-verify anchors before citing this page as decision evidence. External-repo
evidence (flowcast / issue-keeper / im-agentproc) is cited by repo+path, not
by this repo's lines.

| 通道 | 入口（file 锚点） | 真实消费证据 | 外部业务消费 | 与裁决的关系 |
|---|---|---|---|---|
| CLI/内核 | `Cmd` 枚举 `crates/recursive-cli/src/main.rs:252`；Claude 兼容 `--output-format stream-json`（`main.rs:176`）+ wire 映射 `crates/recursive-cli/src/cli/claude_json.rs:1` | ① self-improve：flowcast 执行器本地重建 `runRecursiveDirect`（`.dev/flows/self-improve.flow.js:53-135`，`cli:'recursive'` _meta `:133-137`）；plaita 引擎 `run_recursive` spawn 二进制（`.dev/flows/self_improve_engine.py:370`）；flowcast 仓 `flowcast/executor/recursive-extras.js`（本仓外）。② keeper 舰队经 flowcast 实跑 CLI 面。③ **im-agentproc 内置 executor**（外部仓 `im-agentproc/src/bridge/builtin/recursive.rs`，包装 `recursive --headless --output-format stream-json [-r <session>] -p <msg>`，接口与本基线核对无漂移）——10-18 裁决后出现，方向与裁决一致，**加强** CLI=生产面 | 唯一生产消费者=管线自身（owner 裁决原话） | ✅ 一致 |
| HTTP | `Cmd::Http`（`main.rs:274`，feature `http`）；路由面 `src/http/mod.rs:1290-1362`；in-process trigger scheduler `main.rs:1113` | 仓内：e2e（08-http-api / 21-typescript-sdk / 39-http-auth）+ `sdk/python`·`sdk/typescript`（仓内交付物）+ docker compose 部署形态 + TUI http 分支形态（agui-tui）。外部排查：dsh-lavs/dsh-web-bridge 命中均为 `mkdir recursive:true` 假阳性；#132「借 DSH gateway」=设计参照非消费；im-agentproc 走 CLI 非 HTTP | 零 | ✅ 一致，无需升降级 |
| AG-UI | `POST /agui` + `POST /agui/{thread_id}/cancel`（`src/http/mod.rs:1360-1361`，`src/http/agui.rs`）；crates `agui-protocol`/`agui-client`/`agui-tui` | README AG-UI 节宣称（CopilotKit/`@ag-ui/client` 消费形态）+ docs/architecture/agui.md；#52 系 9 子单全 CLOSED（收口评论逐子单 commit 实证） | 零 | ✅ 一致 |
| ACP | `Cmd::Acp`（`main.rs:271`，feature `acp`，stdio）；`src/acp/server.rs` | 仓内：`e2e/tests/01-acp-initialize.yaml`（仅 initialize 形状）；#183/#179/#180 全 CLOSED。外部 client（Zed 等）：零证据。README 宣称：零（#200 对齐时补一行实验级指路） | 零 | ✅ 一致 |
| MCP（server 面） | `Cmd::Serve`（`main.rs:264`，stdio）；`src/mcp_server.rs:1` | 仓内：`e2e/tests/14-mcp-serve.yaml`；外部 MCP host：零证据。口径警告见 §8（client 面不是通道） | 零 | ✅ 一致 |
| TUI | **独立二进制** `recursive-tui`（`crates/recursive-tui/`，in-process backend `crates/recursive-tui/src/backend.rs:1`，`Backend::spawn` `:79`）；HTTP 协议形态是另一个二进制 `agui-tui`（`crates/agui-tui/src/main.rs:1-2`，`AguiClient` over SSE） | in-process backend + AG-UI-over-HTTP 双形态；人类交互面，无 headless 消费方。门：`.flowcast/gates.json` `tui-presence`/`tui-mutants` 硬性 | 零 | ✅ 一致 |

核校修正记录（相对派单草案，分级结论零影响）：`main.rs` 枚举行 251→**252**、
plaita `run_recursive` 373→**370**、`CancelKind::McpServer` 变体 59→**65**
（`as_str` 臂 73 无误）；草案所引 `backend.rs:435-440` 实为 MCP server
transport 分类函数（`mcp_server_transport`），**不是** TUI 自身的连接分支——
TUI 的双形态锚点以上表两个二进制为准。

## 3. CLI / kernel — 生产面

- **生产承诺**：这条通道的承诺来自消费者，不是来自宣传——self-improve
  管线（flowcast / plaita 两个引擎 + keeper 舰队 + im-agentproc executor，
  §2 表）是它唯一的真实消费者，管线把 SLA/契约周投入压在 CLI 面行为上：
  退出码与 finish reasons 形态、`--output-format stream-json` wire 形状、
  会话续接（`-r`）都是被消费的契约面，行为变化按管线契约对待。
- **降级面**：预算/超时以**数据**收尾而非报错——step 预算
  `FinishReason::BudgetExceeded`（`src/run_core.rs:1016`）、墙钟
  `wall_clock_exceeded`（`src/agent/types.rs:96`）；取消可观测
  （`CancelEvent`，[cancellation.md](cancellation.md) contract 4）。
- **不支持清单**：内核不带租户/身份语义——`TurnContext` 无 identity 字段
  是设计而非缺口（owner 2026-10-18「运行时租户透明」；C 端身份装配归
  #146/#152，此处只指路）；无常驻服务形态（`recursive loop` 仍是单进程
  生命周期）。

## 4. HTTP — 次生产

- **生产承诺（次生产级）**：C 端方向继续投入——验收线=e2e 套件
  （`e2e/tests/08-http-api.yaml` / `21-typescript-sdk.yaml` /
  `39-http-auth.yaml`）+ **SDK 承诺跟随 HTTP 面**（`sdk/python`、
  `sdk/typescript` 是仓内交付物，HTTP 降级面变化时 SDK 文档同步）；
  docker compose 部署形态维持可跑。
- **降级面**（既有行为的显式成文，非新增承诺）：auth 未配置 → **503
  default-deny**（`src/http/auth.rs:103`，落点 `:471`；`INSECURE_OK`
  只是 debug escape hatch，release 构建仍 503，`auth.rs:5-8`）；
  admission 饱和 → **503 + `Retry-After`**（`src/http/handlers.rs:391`，
  header 机制 `src/http/mod.rs:1164-1182`）；`/readyz` 依赖不可用 → 503
  （`src/http/mod.rs:1211`）。
- **审批语义（Goal 429 S2，[policy]）**：HTTP 恒 headless——权限规则里的
  `interactive` 工具走外审 hook（`PermissionRequest` 事件，hook 目录
  `~/.recursive/hooks` / `<ws>/.recursive/hooks`）；有 hook，其
  `permissionDecision` 为权威（#154 fail-closed），无 hook **一律 DENY**。
  启动时 `WARN: HTTP: N interactive rule(s) … will be DENIED on this
  channel` 让该语义可预见。CLI/TUI/ACP 通道的 `interactive` 语义不变
  （各有终端/控制通道审批 hook）。
- **启动姿态行（Goal 429 S3）**：stderr 打一行
  `http security posture: sandbox=<tier> workspace=<path> permissions=<…>
  approval_hooks=<N> auth=<configured|insecure-debug|unconfigured>` +
  四档沙箱单租户 WARN（镜像 microvm 先例）——部署事实启动即说，
  「文档知道的事运行时不说」的缺口收口。
- **trace labels（Goal 430 / #244 W1，owner 2026-10-25 裁决的执行载体）**：
  每个 HTTP turn 的 Langfuse trace 带 `langfuse.trace.metadata.label.<key>`
  属性；W1 默认集只有 `tenant`，来源=**会话 owner 的 tenant**
  （`SessionState.tenant`，非当前请求方；`subject` 不入默认，PII），派生/
  校验/模板展开全在边缘模块 `src/http/labels.rs`（键
  `^[a-z][a-z0-9_]{0,63}$`、值 ≤256B 无控制字符、≤16 键；越界 → WARN
  「trace labels dropped (degradation)」命名键+规则不记值，turn 照跑）。
  **内核零感知**：labels 只住 `RunMeta`（observability 层），collector
  `for (k, v)` 原样透传零键名字面量；`TurnContext` / runtime / tool
  registry 一字不动——由 `tests/identity_opacity_contract.rs` 静态守卫
  钉死（红区 18 文件 + `src/agent/` 生产半区 `tenant|subject` 严格零命中
  + `TurnContext` 13 字段快照）。W2（技能源 URL / 外呼 headers 按 labels
  在 handler 层预展开）见 Goal 431，`expand_template` 已在边缘层就位。
- **不支持清单**：次生产 ≠ 生产 SLA 承诺（宣称面已按本页对齐）；C 端
  身份装配：W1 透传地基已落（上条），技能分区/外呼携带（#244 W2 →
  Goal 431）未落，#146 P3 缓行、属主 #152——只指路，此处不展开不代做；
  Redis session hot-state 不接入 `recursive http`（#92 方向已关）。

## 5. AG-UI — 次生产（随 HTTP）

- **生产承诺**：机制成熟——#52 系「AG-UI 生产化受阻」9 子单全 CLOSED
  （收口评论逐子单 commit 实证）；宣称=协议消费形态（CopilotKit /
  `@ag-ui/client`，见 README AG-UI 节），成熟度=随 HTTP 的次生产。
- **降级面**：随 HTTP 面（§4 的 auth 503 / admission 503+Retry-After
  同样覆盖 `/agui`）；运行取消经 `agui_active_runs` token
  （`src/http/mod.rs:826-834`）。
- **不支持清单**：无外部消费者承诺——今天的消费面只有仓内 e2e 与自家
  `agui-tui`；不要把 README 的协议消费形态读成已验证的外部集成。

## 6. ACP — 实验

- **生产承诺**：无。实验级语义 = **修复不排产**（回归坏了会修，不做
  前向排产与功能承诺）。
- **降级面**：取消 = step 边界协同（[cancellation.md](cancellation.md)
  §4.2，#179 契约）；安全底线（沙箱 / 权限默认）与生产面同规（§9）。
- **不支持清单**：外部 client（Zed 等）零证据、零承诺；README 仅一行
  实验级指路（#200 对齐新增）；e2e 仅 initialize 形状
  （`e2e/tests/01-acp-initialize.yaml`）。

## 7. TUI — 实验

- **生产承诺**：无。人类交互面，无 headless 消费方。
- **降级面**：**降的是 SLA 承诺（实验级修复不排产），不是门**——
  `tui-presence` / `tui-mutants` 在 `.flowcast/gates.json`（`tui-presence`
  条目、`tui-mutants` 条目，后者 CI 化、30min 预算、`onFail: resume-fix`）
  仍**硬性**；勿读成「实验级=无门」。安全底线与生产面同规（§9）。
- **不支持清单**：形态口径=双二进制——in-process `recursive-tui`
  （`backend.rs:1`）/ AG-UI-over-HTTP `agui-tui`
  （`crates/agui-tui/src/main.rs:1-2`）；README 不再宣称
  「connects to HTTP server」单形态。

## 8. MCP client vs server — 勿混写

- 六通道之「MCP」= **server 面**：`recursive serve`
  （`Cmd::Serve`，`main.rs:264`；`src/mcp_server.rs`），把 Recursive 的
  工具暴露给外部 MCP-aware agent。实验级，无 SLA。
- **client 面**（`src/mcp.rs`，消费外部 MCP servers 作工具）是**全通道
  共用的工具集成面，不是六通道之一**——它不参与定级，任何通道都能带
  MCP 工具。引用时勿把 client 侧事实写进 server 通道（#186 系教训）。
  client 侧 #181/#178 已 CLOSED。
- 现状事实记录：MCP client 侧 tracking **#190 五子单全 CLOSED、#190
  本身仍 OPEN**——行政关单归主控/owner，本页只记录该事实，不代关。

## 9. 安全底线 — 全通道同规，实验级不豁免

owner 2026-10-18 裁决语义：**实验级 = 修复不排产，但安全底线与生产面
同规、门不撤。** 底线锚点：沙箱 = fs/shell 工具全走 `tools::resolve_within`
（`src/tools/dispatch.rs:275`）；权限 = permission layer 加载
（`src/config_file.rs` `load_permission_layer`，坏 TOML/读失败 → WARN
可观测，Goal 426 S3(b)）+ 判定主体 = 结构化工具身份
（`src/tools/tool_identity.rs`，Goal 426 S1；`dispatch.rs:39`）；
HTTP `interactive` = **headless 语义**（Goal 429 S2，[policy]）：外审 hook
权威、空 runner fail-closed DENY（`Cmd::Http` 臂 `with_headless(true)`，
管线 Phase 6）。

## 10. 挂账处置 — `CancelKind::McpServer` 留，实验预留（S3 拍板 (a)）

- 现状：`CancelKind::McpServer`（变体 `src/process.rs:65`，`as_str` 臂
  `:73` → `"mcp-server"`）**全仓零构造点**（grep 实证：唯一命中是
  as_str 臂本身）。[cancellation.md](cancellation.md) §4.1 曾明文挂账：
  wire it when a consumer needs functional MCP-server cancellation, or
  delete the variant.
- **处置（Goal 427，主控拍板 (a)，零代码）：留，明标实验预留。** 零构造
  点是记录在案的现状，不是缺口。预留语义 = 未来 MCP host 消费面出现时
  per-call 任务表注册的落点（`spawn_keyed`（`src/process.rs:403`），
  key=server name 模式已在 Goal 425 S3(b) 分析中预置）；不 speculative
  wiring。与 424-S3(a) / 425-S3(a) 同款追认路径。
- cancellation.md §4.1 指路句已同步为「已由 #200/Goal 427 处置：留，
  实验预留」。

## 11. 边界外（不入通道表）

`src/weixin/`（`recursive weixin-daemon` 等，feature-gated 测试夹具性质，
#210 口径）**不是第七通道**，本页以此句为界，不再展开。

## Related Concepts

- [Cancellation & Process Ownership](cancellation.md) — 通道共享的取消/进程所有权契约；§4.1 半落地边界与本页 §10 挂账处置
- [AG-UI](agui.md) — AG-UI 协议层（本页 §5 的机制面）
- [Execution Environments](execution-environments.md) — 安全底线的沙箱分层（本页 §9）
