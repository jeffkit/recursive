# DESIGN: v2 本地分布式宿主（v3）——节点级断点续跑，不经 console

Status: DRAFT v2（四路交叉评审后修订；评审意见以 [D*/R*/O*/T*] 编号并入正文）
Date: 2026-10-01
Author: 值守 agent
评审: 架构×1 / 重试语义×1 / 运维契约×1 / 测试策略×1（全部 APPROVE-WITH-CHANGES，
      本版已吸收全部 P0/P1；意见原文与编号见文末 §11）
关联: issue-keeper `docs/DESIGN-console-execution.md`（L3/console 路线，本文是其本地替代方案）

---

## 0. 一句话

给 v2 换一个**本地宿主循环**：用 plaita 现成的 DISTRIBUTED 策略逐节点驱动，
checkpoint 落 keeper 工件根（跨派发可续）；节点异常自动重试（v2.1 修订：仅
异常类失败，业务红不在此列——§3）；Langfuse 全链观测接上。keeper 派发链
仅增一个 `engine_env` 透传字段（照 agent/reviewer 先例），reaper 语义不动。

## 1. 背景与动机

- v2 现状：bridge 内 `FlowExecution.execute()`（NORMAL 策略）一口气跑完，
  节点间无断点——图重走时 impl 前段已完成的门禁/评审段全部重付。
- L1（wip 分支代码续跑）与 L2（`recursive resume <sid>` 会话续跑）已落地，
  但只覆盖**节点内**损失；**节点间**的重付没有解。
- plaita 的 DISTRIBUTED 策略（`core/strategies.py:208`）是现成的。2026-10-01
  最小实验（`test/dist_demo.py`，plaita 912f7df 顺带修了 codeflow EVENT 的
  eventType 别名 bug）实证：逐节点推进 + 进程崩溃后从落盘 checkpoint 恢复 +
  事件挂起/恢复，全部工作，零 plaita 核心改动。

### 为什么不是 console（L3 路线降级为可选）

v2 的图里**一个 EventNode 都没有**（不需要事件挂起）；console 对 v2 的独有
价值（UI、事件挂起、executions API）用不上，而 v2 需要的 G1/G3 它也没有
（P0 验收实证）。本地宿主绕开 worker 四件套直接拿节点级断点。

## 2. 架构：bridge 进程内的宿主循环

替换 `self_improve_bridge_v2.py` 的 `ex.clean(); ex.execute(fl, params)`。
**v2.1 修订要点**（吸收 D1/T5/O7 的 params 遗漏、D6 的并发防护、R1/D2 的
终态 checkpoint 陷阱、D3/R2/T2 的重试计数、D4 的超时类失败）：

```python
CKPT = ISSUE_ROOT / "checkpoint.json"        # ← keeper 工件根（§4），非 run_dir
LOCK = run_dir / "host.lock"

def _ckpt_save(ctx: dict, step_id: str):
    tmp = CKPT.with_suffix(f".tmp-{os.getpid()}")     # tmp 掺 pid（D6）
    tmp.write_text(json.dumps({
        "flow_id": fl.flow_id,
        "flow_hash": _flow_hash(fl),                  # 图结构指纹（§4，D8）
        "run_id": run_dir.name,
        "saved_at": time.time(),
        "last_node": step_id,
        "context": ctx,
    }))
    tmp.rename(CKPT)                                  # 原子替换

host_fd = os.open(LOCK, os.O_CREAT | os.O_RDONLY)
try:
    fcntl.flock(host_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)   # 双宿主设防（D6）
except BlockingIOError:
    print(json.dumps({"verdict": "retry-later", "stage": "host",
                      "why": "another host holds host.lock"}))
    return 0

params = {"goal": goal, "repo": repo, "run_dir": str(run_dir),
          "agent": agent, "reviewer": reviewer}        # ← fresh 必传（D1/T5）
ex = FlowExecution(callback_handlers=[_Adapter(StepTracker(state_path)),
                                      LangfuseCallback()])          # §6
saved = _ckpt_load()                                  # §4：位置/版本/worktree 三重闸
node_retries: dict[str, int] = {}                     # 按节点计数（D3/R2/T2）
while True:
    try:
        r = (ex.run_distributed(fl, saved_context=saved) if saved
             else ex.run_distributed(fl, params=params))
    except FlowErrorException as e:                   # 仅异常类失败（§3）
        nid = _last_node_hint(e, saved)               # 归一化丢节点身份→从 checkpoint 反推（O7b）
        if _is_timeout_class(e) or node_retries.get(nid, 0) >= MAX_NODE_RETRIES:
            raise                                     # 超时类/预算尽 → engine_error 老路（D4）
        node_retries[nid] = node_retries.get(nid, 0) + 1
        _tracker_note_retry(nid, node_retries[nid])   # state.json 留痕（§5，T7/O6）
        saved = _ckpt_load()                          # last-success 断点
        continue
    if r.get("is_suspend"):                           # 防御分支（D7）：v2 无 EventNode，
        raise _HostError("unexpected suspend")        #   挂起=图被改，别白烧重试
    if r.get("is_end"):
        CKPT.unlink(missing_ok=True)                  # 终态不落 checkpoint（R1/D2）★
        result = r["result"]
        break
    _ckpt_save(r["context"], r.get("id"))
    saved = r["context"]
finally:
    fcntl.flock(host_fd, fcntl.LOCK_UN)
```

