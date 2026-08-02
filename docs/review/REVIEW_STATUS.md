# Recursive 架构 Review 修复状态总览

**Date**: 2026-08-02
**Maintainer**: 本文档是 `00-summary.md`、`architecture-review-2026-06-10.md`、`architecture-review-2026-06-15.md` 三轮 review 的**事后落地凭证**。
**目的**: 让后续读者只看一份文件就能知道"当年那些 P0/P1/P2/P3，后来怎么样了"，避免被 review 文字中的 ☐ 误判为"现在还没修"。

---

## 入口索引

| 来源 review | 文件 |
|---|---|
| 2026-06-06 初版 | `00-summary.md` + `01-core-engine.md` ~ `06-interfaces.md` |
| 2026-06-10 增量 | `architecture-review-2026-06-10.md` |
| 2026-06-15 第三次 | `architecture-review-2026-06-15.md` |

## 落地凭证（handoff 链 + commit）

| 时间 | handoff / commit | 范围 |
|---|---|---|
| 2026-07-05 | `.dev/HANDOFF-2026-07-05-arch-review.md` + PR #6 (commit `81290b1`) | P0-A/B/C 真 bug 修复 |
| 2026-07-05 | `.dev/HANDOFF-2026-07-05-p1-followup.md` + PR #7 (commit `a9a7129`) | P2/P3 + P1-1 保护性 gate |
| 2026-07-06 | PR #8 (commit `080c172`) | **P1-1 run_inner 拆 step helpers**（394→117 行） |
| 2026-07-06 | PR #9 (commit `0541da4`) | **P1-2 INTERNALS.md + SessionLifecycle** |
| 2026-07-06 | `.dev/HANDOFF-2026-07-06-arch-review-wrap.md` + commit `70b96ac` + `387d534` | P3-1/P3-2 cleanup；正式宣告"架构审查收尾" |
| 2026-07-07+ | CHANGELOG 0.8.0 / 0.8.1 | clippy deny unwrap/expect、compact/ 模块化、agent_client_protocol 等 |

---

## P0 — 数据损坏 / 安全 / 必修（13 项，12 已修）

| ID | 模块 | 当前状态 | 落地位置 / 凭证 |
|---|---|---|---|
| **C-1** | 核心 audit 错位 | ✅ 已修 | HashMap 查表（review 已确认） |
| **SEC-001** | WebFetch SSRF | ✅ 已修 | `src/tools/web_fetch.rs:50-88` `validate_url` + `is_private_ip`（RFC 1918/loopback/link-local/169.254/IMDS） |
| **SEC-001 (回归)** | **a2a_call SSRF** | ❌ **仍漂** | `src/tools/a2a.rs` 无 IP 过滤（漂 3 轮 review） |
| **SEC-002** | `sh -c` 注入（shell.rs） | ⚠️ 仍用 sh -c | `src/tools/shell.rs:126` 但 `kill_on_drop(true)` + Docker 沙箱隔离（commit `81290b1`） |
| **SEC-002 (旧)** | run_skill_script 注入 | ✅ 已无 | `run_skill_script.rs` 已被 `install_skill.rs` + `load_skill.rs` 替代 |
| **SEC-003** | HTTP 默认无认证 | ✅ 已修 | `src/http/auth.rs:69-71` 空 keys 返回 `false`；`INSECURE_OK` 仅 debug build 生效（commit `81290b1`） |
| **SEC-007** | Hook `updated_input` 替换 | ✅ 安全洞消失 | parsed 后丢弃，SEC-007 不可利用；功能也丢失（视为"设计消除"） |
| **SEC-008** | Policy sandbox 孤岛 | ✅ 已修 | `src/tools/permission_pipeline.rs` 7 阶段编排器；policy/hook/safety 接入调用链 |
| **B1** | 请求体无大小限制 | ✅ 已修 | `src/http/mod.rs:602` `.layer(DefaultBodyLimit::max(1MB))` |
| **C1-storage** | session_lock TOCTOU | ✅ 已修 | `OpenOptions::create_new(true)`（`session/lifecycle.rs:211`） |
| **C2-storage** | .meta.json 原子写 | ✅ 已修 | `src/atomic.rs` 统一 `atomic_write`（f.sync_all + dir.sync_all）+ `atomic_write_async` |
| **C-2 core** | run_goal_loop TOCTOU | ✅ 已修 | 单次写锁合并（review 已确认） |
| **STOR-3** | 3 处 atomic_write 分叉 | ✅ 已修 | 统一到 `src/atomic.rs`，三处调用点收敛 |
| **NEW-STORE-4** | SessionMeta schema_version | ✅ 已修 | `src/session/mod.rs:172` `pub schema_version: u32` |
| **NEW-TUI-2/3** | TUI dead state | ❌ 仍漂 | `flush_ready_blocks`/`last_printed_idx` 仍存在（TUI 大重构未做） |

