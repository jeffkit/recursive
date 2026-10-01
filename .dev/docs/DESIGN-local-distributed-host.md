# DESIGN: v2 本地分布式宿主（v3）——节点级断点续跑，不经 console

Status: DRAFT v1（待 jeffkit 评审）
Date: 2026-10-01
Author: 值守 agent
关联: issue-keeper `docs/DESIGN-console-execution.md`（L3/console 路线，本文是其本地替代方案）

---

## 0. 一句话

给 v2 换一个**本地宿主循环**：用 plaita 现成的 DISTRIBUTED 策略逐节点驱动，
checkpoint 落 run 目录；节点失败自动重试一次（G1 语义在本地宿主下天然成立，
见 §3）；Langfuse 全链观测顺手接上。keeper 侧零改动，console 迁移（worker
四件套）不再是我的前置依赖。

## 1. 背景与动机

- v2 现状：bridge 内 `FlowExecution.execute()`（NORMAL 策略）一口气跑完，
  节点间无断点——图重走时 impl 前段已完成的门禁/评审段全部重付。
- L1（wip 分支代码续跑）与 L2（`recursive resume <sid>` 会话续跑）已落地，
  但只覆盖**节点内**损失（impl 的代码与会话）；**节点间**的重付（已绿的
  三门、已过的评审段在重跑时再执行一遍）没有解。
- plaita 的 DISTRIBUTED 策略（`core/strategies.py:208`）是现成的：宿主逐节点
  驱动、checkpoint 是宿主手里的 context dict、EventNode 可挂起。console 的
  `local_executor.py` 就是它的一个宿主（checkpoint 存 SQLite）。
- 2026-10-01 最小实验（/tmp/dist_demo.py，plaita 912f7df 顺带修了
  codeflow EVENT 的 eventType 别名 bug）实证：同一条 flow 在本地以
  DISTRIBUTED 逐节点推进 + 进程崩溃后从落盘 checkpoint 恢复 + 事件挂起/
  恢复，全部工作，零 plaita 核心改动。

### 为什么不是 console（L3 路线降级为可选）

| v2 的实际需要 | console 提供？ |
|---|---|
| 节点间 checkpoint + 崩溃恢复 | 策略层提供，宿主循环 40 行自建 |
| 失败节点重试语义（G1） | **没有**（flow_worker 终态短路 error——P0 验收主缺口） |
| 进程清场（G3 四件套） | **没有**（P0 验收 3/4 FAIL） |
| EventNode 事件挂起 | v2 图里一个 EventNode 都没有 |
| Web UI / executions 列表 | 有——但值守走 state.json 轮询已工作，Langfuse UI 可补 |

结论：console 对 v2 的独有价值（UI、事件挂起）用不上，缺的（G1/G3）
它也没有。本地宿主绕开 worker 四件套直接拿节点级断点。

## 2. 架构

bridge 进程内的宿主循环，替换 `self_improve_bridge_v2.py` 里的
`ex.clean(); ex.execute(fl, params)`（NORMAL 一口气）：

```python
CKPT = run_dir / "checkpoint.json"

def _ckpt_save(ctx: dict, step_id: str):
    tmp = CKPT.with_suffix(".tmp")
    tmp.write_text(json.dumps({
        "flow_id": fl.flow_id,
        "flow_hash": _flow_hash(fl),        # flow 定义指纹（§4）
        "saved_at": time.time(),
        "last_node": step_id,
        "context": ctx,                     # DISTRIBUTED 输出的完整 context
    }))
    tmp.rename(CKPT)                        # 原子替换

def _ckpt_load() -> dict | None:
    ...  # 存在且 flow_hash 一致才返回；否则 None（回退 L1 路径）

ex = FlowExecution(callback_handlers=[_Adapter(StepTracker(state_path)),
                                      LangfuseCallback()])          # §6
result = None
saved = _ckpt_load()
node_failures = 0
while True:
    try:
        r = (ex.run_distributed(fl, saved_context=saved) if saved
             else ex.run_distributed(fl))
    except FlowErrorException as e:                          # 节点失败（§3）
        node_failures += 1
        if node_failures > MAX_NODE_RETRIES:                 # 默认 1
            raise                                            # → engine_error 老路
        saved = _ckpt_load()                                 # last success 断点
        _tracker_note(f"node retry #{node_failures} at {e}") # state.json 留痕
        continue
    _ckpt_save(r["context"], r.get("id"))
    saved = r["context"]
    if r.get("is_end"):
        result = r["result"]
        break
```

## 3. G1 失败节点重试：本地宿主下是"免费"的

console 的 G1 难点（设计稿 §G1：`flow_worker.py:272` 把 error 当终态短路、
拦住 resume）在本地宿主下不存在，因为我们**自己就是那个循环**：

1. 节点失败时 `NodeRunner.run_node` 在 `context.update_node_result` **之前**
   抛出（`core/runner.py:205-220`）——失败节点在 context 里**没有条目**，
   `last_node_id` 仍指上一个成功节点；
2. 宿主捕获 `FlowErrorException` 后照常保存 checkpoint（此刻它就是
   "停在最后一个成功节点"的合法断点）；
3. `run_distributed(saved_context=...)` 的 continue 路径经
   `_get_next_from_last`（`strategies.py:268-284`）解析出**下一个节点 = 刚
   失败的那个** → 原地重执行。已成功节点（impl、已绿的三门）不再执行；
4. continue 守卫（`strategies.py:252` 的 pending 拦截）只作用于挂起中的
   EventNode——v2 无 EventNode，不受影响（demo 实证）。

