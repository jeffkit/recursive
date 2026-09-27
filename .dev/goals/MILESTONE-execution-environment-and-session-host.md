# Milestone — Execution Environment（沙箱）与 Session Host（前端公共逻辑下沉）

> 本文件是索引，不是可执行的 goal。执行单元是下面的每个 `3xx-*.md`（Goal 392–405）。
> 面向：recursive 从「单进程高密度 agent 内核 + 一个缺少宿主层的多会话壳」
> 演进为「**分层隔离的执行环境** + **所有前端共享的会话宿主**」。

## 背景（2026-09-27 实测，release 二进制，mock provider）

| 观测 | 数值 | 含义 |
|---|---|---|
| 空载 RSS | 11.5–12.2 MB | 内核本身极轻 |
| 1000 个空闲会话 | 70 MB（+58 KB/会话） | in-process 高密度成立 |
| 单轮 CPU / 服务端 p50 | 0.42 ms / 0.88 ms（无工具） | 运行时开销比 LLM 延迟低 3 个数量级 |
| 默认并发（`max_concurrent_runs=8`）@200ms LLM | 38 turns/s，p50 1.66 s | 闸门决定吞吐 |
| 同负载 cap=64 | 295 turns/s，p50 208 ms | 放宽闸门即线性扩展 |
| 600 轮 HTTP 会话落盘 | **0 字节** | HTTP 会话纯内存，驱逐即丢数据 |
| HTTP 会话上下文管理 | **无 compactor / 无 cap** | `compact_on_overflow` 返回 `false` → 上下文超限致命 |
| 峰值 RSS（32 并发 × 20 轮 × 20KB） | 106 MB（≈2.97 MB/会话） | 容量规划必须按峰值，不能按空闲 |
| `ReadFileState` 缓存上限 | 100 × 256 KB ≈ 25.6 MB（**进程级共享**） | 成本付了、隔离没拿到 |

结论：**内核是高密度的；缺的是宿主层（生命周期/准入/持久化/隔离绑定/观测）与执行面（沙箱）**。
宿主层今天被 HTTP 前端隐式实现，因此所有 hosting 问题都只在 HTTP 上暴露——这也是为什么
第 1 批不是「修 HTTP」，而是把公共逻辑下沉。

## Goal 一览

| Goal | 标题 | 批次 | 依赖 |
|---|---|---|---|
| 392 | HTTP 观测：密度与队列 gauge | 1 | — |
| 393 | HTTP 会话上下文管理对齐 | 1 | — |
| 394 | 会话级工具状态隔离（`fork()` 不再是 `clone()`） | 1 | — |
| 395 | 抽出 `SessionHost`（含 reaper 锁范围修复） | 2 | — |
| 396 | HTTP 会话持久化写路径 | 2 | 395 |
| 397 | HTTP 会话冷加载 | 2 | 396, 394 |
| 398 | HTTP 准入有界化（排队超时 503） | 2 | 395 |
| 399 | 执行预算接线（`RECURSIVE_WALL_TIMEOUT_SECS` 生效 + HTTP 安全默认） | 独立 | **385（硬前置）** |
| 400 | 执行环境契约（capabilities + 失败分类） | 3 | — |
| 401 | `Read`/`Write`/`Edit` 走执行环境 | 3 | 400 |
| 402 | `Glob`/`Grep`/`count_lines` 走执行环境 | 3 | 400, 401 |
| 403 | `ContainerEnvironment` + 选择入口 | 4 | 400, 401, 402 |
| 404 | 会话级环境绑定 + 能力注入 + 后台任务随环境销毁 | 4 | 395, 403（建议 394） |
| 405 | microVM 档设计 + E2B 适配接线 | 5 | 403, 404 |

批次顺序：

```
批次 0  385（已有）—— 任何需要动 kernel.rs / runtime.rs / run_core.rs 的 goal 必须先落它
批次 1  392 ─┬─ 393 ── 394          （392 与 394 可并行）
批次 2  395 ── 396 ── 397
        └──── 398
批次 3  400 ── 401 ── 402
批次 4  403 ── 404
批次 5  405
独立    399（依赖 385；可与批次 3 并行）
```

**可并行**：392 ∥ 394；398 ∥ 396/397；400 ∥ 395（不同文件）；401 ∥ 402（402 依赖 400 但
与 401 的文件集不重叠，若担心 `transport.rs` 冲突则串行）。

## 设计红线（每个 goal 都必须遵守）

1. **不要引入第三套抽象。** 已有 `ToolTransport`（`src/tools/transport.rs:50-77`，含
   read/write/list/mkdir/exec，且 `ToolRegistry` 已持有 `Arc<dyn ToolTransport>`，
   `src/tools/registry.rs:111`）与 `ToolSetProvider` /
   `SandboxMode::{None,Policy,Container,MicroVm}`（`src/tool_set_provider.rs:16-39`）。
   执行面**扩展**它们，不要新建平行体系。
