# Recursive 深度 Review — 2026-08-04

**Reviewer**: Cursor agent (Composer)  
**Baseline**: v0.8.1 (`d434994` / Goal 384 刚落地)  
**Scope**: 意图理解 + 架构 / 代码 / 使用体验；产出可交给 self-improve 的 goal  
**前置阅读**: `.dev/AGENTS.md`、`.dev/ROADMAP-v4.md`、`docs/review/00-summary.md`（2026-06）、`.dev/HANDOFF-2026-07-06-arch-review-wrap.md`

---

## 1. 项目意图（我理解你在做什么）

Recursive 不是「又一个 Claude Code 克隆」，而是三条意图叠在一起：

1. **可嵌入的 Rust ReAct 内核**  
   `AgentKernel`（无状态单 turn）+ `AgentRuntime`（有状态跨 turn）+ 正交的 `ChatProvider` / `Tool` / `EventSink`。目标是把 loop 本身压到极小，能力全部外溢到 tools/providers。

2. **完整本地开发工具平台**  
   在内核外包一层：HTTP/SSE、MCP client+server、ACP、TUI、multi-agent / coordinator、skills、hooks、permissions、cloud storage、sandbox transports。对标 Claude Code / Codex 的「日常写代码」体验，同时可被 agentproc / im-agentproc 当作 profile 后端。

3. **自我改进闭环（真正的 dogfood）**  
   `.dev/goals` + Flowcast `self-improve.flow.js` + 硬质量门（test / clippy / fmt / e2e / tui-mutants）驱动模型改自己的源码。产品与「用自己改进自己」的方法论是一体的——这是仓库最独特的资产。

**一句话定位**：*一个以正交内核为中心、用自迭代方法论硬化出来的 Rust coding-agent 平台*。

当前阶段判断（相对 ROADMAP v4）：

| 层 | 状态 |
|----|------|
| 内核 + invariants | 成熟；Invariant #1 的「大小门」几乎顶满 |
| Persistence / Auth / Observability (Phase 14–17) | 基本完成 |
| TUI 体验对齐 Claude Code | 大幅推进，但仍有关键入口缺口 |
| Ecosystem (SDK / docs / install) | 有，但与「默认用户路径」脱节 |
| Advanced patterns (18.x 自反思 / tool learning / consensus) | 大多仍红 |
| 平台表面扩张 (weixin / acp / agui / coordinator / e2b) | **过宽**，部分应 freeze |

---

## 2. 架构评估

### 2.1 做对的地方

- **Invariant 体系可执行**：`tests/invariants/` 把「loop 小 / 沙箱 / pairing / finish-reason-as-data / no-unwrap」变成 CI 硬门，不只是文档愿望。
- **Kernel / Runtime 拆分清晰**：`run_inner` 本身已压到 ~147 行，职责是「逐步检查终止条件 → LLM → tools → 结果」；真正的复杂度在 sibling helpers（这是正确方向）。
- **工具正交扩展**：`build_standard_tools` + `is_deferred` + `ToolSearch` 是正确的渐进披露骨架；多数 memory/facts 工具已 deferred。
- **自迭代基础设施一流**：goal 文档质量、journal、observation、flow gates、worktree isolation——这套东西比多数 agent 产品本身更有工程价值。
- **2026-06 P0 多数已修**：body limit、SSE heartbeat、session reaper、HTTP auth 默认拒绝、SSRF 系列（371–378）都已落地。

### 2.2 核心张力：Invariant #1 名存实亡的风险

当前行数预算（实测 2026-08-04）：

| 门限 | 现状 | 限额 | 余量 |
|------|------|------|------|
| `kernel.rs` | 998 | 1000 | **2** |
| `runtime.rs` | 3692 | 3700 | **8** |
| `RunCore::run_inner` body | 147 | 150 | **3** |
| `run_core.rs` production | ~1467 | 1500 | **33** |