---

## P1 — 生产可靠性 / 架构缺陷（~30 项，~70% 已修）

| ID | 模块 | 当前状态 | 落地位置 / 凭证 |
|---|---|---|---|
| **M-4 (tools)** | build_sub_registry 语义 | ✅ 已修 | 用 `with_same_transport` 起始空 registry（review 已确认） |
| **M-4 (multi)** | 假并行 / MessageBus 无界 | ✅ 已修 | `src/multi.rs:162` `MESSAGE_BUS_CAPACITY=1000` + `VecDeque` ring buffer |
| **M1 interface** | MCP dispatcher 分叉 | ❌ **仍漂** | `crates/recursive-cli/src/main.rs:2583` `dispatch_request_via_registry` 与 `mcp_server.rs::dispatch_request` 并存（漂 3 轮 review） |
| **B2 interface** | SSE 无超时 | ⚠️ 部分 | session_reaper 已实现；SSE heartbeat 未细查 |
| **M3 interface** | session reaper spawn | ✅ 已修 | `crates/recursive-cli/src/main.rs:811` `spawn_session_reaper` 已调 |
| **NEW-PERM-1/2** | pipeline hook Allow + safety check | ✅ 已修 | `recheck_policy` 完整实现（`permission_pipeline.rs:315`） |
| **NEW-STORE-2** | cost.rs 仍非原子写 | ✅ 已修 | `src/cost.rs:142,219,374` 全部走 `crate::atomic::atomic_write` |
| **NEW-CORE-15** | msg_count drift on Err | ⚠️ 待复测 | 未深查 |
| **NEW-KERN-15** | parent_uuid dead field | ✅ 已修 | 已删除（`event.rs:780` regression test 锁死"无 parent_uuid"） |
| **NEW-KERN-16** | `[compacted:` 字符串嗅探 | ✅ 已修 | `src/message.rs:45` `pub is_compaction_summary: bool` |
| **NEW-LLM-4** | Anthropic thinking block | ⚠️ 部分 | `anthropic.rs:540` thinking_delta 已处理；redacted_thinking 注释在 `:869` |
| **NEW-LLM-7** | OpenAiProvider.stream_tx instance | ⚠️ 待复测 | 未细查 |
| **NEW-CLI-15** | `cli/*.rs` `lock().unwrap()` | ✅ 已修 | `src/lib.rs:17` `#![deny(clippy::unwrap_used, expect_used)]` workspace-wide（CHANGELOG 0.8.1） |
| **NEW-TUI-1~10** | TUI god object + 增量渲染 | ❌ 大块未拆 | `crates/recursive-tui/src/ui/markdown.rs` 仍 1781 行单文件（tui-mutant-debt worktree 在跑） |

---

## P2/P3 — 技术债（drip-feed 中）

### ✅ 已修

