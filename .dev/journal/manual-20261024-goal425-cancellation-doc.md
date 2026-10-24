# Goal 425 — #198 DESIGN 文档落 docs/（#188 收口最后交付物）

- **Date**: 2026-10-24
- **Goal**: recursive#198（owner 拍板契约方向的交付收口）/ #188（tracking，四子单 #164/#165/#166/#177 全 CLOSED + #179）：把已落地的进程所有权/取消契约从源码注释与 dev journal 升格为 `docs/` 单一权威。纯 docs，`src/**` 零触碰。
- **Baseline**: `584f2f63`（origin/main tip）。主控拍板 S3 = **(a) 文档追认边界，零代码**。

## Files touched

- `docs/architecture/cancellation.md`（新）— 四段结构对齐 plaita `DESIGN-cancellation-and-lease.md`，按「已落地待成文」现状映射：§1 现状地图（五通道 + destroy 链逐环 file:line + 精确 cancel 语义）/ §2 断点清单 → 已闭合映射（**四断点全部已闭合**，显式声明与 plaita 先例的形态差）/ §3 四契约（权威符号 + 回归 + 变异负控，含契约 × 回归 × 负控总表）/ §4 迁移路径（S3(a) 拍板落章 + ACP 协作式 cancel 防误读 + 「审计后有意不动」清单 + 诚实边界）。frontmatter/Source 行风格对齐 `compaction.md`。
- `docs/architecture/index.md` — Core Execution 组注册一行。
- `docs/architecture/execution-environments.md` — `destroy` 行后 3 行：teardown = liquidation 指针。
- `docs/architecture/tools/shell.md` — Timeout 段 3 行：超时整组收指针。
- `docs/architecture/sessions.md` — Related Concepts 一行：teardown 路径 → 清算指针。

## 锚点核校（规格两表「核校后落」）

全部在 `584f2f63` 实证，未照抄：

- `process.rs:663` descendant-survived 断言（去 killpg 负控）✓；`:65` `CancelKind::McpServer` 仅声明 + `:73` as_str，**全仓零构造点** ✓。
- `cancel_scope` / `cancel_task` 调用方：`run_background.rs:1760`、`transport.rs:1328`，**均在测试模块**；生产侧零调用方 ✓。
- `transport.rs:927-930` Exec 注册（scope = cwd）/ `:936` spawn_owned / `:958-962` 三臂 select / `:972`、`:982` terminate_group / `:985-988` Interrupted ✓；ssh 两处 `:542`+`:566`、`:719`+`:740` ✓。
- `run_background.rs:211` spawn_keyed（key `background-job:<scope>/<id>`）/ `:683` spawn_owned / `:931-1002` run_background_job（reader 先于 waiter、三臂 select）/ `:643-652` sandbox 路径 cancel 臂 / `:442-490` ReclaimReport + reclaim_all ✓；测试名与行号 `:1724`/`:1785`/`:1834`/`:1854`/`:1903` ✓。
- `runtime.rs:1426-1460` destroy_environment（once-guard `:1432`、WARN `:1442-1451` 注释引 owner ruling、info `:1452-1457`、transport destroy `:1459`）✓；`:1490` set_interrupt_token（同时设 kernel.shutdown_token）✓。
- `mcp.rs:643` spawn_grouped / `:2611` kill_gracefully（SIGTERM 组 → 宽限 → kill_group，leader 优雅退出后仍 kill_group）/ `:385` tool_timeout、`:609-617` read_timeout 不被 tool_timeout 放宽（#181）✓。
- `runner.rs:754` attach_group / `:948` kill_group ✓。
- ACP：`server.rs:179` TurnControl / `:208` cancel_session / `:1589-1592` 空参 cancel 选最近 prompt 的 session / `:1626-1676` turn task 跑 `rt.run()` 到完、不 abort / `:1648-1651` stopReason 映射 ✓；`acp/session.rs:165` refresh_cancel_token ✓；5 条回归 `:3102`/`:3140`/`:3179`/`:3237`/`:2857` + 4 条 `session_cancel_*` `:2794-2840` ✓。
- RunCore token 两个消费点：`run_core.rs:934` check_shutdown 步界探针；`:1260` token 传入 `llm.stream` → `:1082` make_cancelled_outcome（Goal 353 流中断）。**比规格「纯步界」更精确，文档如实写两点**（在途工具不被 abort 的结论不变）。
- commit `75749136`、`4abc3a81` 均 `git merge-base --is-ancestor main` ✓。

## Tests added

无（纯 docs）。CI 口径 = `cargo test --workspace` 全绿 + 新文档在 index.md 可达。

## Notes

- **成文不重设计**：全文只描述 `75749136` + `4abc3a81` 已落实现；S3 两处半落按主控 (a) 追认为记录在案的边界（exec scope 句柄 = 预留可达接口；`McpServer` 变体挂账 #200），不接线不删变体。
- **形态差显式声明**：plaita 先例的「断点清单」是待修清单，本文 §2 是已闭合映射，intro 与 §2 开头两处明说，不假装还有未落设计。
- **防误读**：§4.2 专段写明 ACP cancel 为步界协作式（token 两消费点），「cancel 立即杀在途工具」是误读。
- **不吸收** #200 / #146 / #152：仅在 §4.4 诚实边界指路。**不关单不动标签**（S4），等主控验收回评。
- `.dev/journal/manual-20261018-goal412-process-contract.md` 与 `src/process.rs` module doc 仍是语义底稿；本文是地图，冲突时以实现 + 测试为准、升 issue。