`run_inner` 看起来很瘦，但 **3834 行的 `run_core.rs`（大半是测试）+ 近顶满的 production helpers** 说明复杂度只是被挪到同文件 sibling。下一个往 loop 周边加 10 行逻辑的 self-improve goal，就有很大概率直接撞红 invariant 测试——这是**整个自迭代流水线的系统性瓶颈**，优先于任何新功能。

### 2.3 表面过宽（keep / invest / freeze / delete）

| 表面 | 规模（约） | 建议 | 理由 |
|------|-----------|------|------|
| Kernel + tools + session | 核心 | **invest** | 产品灵魂 |
| `recursive-tui` | 活跃 | **invest** | 用户主路径；但需接到默认入口 |
| HTTP + MCP | 成熟 | **keep** | SDK / 编排依赖 |
| ACP (`src/acp/` ~7k LOC) | 大 | **keep，但隔离** | IDE 集成有价值；勿污染默认 tool 列表 |
| coordinator / multi / tasks / team | ~3k | **keep / 收敛文档** | 功能在，用例少；别再扩协议 |
| `agui-*` crates | ~2.7k | **keep** | HTTP AG-UI 路径在用 |
| `weixin` feature | ~0.5k，上次实质改动 2026-06-22 | **freeze** | 非默认 feature；别再投 self-improve |
| e2b / cloud-runtime | feature-gated | **keep as opt-in** | 不进默认路径 |
| Plugin system (Phase 16) | deferred | **继续 deferred** | MCP 已覆盖扩展 |

2026-07-06 handoff 推迟「拆 crate / Session companion」的判断仍然成立：没有第三方 platform 消费者，拆 crate 只有成本。

### 2.4 文档与现实漂移

| 声称 | 现实 | 位置 |
|------|------|------|
| 「Nothing → TUI (if compiled in)」 | 无子命令时固定 `Cmd::Repl`（行式 `recursive>`） | `crates/recursive-cli/src/main.rs:615-636` |
| `install.sh`: `recursive # open TUI` | brew/install 只装 `recursive`，不装 `recursive-tui`；默认进 REPL | `install.sh:131` |
| AGENTS.md「Step budget: default 200」 | `RECURSIVE_MAX_STEPS` 默认 **0 = unlimited** | `AGENTS.md:93` vs `src/config.rs:904-918` |
| architecture overview: `src/cli/` / `src/tui/` | 已迁到 `crates/recursive-cli` / `crates/recursive-tui` | `docs/architecture/overview.md:20` |
| ROADMAP v4 Phase 20「依赖图未实现」 | `DEPENDENCY.md` 存在但停在 2026-06-17 | `.dev/goals/DEPENDENCY.md` |

---

## 3. 代码质量（增量发现，不含已修 goal）

已排除：SSRF 系列、swallowed errors(380)、task leaks(379)、compaction 大修(328–347)、stream interrupt(382)、todo panel(384)、runtime builder tests(358) 等。

### P0 / 高优先级（阻塞自迭代或默认体验）

1. **行数预算耗尽** — 见上表。不腾挪空间，后续 goal 会随机撞 invariant。  
2. **默认入口承诺失败** — install/注释说 TUI，代码进 REPL；Homebrew 用户根本拿不到 TUI。

### P1

3. **`ClientReadFile` / `ClientWriteFile` 未 `is_deferred`，且非 ACP 会话也注册** — LLM 看到无用工具，调用必失败（`client_fs.rs:140-165`）。  
4. **Bare `recursive` 与 `recursive-tui` 双二进制割裂** — CLI crate 不依赖 tui；发布路径只推 `recursive`。  
5. **TUI 输入历史不跨会话持久化** — `docs/tui-fake-cc-gap.md` 仍标 🔴；代码无 `save_history`/`HISTFILE`。

### P2

6. **架构文档路径过期**（overview / 若干 INTERNALS 引用）。  
7. **AGENTS.md 运行时契约数字过期**（step budget 200）。  
8. **`cost.rs` 未知模型写 `cost_usd: 0.0`**（`unwrap_or(0.0)`）——观测上像「免费」而非「未知」。  
9. **weixin / 部分 Phase 18 条目** 继续占 roadmap 注意力，应明确 freeze。