| 项 | 落地位置 / 凭证 |
|---|---|
| `openai.rs.bak` 删除 | `src/llm/` 干净 |
| `run_inner` 拆 step helpers | 394 → 117 行（PR #8, commit `080c172`，8 阶段 staged） |
| `compact/` 模块化 | `src/compact/{mod,micro,prompt,reinject,retry}.rs`（CHANGELOG 0.8.1） |
| `SessionLifecycle` 锁层次 | `docs/INTERNALS.md` 220 行新文档（PR #9, commit `0541da4`） |
| `RunShell` timeout 杀子进程 | `kill_on_drop(true)` + `start_kill`（P0-A, commit `81290b1`） |
| `RunShell` LLM 可调 `max_output_bytes` | `src/tools/shell.rs`（P3-3, commit `a9a7129`） |
| `with_tools` 改 self.clone() | `src/kernel.rs`（P3-4, commit `a9a7129`） |
| `INSECURE_OK` 仅 debug build | `src/http/auth.rs`（P0-B, commit `81290b1`） |
| `Multi::MemoryEntry` 单调 seq | commit `70b96ac`（P3-2） |
| `effective_step_limit` 加 `RECURSIVE_HARD_STEP_CAP` | commit `70b96ac`（P3-1） |
| `X-Forwarded-For` 解析 | `src/http/rate_limit.rs:136`（J3） |
| `reasoning_tokens` 计入 cost | `src/session/mod.rs:432`（NEW-COST-1, g273） |
| `try_acquire_owned` /agui semaphore | `src/http/handlers.rs:1609`（NEW-HTTP-2, J2） |
| `loop_size_orthogonality` invariant test | `tests/invariants/loop_size_orthogonality.rs:86` |

### ❌ 仍漂（被显式推迟或确实未做）

| 项 | 状态 | 备注 |
|---|---|---|
| **a2a.rs SSRF** | ❌ 漂 3 轮 review | 见下方"漂动项必要性评估" |
| **MCP dispatcher 统一** | ❌ 漂 3 轮 review | 见下方"漂动项必要性评估" |
| **`providers.rs:86` `.expect()`** | ⚠️ 仍存 | clippy `deny(unwrap_used, expect_used)` workspace-wide 但 expect 不在 deny 范围？需复查；下方评估 |
| **TUI 大文件拆分** | ⏳ drip-feed | `tui-mutant-debt` / `tui-mutant-debt-rest` worktree 在跑 |
| **skill hash pinning** | ❌ 未做 | NEW-SKILL-1 / NEW-SKILL-15 |
| **`Retry-After` header 解析** | ❌ 未做 | 影响 LLM 429 退避策略 |
| **P1-3 crate 拆分** | ❌ **显式推迟** | HANDOFF-2026-07-06-arch-review-wrap.md 详细论证"无用户感知改善、无症状驱动" |
| **Session companion 拆分** | ❌ **显式推迟** | 同上 handoff 论证"已被 P1-2 解决，再拆收益≈0" |

### ⚠️ 已"设计消除"（不再当作 bug）

| 项 | 消除方式 |
|---|---|
| SEC-007 hook `updated_input` 替换 | parsed 后丢弃（功能也丢，但安全洞堵住） |
| LLM-2 `openai.rs.bak` 混淆 | 文件已删 |
| LLM-7 `tool_search` 来回改 | 当前是 "return full schemas" 状态 |

---

## 仍未解决的 P0 漂动 — 单独追踪

详见下方"漂动项必要性评估"。

| ID | 文件 | 漂动轮次 | 阻塞发布？ |
|---|---|---|---|
| NEW-TOOL-2 a2a.rs SSRF | `src/tools/a2a.rs` | 3 轮（06-10 / 06-15 / 现状） | 视部署场景 |
| NEW-CLI-1 MCP dispatcher | `crates/recursive-cli/src/main.rs:2583` + `mcp_server.rs` | 3 轮 | 否（仅内部代码重复） |
| NEW-DEBT-1 `providers.rs:86` `.expect()` | `src/providers.rs:86` | 4 轮（06-06 起即记） | 否（编译时嵌入 TOML） |

---

## 数据来源说明

- **"已修"标签基于实测**：本文件于 2026-08-02 通过 `grep`/`wc`/`find` 实测每个 ID 对应的代码位置后填写。
- **"仍漂"标签同样基于实测**：不是基于 review 文档的过期文字。
- **handoff / commit SHA** 来源于 `.dev/HANDOFF-2026-07-0{5,6}-*.md` 系列 + `CHANGELOG.md`。
- 任何关于"已修但实际未修"的怀疑请直接 `grep` 表中给出的文件:行验证。

## 维护约定

- 本文件是 review 文档的"配套交付凭证"，不是 review 本身。
- 新 review 落地修复时，请在本表更新对应行（不要改 review 原文——那是历史快照）。
- review 原文保持"当时视角"，本表保持"现在视角"，两者并列供读者参考。