即：**错误放行 + continue 步进**的语义不需要改 plaita 任何代码，纯宿主侧
捕获-保存-续传。策略层零改动 = 不背 plaita 版本升级的兼容包袱。

重试策略（宿主政策，可配）：每节点最多自动重试 1 次（`MAX_NODE_RETRIES=1`，
env `RECURSIVE_NODE_RETRIES`），重试仍败 → 向上抛 → bridge 走既有
engine_error 收尾（WIP 快照 + RESULT），checkpoint 保留供 keeper reopen 后
继续（重派时先续 checkpoint，无 checkpoint 才走 L1）。

## 4. checkpoint 设计

- **位置**：`<run_dir>/checkpoint.json`（run_dir 已是 keeper 工件根，随
  终态保留；不进 worktree——回收 rmtree 伤不到它）。
- **schema**：`{flow_id, flow_hash, saved_at, last_node, context}`。
- **flow_hash**：flow 定义内容指纹（sha256 of `self_improve_flow_v2.py`
  源码）。不一致（flow 升级后重派）→ 丢弃 checkpoint 回退 L1——context 里
  的节点结果对新图不再可信，宁可重走也不续错。
- **原子写**：tmp + rename。
- **与 L1/L2 的组合矩阵**（谁在什么场景兜底）：

| 场景 | 兜底层 |
|---|---|
| 节点失败（门禁红、评审 NEEDS_FIX 后挂） | **v3 宿主**：原地重试，成功节点跳过 |
| keeper 重派（同 run 无 checkpoint） | L1：wip 分支基线继承 |
| impl 超时/被杀（节点内损失） | L1（代码）+ L2（会话 resume） |
| 进程崩溃/断电（图中断） | v3 checkpoint（磁盘上）→ 重派续走 |
| checkpoint 与 flow 版本不符 | 丢弃 → L1 |

## 5. 契约不变量（keeper 与值守零改动）

- `state.json`（status/currentStep/verdict/node_timings）与 RESULT 行契约
  不变——reaper 语义、台账、兜底评论全部不动；
- StepTracker 照常挂 callback_handlers（DISTRIBUTED 下 on_node_start/end
  照发，且可加 suspend/resume 钩子留痕）；
- run_dir 布局只新增 `checkpoint.json` 一个文件；
- v1/v2 的 L1 基线继承、L2 会话存储逻辑不变。

## 6. Langfuse 接线（净增观测）

`plaita.obs.LangfuseCallback`（obs.py:94，有单测）加入 callback_handlers：
trace id = execution_id，覆盖 agent 内部循环、token 用量、节点 span。
`LANGFUSE_*` env 已经在 SUBPROCESS_ENV_EXTRA 注入白名单里，bridge 进程
自身读环境即可，无需新配置。此前 v2 只有 state.json 探针、没有 trace——
本次一并补上（可事后在 Langfuse UI 里回放每单全链）。

## 7. 测试计划（harness 扩展，三过纪律不变）

`test/flow_v2_paths.py` 新增场景（复用桩替身 + 真 git fixture）：

- s11 宿主逐节点推进等价性：同一 flow 场景 s1 在 v3 宿主下终态一致、
  节点执行序列一致；
- s12 崩溃恢复：第 N 步后丢实例 → 新 FlowExecution + 落盘 checkpoint 续走
  → 终态 committed；
- s13 节点失败自动重试：gate 首红（脚本序列 [1,0]）→ 宿主重试 → committed，
  且断言失败前的节点**未重复执行**（桩调用计数）；
- s14 重试耗尽：双红 [1,1] → engine_error 且 checkpoint 保留；
- s15 flow_hash 不符：checkpoint 残留 + flow 改动 → 丢弃 → 走 L1 路径。

## 8. 灰度与回滚

- 开关：env `RECURSIVE_HOST_V3=1`（bridge 读取；keeper 的
  `pipeline_repos.jeffkit/recursive` 可经 `agent_env`/setup 注入，或先手动
  在 daemon env 加）——默认关，NORMAL 原路；
- 首批 canary：#68 派发时开（它是最大的单，收益最直观）；
- 回滚 = 关 env，零迁移成本（checkpoint 文件留盘无害）。

## 9. 工作量

| 项 | 估时 |
|---|---|
| 宿主循环 + checkpoint I/O + 重试政策 | ~2h |
| LangfuseCallback 接线 + env 核验 | ~0.5h |
| harness s11-s15 | ~2h |
| 三过 + 灰度观察 | 半个班次 |
| **合计** | **~0.5 天编码 + 1 个观察班次** |

## 10. 开放问题（留评审）

- D1 `MAX_NODE_RETRIES=1` 是否合适？与 keeper 层 engine_error 自动重试
  （连续 2 次升级）叠加后，单节点最多被执行 2×2=4 次——是否需要在
  state.json 暴露 node_retries 计数给值守判读？（我倾向暴露）
- D2 checkpoint 里是否要记 `impl` 的 session_id 快照？（现状 preflight 每
  次扫 sessions 目录取最新——够用，但记下来可在「多会话并存」时消歧）
- D3 flow_hash 用源码 sha256 会让「注释级改动」也失效 checkpoint——是否
  放宽为图结构指纹（flow_id + 节点 id 集）？（我倾向后者）
- D4 后续若 console 仍要迁（worker 四件套修完后），本宿主的 checkpoint
  schema 是否向 console 的 SQLite 记录看齐以便平移？（可后置）