2. **默认档不变。** Tier A（in-process + 路径 jail）保持默认，密度优势（实测 ~50 KB/会话，
   1000 会话 70 MB）不能丢；容器/microVM 一律 opt-in。
3. **隔离 ≠ 安全。** 容器/microVM 只界定爆炸半径。出网策略与凭据不下发沙箱属于下一个
   milestone；任何 goal 不得声称「已安全」。
4. **不动内核不变量。** Invariant #1（`run_inner` 保持小）、#3（`tools::resolve_within`）、
   #7（finish reason 是数据）、#8（tool-call ↔ tool-result 配对）全部适用。
5. **测试同批落地。** `agent-presence` 门要求动 `src/` 必须带测试；`agent-mutants` 门
   要求测试真的 pin 住行为（同义反复的测试会被打回 resume-fix）。
6. **不新增依赖**，除非 goal 显式授权并给出理由（invariant #6）。容器档复用已有 `bollard`，
   microVM 档复用已有 `reqwest`。
7. **默认档的 prompt 与输出格式逐字节不变**（e2e 是 replay 模式，prompt/格式漂移会破门禁）。

## 完成后应具备的能力（验收口径）

- `recursive http` 可以：会话被驱逐/关闭后 transcript **落盘**、重启后**冷加载**继续；
  队列满或等待超时**快速 503** 而不是无限排队；`/metrics` 能看到**在飞/等待/会话/
  transcript 字节**；会话有**默认执行预算**（不再可能无限占住 permit）。
- 同一份 agent 逻辑，在 `--sandbox=none`（默认）与 `--sandbox=container` 下对
  `Read`/`Write`/`Edit`/`Glob`/`Grep`/`Bash` 行为一致，并有门控集成测试证明
  （容器内非 root、无网络、销毁无残留）。
- 会话级工具状态（read-before-edit 护栏、touched-files、后台任务、环境句柄）**不再跨会话串味**。
- 系统提示在**环境档**下明确告知模型能力真相（网络开关、路径根、可用工具链），
  默认档提示不变。
- microVM 档有**成文的设计与判据**，并通过 E2B 适配可被选择（不自建平台）。

## 非目标（明确不做）

- 自建 Firecracker/Cloud Hypervisor 集群、镜像体系、warm pool、快照编排（Goal 405 只出
  设计 + E2B 适配）。
- 出网策略、凭据 broker、审计体系（下一个 milestone）。
- 工作区级数据隔离（overlayfs / CoW / 快照克隆）——它是「并行候选探索」的前提，
  单独排期，不在本 milestone 的 405 之内。
- WASM 执行档（跑不了 `cargo test`，不适用 coding agent）。
- 重写 `AgentKernel` / `RunCore`。

---

## Issue 索引

- [ ] #19 — Goal 392 HTTP 观测：密度与队列 gauge（批次 1，无依赖）
- [ ] #20 — Goal 393 HTTP 会话上下文管理对齐（批次 1，无依赖）
- [ ] #21 — Goal 394 会话级工具状态隔离（批次 1，无依赖）
- [ ] #22 — Goal 395 抽出 SessionHost + reaper 锁范围修复（批次 2，无依赖）
- [ ] #23 — Goal 396 HTTP 会话持久化写路径（批次 2，依赖 #22）
- [ ] #24 — Goal 397 HTTP 会话冷加载（批次 2，依赖 #23 #21）
- [ ] #25 — Goal 398 准入有界化 503（批次 2，依赖 #22）
- [ ] #26 — Goal 399 执行预算接线（独立，**硬前置：Goal 385 / 代码规模 headroom 未落前不要启动**）
- [ ] #27 — Goal 400 执行环境契约（批次 3，无依赖）
- [ ] #28 — Goal 401 Read/Write/Edit 走执行环境（批次 3，依赖 #27）
- [ ] #29 — Goal 402 Glob/Grep/count_lines 走执行环境（批次 3，依赖 #27 #28）
- [ ] #30 — Goal 403 ContainerEnvironment 容器档（批次 4，依赖 #27 #28 #29）
- [ ] #31 — Goal 404 会话级环境绑定（批次 4，依赖 #22 #30）
- [ ] #32 — Goal 405 microVM 档设计 + E2B 接线（批次 5，依赖 #30 #31）

## 推荐起跑顺序

```
批次 0  Goal 385（仓库内已有 goal，未落 issue）—— kernel.rs / runtime.rs 的 line-budget 前置
批次 1  #19（392）∥ #21（394）→ #20（393）
批次 2  #22（395）→ #23（396）→ #24（397）   ∥   #25（398）
批次 3  #27（400）→ #28（401）∥ #29（402）
批次 4  #30（403）→ #31（404）
批次 5  #32（405）
独立    #26（399，需 385 先落）
```