> ★ 终态 checkpoint 是评审抓出的最深陷阱：若终态（含 failed-preserved/
> retry-later）也落盘，keeper 重派会从 End 节点"续走"→ `_get_next_from_last`
> 返回 None → 把整张 `$NODE` 表当 verdict 返回 → 契约破坏且 L1 被永久挡住
> （R1/D2，P0×2 独立发现）。终态一律删 checkpoint，交给 L1 矩阵。

## 3. G1 重试语义：v2.1 修订口径

**机制成立**（评审核实）：节点失败在 `context.update_node_result` 之前抛出
（`runner.py:205-222`）→ 失败节点无 context 条目、`last_node_id` 仍指上一个
成功节点；宿主保存的 checkpoint 即"停在最后成功节点"的合法断点；continue
经 `_get_next_from_last`（`strategies.py:268-288`）解析出**刚失败的那个节点**
原地重执行。continue 守卫（`strategies.py:252`）只拦 EventNode——v2 无。

**边界修订**（吸收 R3/T1——原稿把"业务红"错划进宿主重试）：

| 失败类型 | 例子 | 宿主重试？ |
|---|---|---|
| **异常类**（会 raise） | AGENTRUN 的 AgentRunError（CLI 崩/非零退出/被杀）、CODE python 异常、GIT_PUBLISH/WRITEFILE/preflight 异常 | **是**（G1 覆盖面） |
| **业务红**（正常返回 False） | GATE 红门（`gate.py:90` 返回结构化结果，从不 raise）、评审 NEEDS_FIX | **否**——走 flow 图内修复环/failed-preserved，与 NORMAL 时代相同 |
| **超时类** | impl 撞 7200s 墙 | **否**——确定性信号，原地重试=再烧 2h（D4）；直接 engine_error，由 keeper 层重派走 L1+L2 |
| **协议类** | ResumeError、unmatched branch（图坏了） | **否**——与节点失败共用异常出口（R7），按 above 归入不重试类（`_is_timeout_class` 同款判别扩展） |

回调生命周期（R5/D7/T3）：每次节点失败 `_raise_distributed_error` 会 fire
`on_flow_end(error)`、而续传不再 fire `on_flow_start`——宿主**在终态显式补
`on_flow_end`**，并声明重试后的 Langfuse span 语义 = 同 trace 内分段（接受
一个 ERROR 根 + 后续段，或宿主层面按重试轮次分 trace——实现时二选一，
测试断言事件序列，见 s13）。

## 4. checkpoint 设计（v2.1：位置与组合顺序是本版最大的修正）

- **位置 = keeper 工件根 `~/.issue-keeper/pipeline/recursive-<n>/checkpoint.json`**
  （O1/T4：原稿放 run_dir 是结构性不可达——keeper 重派生成新 run_id，新
  run_dir 里永远查不到旧 checkpoint；工件根按 issue 稳定、reaper 从不清理。
  与 L2 sessions 已有的先例完全同构）。**per-issue 单槽**：新派发发现
  checkpoint 即续（内嵌 run_id 仅作审计）。
- **恢复三重闸**（`_ckpt_load`）：存在 且 flow 指纹一致 且 **worktree 存在**。
- **组合顺序不变量（O2，P0）**：跨派发恢复**必须先 L1 重建 worktree
  （wip 分支），再按 checkpoint 跳节点**——终态回收会 rmtree worktree，
  只续 checkpoint 不重建 worktree = 在已删除的路径上跑门禁 = paths 条件门
  静默跳过 = 假绿落地。实现上：checkpoint 命中但 worktree 缺失 → 丢弃
  checkpoint 走完整 L1 路径（preflight 节点本就做 worktree add）。二者是
  叠加关系不是二选一。