### 正面

- Policy sandbox **已接入** `permission_pipeline::recheck_policy`（2026-06 的「孤岛」指控已过时）。  
- Memory/facts 工具大多已 deferred。  
- SSE heartbeat + body limit + session reaper 已在。

---

## 4. 使用体验

### 新用户摩擦排序

1. **装完 `brew install` / `install.sh` 后敲 `recursive`，得到的是 `recursive>` 行式 REPL，不是宣传的 TUI**（无 `/` 命令面板、无 todo panel、无 plan modal）。  
2. 要 TUI 需另装/另跑 `recursive-tui`，README 也写成 `cargo run -p recursive-tui`——对 brew 用户不成立。  
3. 未配置时依赖 `recursive init` / doctor，路径本身不错，但默认入口体验已经先输一轮。  
4. 长会话 TUI 仍缺：跨会话 prompt 历史、虚拟滚动、doctor-on-splash（见 `tui-fake-cc-gap.md`）。

### 投资错位

代码与 self-improve 大量砸在 TUI 细节（Goals 343–384），但**默认二进制入口仍未接上 TUI**。这是「产品完成度」与「发布完成度」的错位——优先修入口，再继续抠 panel。

---

## 5. 战略建议（给 orchestrator）

**短期（1–2 个 batch，先于新功能）**

1. 腾出 invariant 行数余量（提取 helpers / 测代拆文件），否则流水线自堵。  
2. 让 `recursive`（无参数）启动 TUI；REPL 保留为 `recursive repl`。发布物带上 TUI。  
3. ACP 专用工具 deferred 或仅在 ACP 模式注册。  
4. 清一波文档漂移（AGENTS step budget、overview 路径、install 文案对齐）。

**中期**

5. TUI 跨会话历史 + 长 transcript 虚拟滚动。  
6. ROADMAP v4 Phase 18 只挑有 dogfood 价值的做（自反思 / long-running goals），consensus / tool-learning 继续压后。  
7. 写 `docs/architecture/packaging.md`：明确「何时才拆 crate」判据（沿用 2026-07-06 handoff）。

**不要做**

- 现在拆 `recursive-kernel` / `recursive-platform` crate。  
- 重启 native plugin 系统。  
- 继续往 weixin 投 self-improve。

---

## 6. 本次产出的 Goal 清单

写入 `.dev/goals/`，编号从 385 起，可直接丢进 `launch-flow.sh`：

| ID | 文件 | 优先级 | 一句话 |
|----|------|--------|--------|
| 385 | `385-invariant-size-headroom.md` | P0 | 给 kernel/runtime/run_inner/run_core 腾出行数余量 |
| 386 | `386-default-launch-tui.md` | P0 | 无子命令时启动 TUI；对齐 install.sh / 注释 |
| 387 | `387-acp-tools-defer-or-gate.md` | P1 | ClientRead/WriteFile deferred 或仅 ACP 注册 |
| 388 | `388-doc-drift-agents-arch-overview.md` | P1 | 修 AGENTS step budget + architecture overview 路径 |
| 389 | `389-tui-prompt-history-persist.md` | P1 | TUI 输入历史跨会话持久化 |
| 390 | `390-cost-unknown-model-null.md` | P2 | 未知定价写 null 而非 0.0 |
| 391 | `391-packaging-trigger-criteria.md` | P2 | 文档化「何时拆 crate」；freeze weixin 声明 |

建议执行顺序：**385 → 386 → 387 → 388**，然后 389；390/391 可穿插。

---

## 7. 与历史 review 的关系

- **不重复** 2026-06 `00-summary.md` 已修的 P0（auth / SSRF / body limit / audit 错位等）。  
- **继承** 2026-07-06 handoff：不拆 crate、不拆 Session companion。  
- **新主题**是「自迭代空间耗尽」+「默认体验入口断裂」——这是 v0.8 功能堆满之后的下一阶段瓶颈。
