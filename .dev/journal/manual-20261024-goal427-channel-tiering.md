# Goal 427 — #200 生产通道分级成文（S1 channels.md + S2 README 对齐 + S3(a) 挂账收口）

- **Date**: 2026-10-24
- **Goal**: Goal 427（recursive#200；主控批 A 席草案，B2 执行；基线 `47fbc85c` = origin/main）
- **Files touched**:
  - `docs/architecture/channels.md`（新增，S1 主体）：分级总表（引 owner #200 2026-10-18 裁决语义 + 复裁条款）→ 消费面证据表（核查基线 47fbc85c，证据失效日语义成文；核校修正记录入档）→ 六通道各一节「生产承诺/降级面/不支持清单」（降级面行为全部 file:line 锚点；实验级=修复不排产但安全底线〔§9 沙箱/权限锚点〕与生产同规、门不撤）→ MCP client/server 口径节（client 面=工具集成非通道；#190 五子单全 CLOSED/本体 OPEN 事实记录，不代关）→ 挂账处置节（§10）→ weixin 边界一句（§11）。
  - `README.md`（S2 surgical，#1-#8）：headline 分级注记；features MCP 条目拆 client/server 措辞（server 标 *experimental*）+ TUI 标实验 + 新增 ACP 一行实验级指路（已批可选①）；quickstart TUI 精度修正（「connects to HTTP server」→ in-process backend，HTTP 形态=agui-tui）；Docker/Cloud 节加次生产成熟度注记；Python SDK 节一行（承诺跟随 HTTP 面）；AG-UI 节一行成熟度注记（已批可选②）。env 表 #5（RECURSIVE_REINJECT_FILES）按规格保留不动。
  - `docs/architecture/cancellation.md` §4.1：McpServer 指路句 surgical 更新为已决状态（S3 拍板 (a)：留，实验预留；`spawn_keyed` key=server name 预置；指向 channels.md §10）。**零代码，变体未删。**
  - `docs/architecture/index.md`：channels.md 注册进 Core Execution 组（盘点表 #9）。
- **Tests added**: 无（纯 docs，S3(a) 零代码路线；CI 口径 = `cargo test --workspace` 全绿即文档不触码证词）。
- **三门**: `cargo test --workspace` / `cargo clippy --all-targets --all-features --workspace -- -D warnings` / `cargo fmt --all -- --check`（结果见交付行与 commit 前置输出）。
- **Notes**:
  1. **证据表核校修正**（分级结论零漂移，四处处号/锚点修正已写进 channels.md §2）: `main.rs` Cmd 枚举 251→252；plaita `run_recursive` 373→370；`CancelKind::McpServer` 变体 59→65（as_str 臂 73 无误，与 cancellation.md 同锚）；草案所引 `backend.rs:435-440` 实为 `mcp_server_transport`（MCP server transport 分类函数），**非** TUI 自身连接分支——TUI「双形态」实为两个二进制：`recursive-tui`（in-process，backend.rs:1/:79）与 `agui-tui`（AguiClient over SSE，main.rs:1-2），README quickstart 精度修正按此落。
  2. **外部仓证据**（flowcast 执行器 `flowcast/executor/recursive-extras.js`、keeper 舰队、im-agentproc executor `im-agentproc/src/bridge/builtin/recursive.rs`）按 repo+path 引用，不落本仓行号；channels.md §2 已写明证据失效日 = 基线 `47fbc85c`（2026-10-24）。
  3. **红线遵守**: 零 `src/**` 改动（git status 仅 4 个文档文件）；宣称全部向分级收敛（实验级加标注、次生产注记，无反向拔高）；TUI 写明「降 SLA 承诺、门不撤」（gates.json tui-presence/tui-mutants 硬性）；MCP client/server 口径分节不混写；#190 只记录不代关；#146/#152 仅指路；S4 未关单未动标签。
  4. 未跑本地 mutants（规格 §2b 豁免）；未 push、未动 main；交付 commit 留 wt-b2 等主控验收。