- **flow_hash = 图结构指纹**（flow_id + 节点 id 集 + 边表；D8/开放问题 D3
  采纳），并附 `plaita_version`/`plaita_nodes_version`（D8：节点结果 schema
  漂移的假绿风险）。不一致 → 丢弃 → L1。
- **损坏鲁棒**（T6）：`_ckpt_load` 对 JSON 截断/损坏返回 None（吞异常落
  recovery-error.log），与 flow_hash 不符同轨 → L1。

**组合矩阵（v2.1 修订）**：

| 场景 | 兜底层 |
|---|---|
| 节点**异常**（AGENTRUN 崩、CODE 异常、发布异常） | v3 宿主原地重试（每节点 1 次），成功节点跳过 |
| 业务红（门禁红/评审否决） | flow 图内修复环（原有，不属宿主） |
| impl 超时/被杀（节点内损失） | keeper 重派 → L1（代码）+ L2（会话 resume）；宿主不原地重试（D4） |
| 进程崩溃/断电（图中断） | v3 checkpoint（工件根）+ worktree 在 → 续走 |
| keeper 终态回收后重派 | checkpoint 丢弃（worktree 闸）→ L1 完整路径 |
| flow/plaita 版本漂移 | flow_hash/版本闸 → 丢弃 → L1 |
| 宿主进程被 kill -9 / KeyboardInterrupt（R8） | 无收尾——靠盘上 checkpoint + keeper 重派（写明，接受） |
| 双宿主并发 | host.lock → 后到者 retry-later 退出（D6） |

## 5. 契约（keeper 侧改动收敛为一个字段 + 一个已修 bug）

- **state.json/RESULT 契约不变**；run_dir 新增 `host.lock`；
- **node_retries 留痕契约**（T7/O6 落地）：`state.json.node_retries[node_id] =
  {count, last_error}`，StepTracker 增加 `note_retry`；`currentStep` 语义补充
  `<node>#attempt2` 后缀；
- **台账 extra**：`node_retry_exhausted: true`（宿主重试耗尽）→ keeper 见标记
  跳过 engine_error 自动重派直接升级（O3 的预算墙另见 §8）；
- **keeper 已修 bug（评审 O3 发现，先行落地 929088e）**：engine_error 自动
  重试 off-by-one——台账终态行在计数前已落、决策又 +1 → 首败即升级、
  重试分支自 e2129da 起零触发（日志实证 0/24）。修复=决策直接用 trailing
  计数。**此修复是本方案叠加数学的前提**；
- **dispatch 链新增 `engine_env` 透传**（O4）：`PipelineRepoConfig.engine_env:
  dict` → payload → `v2_bridge` 的 `subprocess.run(env={**os.environ,
  **engine_env})`——照 agent/reviewer 字段先例（config.py:313 → keeper.py:1609
  → v2_bridge.py:97）。**灰度口径修订**（O4）：env 是 per-repo 旋钮，灰度=
  recursive 整仓开（日限/在途闸兜底），不做 per-issue。

## 6. Langfuse 接线（净增观测 + 生命周期归宿主）

`plaita.obs.LangfuseCallback` 加入 callback_handlers（LANGFUSE_* env 已在
bridge 注入链）。生命周期语义（R5/D7/T3）：DISTRIBUTED 下成功 End 不触发
`on_flow_end`、节点失败触发 error 版——**宿主在终态（成功或耗尽）显式补
`on_flow_end`**；重试轮次的 trace 分段策略在实现时定死并写进 s13 断言。

## 7. 测试计划（v2.1：按 T1/R3 推翻重写 s13/s14）

桩机制经核实两策略一致（`NodeRunner.run_node → Node.execute` 类级替换拦得到；
childflow 在 DISTRIBUTED 下整段 NORMAL 跑完）。修订集（~13 场景）：

- **s11 等价性**：s1+s2 两条在宿主循环下终态 dict 与 CALLS 序列一致，且
  impl 收到 goal/repo（兼验 params 首传，D1/T5）；**全部场景挂生产 handlers**
  （StepTracker + 离线 LangfuseCallback，T3）；
- **s12 崩溃恢复（升级）**：两个 bridge 子进程——第一个 kill -9 于
  checkpoint.json 出现后（N 固定选**修复环中段**：首门红+fix 完成后复检前，
  必穿 last_branch 还原，T8），第二个同参重入 → 续走 → committed；
- **s13/s14 推翻重写（T1/R3）**：桩加 `@RAISE` 哨兵（`_route_agent` 抛异常
  模拟 CLI 崩溃）——s13 = impl 抛一次 → 宿主重试 → committed，断言失败前
  节点未重复执行 + 回调事件序列；s14 = 连抛 → engine_error + checkpoint
  保留 + `node_retry_exhausted` 落台账；gate 红类失败已由 s2-s5 覆盖，勿混；
- **s15 扩为 checkpoint 不可信家族**（T6）：flow_hash 不符 / JSON 截断损坏 /
  遗留 .tmp → 同一断言（load 返回 None → 全新 L1 → committed）；
- **s16 终态后重派**（R1/D2）：committed 终态后 checkpoint 已删 → 重派走
  L1 → 不复现 `$NODE` 表 verdict；
- **s17 worktree 闸**（O2）：checkpoint 命中但 worktree 已被终态回收删除 →
  丢弃 → L1 → 不假绿；
- **新增廉价冒烟**：真 bridge `--dry-run` + `RECURSIVE_HOST_V3=1`（无桩），
  断言 verdict/checkpoint/state 契约（T5）。

## 8. 预算墙与三口时钟（O3/O5）

三口独立时钟现状：keeper `pipeline_timeout_secs`（8h，killpg 整组、timed_out
路径**直接消费无自动重试**）、v2_bridge `V2_TIMEOUT_SECS`（8h）、v3 宿主
重试预算（隐含 ≥ Σ节点预算×2）。**不变量**：`pipeline_timeout_secs ≥
Σ(节点预算)×(1+MAX_NODE_RETRIES) + 固定开销`；宿主在每次节点执行与重试前
检查**run 级 deadline**（dispatch payload 注入 epoch，engine_env 同链路），
不足该节点预算即快速失败。impl 4h + 门禁环 + 评审环逼近 8h 的组合由 deadline
显式拦截而非靠 keeper 杀进程的 timed_out 路径（那条路无重试且只有兜底评论）。

## 9. 工作量（v2.1 修订后）

| 项 | 估时 |
|---|---|
| 宿主循环（含 flock/三重闸/按节点重试/超时类判别/deadline） | ~3h |
| Langfuse 接线 + 生命周期 | ~0.5h |
| `engine_env` 透传（keeper config/keeper/v2_bridge 三点 + 测试） | ~1h |
| harness s11-s17 + 生产 handlers 接入 | ~3h |
| 三过 + canary（recursive 整仓开 env） | 1 个观察班次 |
| **合计** | **~0.8 天编码 + 1 个观察班次** |

## 10. 开放问题（v2.1 更新）

- D1 node_retries 已升级为验收项（§5 契约）——值守判读口径待班次演练；
- D2 checkpoint 是否记 impl session_id：暂缓（preflight 扫描已够用，多会话
  并存场景出现再议）；
- D3 ~~flow_hash 口径~~ → 已采纳图结构指纹 + 版本号（§4）；
- D4 console schema 平移：仍后置；
- **D5（新）WorkspaceLease**：checkpoint 恢复还原同一 execution_id → 撞死
  进程的文件租约（TTL≈2h）→ SandboxLeaseError 空转（架构评审 D5）。选型：
  (a) 恢复前按 holder `hostname:pid` 存活探测、可证死则抢占；(b) 崩溃恢复轮
  换 execution_id（Langfuse trace 断链代价）；(c) 文档写明 TTL 停滞窗口供
  值守 runbook。**倾向 (a)**，实现代价 ~20 行；
- **D6（新）keeper 自动重试修复（929088e）与 v3 宿主重试的叠加**：宿主重试
  是节点级、keeper 是派发级，二者语义正交；但 `node_retry_exhausted` 标记
  的消费行为（跳过自动重派）需在 keeper reaper 加一个 if——这打破了 §5
  「零改动」？否：该标记走台账 extra，reaper 判断是既有升级路径的前置过滤，
  实现时连同 O3 修复合入。

## 11. 评审记录

四路交叉评审（架构 / 重试语义 / 运维契约 / 测试策略），全部
APPROVE-WITH-CHANGES，共 32 条意见（D1-D8 / R1-R8 / O1-O8 / T1-T8）。
本版已吸收全部 P0×5（params 遗漏 D1/T5/O7、终态 checkpoint 陷阱 R1/D2、
checkpoint 位置不可达 O1/T4、组合顺序假绿 O2、测试空洞 T1/R3）与全部 P1；
P2 中 T6/T8/R7/R8 已并入正文，其余（含 keeper 自动重试死分支的生产修复
929088e）随实现落地。意见全文见值守会话记录。
